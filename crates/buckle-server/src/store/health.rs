//! Health check results, kept for 30 days, and each check's status per
//! epoch (the rollout and revision applied when it ran), kept for the host's
//! [`HEALTH_EPOCHS`] most recent epochs.

use super::{db_error, parse_ts, ts};
use buckle_api::{CheckEpoch, HostCheck};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

/// How many epochs of check status a host keeps.
pub const HEALTH_EPOCHS: u32 = 10;

const COLUMNS: &str =
    "host, rollout, scope, module, check_name, kind, required, status, reason, at";

/// One row of `health_events`.
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

/// Repository over `health_events`.
pub struct HealthEvents<'c> {
    conn: &'c Connection,
}

impl<'c> HealthEvents<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Records results taken while `revision` was applied, folds them into
    /// each check's status for its epoch, and prunes rows older than 30 days
    /// and epochs beyond the host's [`HEALTH_EPOCHS`] most recent.
    pub fn record(&self, revision: &str, events: &[HealthEventRecord]) -> Result<()> {
        for e in events {
            self.conn
                .execute(
                    "INSERT INTO health_events (host, rollout, scope, module, check_name, kind, required, status, reason, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![e.host, e.rollout, e.scope, e.module, e.check, e.kind, e.required, e.status, e.reason, ts(e.at)],
                )
                .map_err(db_error)?;
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
        if let Some(latest) = events.iter().map(|e| e.at).max() {
            let cutoff = latest - chrono::Duration::days(30);
            self.conn
                .execute(
                    "DELETE FROM health_events WHERE at < ?1",
                    params![ts(cutoff)],
                )
                .map_err(db_error)?;
        }
        Ok(())
    }

    /// The most recent results for a host, newest first.
    pub fn for_host(&self, host: &str, limit: u32) -> Result<Vec<HealthEventRecord>> {
        self.query(
            &format!("SELECT {COLUMNS} FROM health_events WHERE host = ?1 ORDER BY at DESC, id DESC LIMIT ?2"),
            params![host, limit],
        )
    }

    /// A host's results from `from` until `until` (open-ended when `None`),
    /// oldest first, at most the newest `limit` of them.
    pub fn between(
        &self,
        host: &str,
        from: DateTime<Utc>,
        until: Option<DateTime<Utc>>,
        limit: u32,
    ) -> Result<Vec<HealthEventRecord>> {
        let mut rows = self.query(
            &format!("SELECT {COLUMNS} FROM health_events WHERE host = ?1 AND at >= ?2 AND (?3 IS NULL OR at < ?3) ORDER BY at DESC, id DESC LIMIT ?4"),
            params![host, ts(from), until.map(ts), limit],
        )?;
        rows.reverse();
        Ok(rows)
    }

    fn query(&self, sql: &str, args: impl rusqlite::Params) -> Result<Vec<HealthEventRecord>> {
        let mut stmt = self.conn.prepare(sql).map_err(db_error)?;
        let rows = stmt
            .query_map(args, |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, bool>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, String>(9)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .map(
                |(host, rollout, scope, module, check, kind, required, status, reason, at)| {
                    Ok(HealthEventRecord {
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
