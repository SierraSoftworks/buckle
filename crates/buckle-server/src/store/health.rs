//! Health check results, kept for [`HEALTH_RETENTION`], and each check's
//! status per epoch (the rollout and revision applied when it ran), kept for
//! the host's [`HEALTH_EPOCHS`] most recent epochs.
//!
//! Readings are stored run-length encoded: a reading identical to its
//! check's previous one (same status, reason, scope, rollout) extends that
//! row (`last_at`, `readings`) rather than adding one. Clusters read every
//! workload's checks each minute, almost always unchanged, so this keeps
//! the table to the transitions. Readers get readings back: each row is its
//! first reading and, when it repeated, its last.

use super::{db_error, parse_ts, ts};
use buckle_api::{CheckEpoch, HostCheck};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

/// How many epochs of check status a host keeps.
pub const HEALTH_EPOCHS: u32 = 10;

/// How long a reading is kept after it was last seen.
pub const HEALTH_RETENTION: chrono::Duration = chrono::Duration::days(30);

const COLUMNS: &str = "host, rollout, scope, module, check_name, kind, required, status, reason, at, last_at, readings";

/// One reading of one check (a row of `health_events` is a run of them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthEventRecord {
    pub host: String,
    pub rollout: Option<String>,
    /// `baseline`, `bake`, `rollout`, `drift` or `manual`.
    pub scope: String,
    pub module: String,
    pub check: String,
    pub kind: String,
    pub required: bool,
    /// `pass`, `fail` or `unknown`.
    pub status: String,
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

/// A stored run of identical readings.
struct Span {
    reading: HealthEventRecord,
    last_at: DateTime<Utc>,
    readings: u32,
}

impl Span {
    /// The run's first reading and, when it repeated, its last, both
    /// clamped to `[from, until)`.
    fn readings(
        self,
        from: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Vec<HealthEventRecord> {
        let first = from.map_or(self.reading.at, |f| self.reading.at.max(f));
        let last = until.map_or(self.last_at, |u| {
            self.last_at.min(u - chrono::Duration::microseconds(1))
        });
        let mut out = vec![HealthEventRecord {
            at: first,
            ..self.reading.clone()
        }];
        if self.readings > 1 && last > first {
            out.push(HealthEventRecord {
                at: last,
                ..self.reading
            });
        }
        out
    }
}

/// Repository over `health_events`.
pub struct HealthEvents<'c> {
    conn: &'c Connection,
}

impl<'c> HealthEvents<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Records results taken while `revision` was applied (a repeat of a
    /// check's previous reading extends it), folds them into each check's
    /// status for its epoch, and prunes epochs beyond the host's
    /// [`HEALTH_EPOCHS`] most recent. Old readings go in [`Self::prune`].
    pub fn record(&self, revision: &str, events: &[HealthEventRecord]) -> Result<()> {
        for e in events {
            let extended = self
                .conn
                .execute(
                    "UPDATE health_events SET last_at = MAX(last_at, ?10), readings = readings + 1
                     WHERE id = (SELECT id FROM health_events WHERE host = ?1 AND module = ?4 AND check_name = ?5 ORDER BY id DESC LIMIT 1)
                       AND rollout IS ?2 AND scope = ?3 AND kind = ?6 AND required = ?7 AND status = ?8 AND reason IS ?9",
                    params![e.host, e.rollout, e.scope, e.module, e.check, e.kind, e.required, e.status, e.reason, ts(e.at)],
                )
                .map_err(db_error)?;
            if extended == 0 {
                self.conn
                    .execute(
                        "INSERT INTO health_events (host, rollout, scope, module, check_name, kind, required, status, reason, at, last_at, readings) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, 1)",
                        params![e.host, e.rollout, e.scope, e.module, e.check, e.kind, e.required, e.status, e.reason, ts(e.at)],
                    )
                    .map_err(db_error)?;
            }
            self.conn
                .execute(
                    "INSERT INTO health_status (host, rollout, revision, module, check_name, kind, required, scope, status, reason, worst, readings, first_at, last_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?9, 1, ?11, ?11)
                     ON CONFLICT (host, rollout, revision, module, check_name) DO UPDATE SET
                        kind = excluded.kind, required = excluded.required, scope = excluded.scope,
                        status = excluded.status, reason = excluded.reason,
                        worst = CASE WHEN (CASE excluded.status WHEN 'fail' THEN 2 WHEN 'unknown' THEN 1 ELSE 0 END)
                                        > (CASE worst WHEN 'fail' THEN 2 WHEN 'unknown' THEN 1 ELSE 0 END)
                                     THEN excluded.status ELSE worst END,
                        readings = readings + 1, last_at = excluded.last_at",
                    params![e.host, e.rollout.as_deref().unwrap_or(""), revision, e.module, e.check, e.kind, e.required, e.scope, e.status, e.reason, ts(e.at)],
                )
                .map_err(db_error)?;
        }
        let hosts: std::collections::BTreeSet<&str> =
            events.iter().map(|e| e.host.as_str()).collect();
        for host in hosts {
            self.conn
                .execute(
                    "DELETE FROM health_status WHERE host = ?1 AND (rollout, revision) NOT IN (
                        SELECT rollout, revision FROM health_status WHERE host = ?1
                        GROUP BY rollout, revision ORDER BY MAX(last_at) DESC LIMIT ?2)",
                    params![host, HEALTH_EPOCHS],
                )
                .map_err(db_error)?;
        }
        Ok(())
    }

    /// Forgets readings last seen before `cutoff`; how many runs went.
    pub fn prune(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        self.conn
            .execute(
                "DELETE FROM health_events WHERE last_at < ?1",
                params![ts(cutoff)],
            )
            .map_err(db_error)
    }

    /// The most recent results for a host, newest first.
    pub fn for_host(&self, host: &str, limit: u32) -> Result<Vec<HealthEventRecord>> {
        let mut rows: Vec<HealthEventRecord> = self
            .spans(
                &format!("SELECT {COLUMNS} FROM health_events WHERE host = ?1 ORDER BY last_at DESC, id DESC LIMIT ?2"),
                params![host, limit],
            )?
            .into_iter()
            .flat_map(|s| s.readings(None, None))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.at));
        rows.truncate(limit as usize);
        Ok(rows)
    }

    /// A host's results from `from` until `until` (open-ended when `None`),
    /// oldest first, at most the newest `limit` of them. A run of readings
    /// that began before `from` reads as a reading at `from`.
    pub fn between(
        &self,
        host: &str,
        from: DateTime<Utc>,
        until: Option<DateTime<Utc>>,
        limit: u32,
    ) -> Result<Vec<HealthEventRecord>> {
        let mut rows: Vec<HealthEventRecord> = self
            .spans(
                &format!("SELECT {COLUMNS} FROM health_events WHERE host = ?1 AND last_at >= ?2 AND (?3 IS NULL OR at < ?3) ORDER BY last_at DESC, id DESC LIMIT ?4"),
                params![host, ts(from), until.map(ts), limit],
            )?
            .into_iter()
            .flat_map(|s| s.readings(Some(from), until))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.at));
        rows.truncate(limit as usize);
        rows.reverse();
        Ok(rows)
    }

    fn spans(&self, sql: &str, args: impl rusqlite::Params) -> Result<Vec<Span>> {
        type Row = (
            String,
            Option<String>,
            String,
            String,
            String,
            String,
            bool,
            String,
            Option<String>,
            String,
            String,
            u32,
        );
        let mut stmt = self.conn.prepare(sql).map_err(db_error)?;
        let rows = stmt
            .query_map(args, |r| {
                Ok::<Row, _>((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .map(
                |(
                    host,
                    rollout,
                    scope,
                    module,
                    check,
                    kind,
                    required,
                    status,
                    reason,
                    at,
                    last_at,
                    readings,
                )| {
                    Ok(Span {
                        reading: HealthEventRecord {
                            host,
                            rollout,
                            scope,
                            module,
                            check,
                            kind,
                            required,
                            status,
                            reason,
                            at: parse_ts(&at)?,
                        },
                        last_at: parse_ts(&last_at)?,
                        readings,
                    })
                },
            )
            .collect()
    }

    /// Each check of a host, its latest reading first and its epochs newest
    /// first, ordered by module and check.
    pub fn checks(&self, host: &str) -> Result<Vec<HostCheck>> {
        let mut stmt = self
            .conn
            .prepare("SELECT rollout, revision, module, check_name, kind, required, scope, status, reason, worst, readings, first_at, last_at FROM health_status WHERE host = ?1 ORDER BY last_at DESC")
            .map_err(db_error)?;
        type Row = (
            String,
            String,
            String,
            String,
            String,
            bool,
            String,
            String,
            Option<String>,
            String,
            u32,
            String,
            String,
        );
        let rows = stmt
            .query_map(params![host], |r| {
                Ok::<Row, _>((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                    r.get(12)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let mut checks: BTreeMap<(String, String), HostCheck> = BTreeMap::new();
        for (
            rollout,
            revision,
            module,
            check,
            kind,
            required,
            scope,
            status,
            reason,
            worst,
            readings,
            first_at,
            last_at,
        ) in rows
        {
            let epoch = CheckEpoch {
                rollout_id: (!rollout.is_empty()).then_some(rollout),
                revision,
                status: status.clone(),
                worst,
                reason: reason.clone(),
                readings,
                first_at: parse_ts(&first_at)?,
                last_at: parse_ts(&last_at)?,
            };
            // Rows come newest first: the first one per check is its latest.
            checks
                .entry((module.clone(), check.clone()))
                .or_insert_with(|| HostCheck {
                    module,
                    check,
                    kind,
                    required,
                    status,
                    reason,
                    scope,
                    at: epoch.last_at,
                    history: Vec::new(),
                })
                .history
                .push(epoch);
        }
        Ok(checks.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewHost, Store};
    use buckle_api::HostStatus;
    use chrono::Duration;

    fn reading(
        rollout: Option<&str>,
        check: &str,
        status: &str,
        at: DateTime<Utc>,
    ) -> HealthEventRecord {
        HealthEventRecord {
            host: "pi-1".into(),
            rollout: rollout.map(str::to_string),
            scope: "drift".into(),
            module: "web".into(),
            check: check.into(),
            kind: "http".into(),
            required: true,
            status: status.into(),
            reason: (status != "pass").then(|| format!("{check} is {status}")),
            at,
        }
    }

    async fn store_with_host() -> Store {
        let store = Store::in_memory().await.unwrap();
        store
            .call(|db| {
                db.hosts().create(&NewHost {
                    id: "pi-1".into(),
                    status: HostStatus::Active,
                    token_hash: None,
                    auth_method: "token".into(),
                    at: Utc::now(),
                })?;
                Ok(())
            })
            .await
            .unwrap();
        store
    }

    async fn record(store: &Store, revision: &str, events: Vec<HealthEventRecord>) {
        let revision = revision.to_owned();
        store
            .call(move |db| db.health().record(&revision, &events))
            .await
            .unwrap();
    }

    async fn checks(store: &Store) -> Vec<HostCheck> {
        store.call(|db| db.health().checks("pi-1")).await.unwrap()
    }

    #[tokio::test]
    async fn checks_carry_their_latest_reading_and_worst_per_epoch() {
        let store = store_with_host().await;
        let t0 = Utc::now() - Duration::hours(3);
        for (i, status) in ["pass", "fail", "pass"].into_iter().enumerate() {
            let at = t0 + Duration::minutes(i as i64);
            record(&store, "r1", vec![reading(Some("ro-1"), "api", status, at)]).await;
        }
        let later = t0 + Duration::hours(1);
        record(
            &store,
            "r2",
            vec![reading(Some("ro-2"), "api", "unknown", later)],
        )
        .await;

        let checks = checks(&store).await;
        assert_eq!(checks.len(), 1);
        let api = &checks[0];
        assert_eq!(
            (api.status.as_str(), api.at.timestamp_micros()),
            ("unknown", later.timestamp_micros()),
            "the newest reading is current"
        );
        let epochs: Vec<_> = api
            .history
            .iter()
            .map(|e| {
                (
                    e.rollout_id.as_deref(),
                    e.status.as_str(),
                    e.worst.as_str(),
                    e.readings,
                )
            })
            .collect();
        assert_eq!(
            epochs,
            vec![
                (Some("ro-2"), "unknown", "unknown", 1),
                (Some("ro-1"), "pass", "fail", 3)
            ],
            "newest epoch first, each with its last and worst reading"
        );
    }

    #[tokio::test]
    async fn applies_outside_a_rollout_are_epochs_by_revision() {
        let store = store_with_host().await;
        let t0 = Utc::now() - Duration::hours(1);
        record(&store, "r1", vec![reading(None, "api", "pass", t0)]).await;
        record(
            &store,
            "r2",
            vec![reading(None, "api", "pass", t0 + Duration::minutes(1))],
        )
        .await;
        let history = &checks(&store).await[0].history;
        assert_eq!(
            history
                .iter()
                .map(|e| (e.rollout_id.clone(), e.revision.as_str()))
                .collect::<Vec<_>>(),
            vec![(None, "r2"), (None, "r1")]
        );
    }

    #[tokio::test]
    async fn only_recent_epochs_are_kept() {
        let store = store_with_host().await;
        let t0 = Utc::now() - Duration::hours(24);
        let total = HEALTH_EPOCHS + 3;
        for i in 0..total {
            let at = t0 + Duration::minutes(i as i64);
            let rollout = format!("ro-{i}");
            record(
                &store,
                &format!("r{i}"),
                vec![reading(Some(&rollout), "api", "pass", at)],
            )
            .await;
        }
        let history = &checks(&store).await[0].history;
        assert_eq!(history.len() as u32, HEALTH_EPOCHS);
        assert_eq!(
            history[0].rollout_id.as_deref(),
            Some(format!("ro-{}", total - 1).as_str())
        );
    }

    #[tokio::test]
    async fn a_check_missing_from_the_latest_epoch_keeps_its_last_reading() {
        let store = store_with_host().await;
        let t0 = Utc::now() - Duration::hours(1);
        record(
            &store,
            "r1",
            vec![
                reading(Some("ro-1"), "api", "fail", t0),
                reading(Some("ro-1"), "metrics", "pass", t0),
            ],
        )
        .await;
        record(
            &store,
            "r2",
            vec![reading(
                Some("ro-2"),
                "metrics",
                "pass",
                t0 + Duration::minutes(5),
            )],
        )
        .await;
        let checks = checks(&store).await;
        let api = checks
            .iter()
            .find(|c| c.check == "api")
            .expect("api is still listed");
        assert_eq!((api.status.as_str(), api.history.len()), ("fail", 1));
    }

    #[tokio::test]
    async fn forgetting_a_host_forgets_its_health() {
        let store = store_with_host().await;
        record(
            &store,
            "r1",
            vec![reading(Some("ro-1"), "api", "pass", Utc::now())],
        )
        .await;
        store
            .call(|db| db.hosts().delete("pi-1").map(|_| ()))
            .await
            .unwrap();
        assert!(checks(&store).await.is_empty());
        let events = store
            .call(|db| db.health().for_host("pi-1", 10))
            .await
            .unwrap();
        assert!(events.is_empty());
    }

    async fn stored_runs(store: &Store) -> Vec<(String, u32)> {
        store
            .call(|db| {
                let mut stmt = db
                    .conn()
                    .prepare("SELECT status, readings FROM health_events ORDER BY id")
                    .map_err(db_error)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?;
                Ok(rows)
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn repeated_readings_extend_one_row() {
        let store = store_with_host().await;
        let t0 = DateTime::from_timestamp(Utc::now().timestamp() - 3600, 0).unwrap();
        for (i, status) in ["pass", "pass", "pass", "fail", "fail", "pass"]
            .into_iter()
            .enumerate()
        {
            let at = t0 + Duration::minutes(i as i64);
            record(&store, "r1", vec![reading(None, "api", status, at)]).await;
        }
        assert_eq!(
            stored_runs(&store).await,
            [("pass".into(), 3), ("fail".into(), 2), ("pass".into(), 1)]
        );
        let readings = store
            .call(|db| db.health().for_host("pi-1", 10))
            .await
            .unwrap();
        assert_eq!(
            readings
                .iter()
                .map(|r| ((r.at - t0).num_minutes(), r.status.as_str()))
                .collect::<Vec<_>>(),
            [
                (5, "pass"),
                (4, "fail"),
                (3, "fail"),
                (2, "pass"),
                (0, "pass")
            ],
            "each run reads as its first and last reading, newest first"
        );
    }

    #[tokio::test]
    async fn a_run_spanning_the_window_reads_at_its_edges() {
        let store = store_with_host().await;
        let t0 = Utc::now() - Duration::hours(1);
        for i in 0..10 {
            record(
                &store,
                "r1",
                vec![reading(None, "api", "pass", t0 + Duration::minutes(i))],
            )
            .await;
        }
        let (from, until) = (t0 + Duration::minutes(3), t0 + Duration::minutes(6));
        let window = store
            .call(move |db| db.health().between("pi-1", from, Some(until), 10))
            .await
            .unwrap();
        assert_eq!(window.len(), 2);
        assert_eq!(window[0].at, from);
        assert!(window[1].at < until && window[1].at > from);
    }

    #[tokio::test]
    async fn the_migration_folds_existing_readings_into_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buckle.db");
        let before = &crate::store::MIGRATIONS[..18];
        let store = Store::open_with(&path, before).await.unwrap();
        store
            .call(|db| {
                db.conn()
                    .execute_batch(
                        "INSERT INTO health_events (host, scope, module, check_name, status, at) VALUES
                            ('pi-1', 'drift', 'web', 'api', 'pass', '2026-01-01T00:00:00Z'),
                            ('pi-1', 'drift', 'web', 'db', 'pass', '2026-01-01T00:00:00Z'),
                            ('pi-1', 'drift', 'web', 'api', 'pass', '2026-01-01T00:01:00Z'),
                            ('pi-1', 'drift', 'web', 'api', 'fail', '2026-01-01T00:02:00Z'),
                            ('pi-1', 'drift', 'web', 'api', 'pass', '2026-01-01T00:03:00Z'),
                            ('pi-1', 'drift', 'web', 'api', 'pass', '2026-01-01T00:04:00Z');
                         INSERT INTO host_facts (host_id, facts, collected_at) VALUES
                            ('pi-1', '{\"n\": 1}', '2026-01-01T00:00:00Z'),
                            ('pi-1', '{\"n\": 2}', '2026-01-01T00:10:00Z');",
                    )
                    .map_err(db_error)
            })
            .await
            .unwrap();
        drop(store);
        let store = Store::open(&path).await.unwrap();
        let runs: Vec<(String, String, u32, String, String)> = store
            .call(|db| {
                let mut stmt = db
                    .conn()
                    .prepare("SELECT check_name, status, readings, at, last_at FROM health_events ORDER BY id")
                    .map_err(db_error)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .map_err(db_error)?;
                Ok(rows)
            })
            .await
            .unwrap();
        let t = |m: u32| format!("2026-01-01T00:0{m}:00Z");
        assert_eq!(
            runs,
            [
                ("api".into(), "pass".into(), 2, t(0), t(1)),
                ("db".into(), "pass".into(), 1, t(0), t(0)),
                ("api".into(), "fail".into(), 1, t(2), t(2)),
                ("api".into(), "pass".into(), 2, t(3), t(4)),
            ]
        );
        let facts = store
            .call(|db| db.hosts().latest_facts("pi-1"))
            .await
            .unwrap()
            .map(|(f, _)| f);
        assert_eq!(facts, Some(serde_json::json!({"n": 2})));
    }

    #[tokio::test]
    async fn between_returns_the_window_oldest_first() {
        let store = store_with_host().await;
        let t0 = Utc::now() - Duration::hours(1);
        let events = (0..5)
            .map(|i| reading(None, &format!("c{i}"), "pass", t0 + Duration::minutes(i)))
            .collect();
        record(&store, "r1", events).await;
        let (from, until) = (t0 + Duration::minutes(1), Some(t0 + Duration::minutes(4)));
        let window = store
            .call(move |db| db.health().between("pi-1", from, until, 10))
            .await
            .unwrap();
        assert_eq!(
            window.iter().map(|e| e.check.as_str()).collect::<Vec<_>>(),
            ["c1", "c2", "c3"]
        );
        let capped = store
            .call(move |db| db.health().between("pi-1", t0, None, 2))
            .await
            .unwrap();
        assert_eq!(
            capped.iter().map(|e| e.check.as_str()).collect::<Vec<_>>(),
            ["c3", "c4"],
            "the newest within the cap"
        );
    }
}
