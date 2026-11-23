//! What the store forgets, and giving the space back.
//!
//! A sweep ([`Store::sweep`], hourly from [`run`]):
//! * forgets health readings last seen more than [`HEALTH_RETENTION`] ago;
//! * trims finished rollouts and runs older than [`PAYLOAD_RETENTION`]: their
//!   rows and summaries stay, their plans, outcomes and step events go. The
//!   newest row of each target (and workload) and its newest healthy row
//!   keep theirs, as does each target's newest run: the host page shows them;
//! * returns free pages to the file (`incremental_vacuum`) and truncates the
//!   WAL;
//! * removes schema snapshots too old to restore.

use super::health::HEALTH_RETENTION;
use super::{Store, db_error, ts};
use crate::store::runs::RUNNING;
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How long finished rollouts and runs keep their plans and outcomes.
pub const PAYLOAD_RETENTION: chrono::Duration = chrono::Duration::days(90);

/// How often [`run`] sweeps.
pub const SWEEP_EVERY: Duration = Duration::from_secs(3600);

/// What one sweep removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Swept {
    pub health_readings: usize,
    pub rollout_rows: usize,
    pub runs: usize,
    pub run_events: usize,
    pub freed_pages: i64,
    pub snapshots: usize,
}

const TERMINAL: &str = "('completed', 'rolled_back', 'aborted', 'skipped')";

/// Rows each target keeps whole: its newest row per workload, and its newest
/// healthy one (what it last converged to).
const KEPT_ROWS: &str = "SELECT rowid FROM (SELECT rowid, ROW_NUMBER() OVER (PARTITION BY host, workload ORDER BY created_at DESC, rowid DESC) AS n FROM rollout_hosts) WHERE n = 1
     UNION SELECT rowid FROM (SELECT rowid, ROW_NUMBER() OVER (PARTITION BY host, workload ORDER BY updated_at DESC, rowid DESC) AS n FROM rollout_hosts WHERE state = 'healthy') WHERE n = 1";

/// Runs each target keeps whole: its newest, and its newest apply.
const KEPT_RUNS: &str = "SELECT MAX(id) FROM runs GROUP BY host
     UNION SELECT MAX(id) FROM runs WHERE action IS NULL GROUP BY host";

/// Drops the plans of finished rollouts' rows older than `cutoff` (the
/// current one and those of earlier attempts).
fn trim_rollouts(conn: &Connection, cutoff: DateTime<Utc>) -> Result<usize> {
    conn.execute(
        &format!(
            "UPDATE rollout_hosts SET plan = NULL,
                detail = CASE WHEN json_type(detail, '$.earlier') = 'array'
                    THEN json_set(detail, '$.earlier', json((SELECT json_group_array(json_remove(value, '$.plan')) FROM json_each(detail, '$.earlier'))))
                    ELSE detail END
             WHERE rollout IN (SELECT id FROM rollouts WHERE finished_at < ?1 AND state IN {TERMINAL}
                               AND NOT COALESCE(json_extract(detail, '$.stragglers_open'), 0))
               AND (plan IS NOT NULL OR json_type(detail, '$.earlier[0].plan') IS NOT NULL)
               AND rowid NOT IN ({KEPT_ROWS})"
        ),
        params![ts(cutoff)],
    )
    .map_err(db_error)
}

/// Drops the outcomes and step events of runs finished before `cutoff`.
fn trim_runs(conn: &Connection, cutoff: DateTime<Utc>) -> Result<(usize, usize)> {
    let old = format!(
        "SELECT run_id FROM runs WHERE finished_at < ?1 AND outcome != ?2 AND id NOT IN ({KEPT_RUNS})"
    );
    let events = conn
        .execute(
            &format!("DELETE FROM run_events WHERE run_id IN ({old})"),
            params![ts(cutoff), RUNNING],
        )
        .map_err(db_error)?;
    let runs = conn
        .execute(
            &format!(
                "UPDATE runs SET outcome_json = NULL WHERE outcome_json IS NOT NULL AND run_id IN ({old})"
            ),
            params![ts(cutoff), RUNNING],
        )
        .map_err(db_error)?;
    Ok((runs, events))
}

/// Returns the free pages to the file system and truncates the WAL; how
/// many pages were freed.
fn reclaim(conn: &Connection) -> Result<i64> {
    let free: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .map_err(db_error)?;
    if free > 0 {
        // Each step frees one page: run it to the end.
        let mut stmt = conn
            .prepare("PRAGMA incremental_vacuum")
            .map_err(db_error)?;
        let mut rows = stmt.query([]).map_err(db_error)?;
        while rows.next().map_err(db_error)?.is_some() {}
    }
    // In memory there is no WAL; the pragma then reports busy = 0 rows.
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    Ok(free)
}

impl Store {
    /// Forgets what outlived its retention and returns the space; see the
    /// module docs.
    pub async fn sweep(&self, now: DateTime<Utc>) -> Result<Swept> {
        let health_readings = self
            .call(move |db| db.health().prune(now - HEALTH_RETENTION))
            .await?;
        let cutoff = now - PAYLOAD_RETENTION;
        let rollout_rows = self
            .transaction(move |db| trim_rollouts(db.conn(), cutoff))
            .await?;
        let (runs, run_events) = self
            .transaction(move |db| trim_runs(db.conn(), cutoff))
            .await?;
        let freed_pages = self.call(|db| reclaim(db.conn())).await?;
        let snapshots = match self.path.clone() {
            Some(path) => tokio::task::spawn_blocking(move || {
                super::snapshot::expire(&path, std::time::SystemTime::now())
            })
            .await
            .unwrap_or_default(),
            None => 0,
        };
        Ok(Swept {
            health_readings,
            rollout_rows,
            runs,
            run_events,
            freed_pages,
            snapshots,
        })
    }
}

/// Sweeps the store shortly after start and every [`SWEEP_EVERY`].
pub async fn run(store: Store, cancel: CancellationToken) {
    let mut every = tokio::time::interval_at(
        tokio::time::Instant::now() + Duration::from_secs(60),
        SWEEP_EVERY,
    );
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = every.tick() => {}
        }
        match store.sweep(Utc::now()).await {
            Ok(swept) if swept != Swept::default() => {
                tracing::info!(
                    health_readings = swept.health_readings,
                    rollout_rows = swept.rollout_rows,
                    runs = swept.runs,
                    run_events = swept.run_events,
                    freed_pages = swept.freed_pages,
                    snapshots = swept.snapshots,
                    "swept the store"
                )
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "the store could not be swept"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::HealthEventRecord;

    fn days_ago(now: DateTime<Utc>, days: i64) -> String {
        ts(now - chrono::Duration::days(days))
    }

    /// Rollouts, rows and runs as the controller leaves them, by raw SQL so
    /// only the columns retention reads matter.
    fn seed(conn: &Connection, now: DateTime<Utc>) {
        let old = days_ago(now, PAYLOAD_RETENTION.num_days() + 10);
        let recent = days_ago(now, 1);
        let rollout = |id: &str, finished: &str, detail: &str| {
            conn.execute(
                "INSERT INTO rollouts (id, revision, state, policy, started_at, finished_at, triggered_by, detail) VALUES (?1, 'r', 'completed', '{}', ?2, ?2, 'test', ?3)",
                params![id, finished, detail],
            )
            .unwrap();
        };
        rollout("old", &old, "{}");
        rollout("stragglers", &old, r#"{"stragglers_open": true}"#);
        rollout("new", &recent, "{}");
        let row = |rollout: &str, host: &str, state: &str, at: &str| {
            conn.execute(
                "INSERT INTO rollout_hosts (rollout, host, workload, phase, state, plan, created_at, updated_at, detail) VALUES (?1, ?2, '', 'p', ?3, '{\"big\": true}', ?4, ?4, '{\"revision\": \"r\", \"earlier\": [{\"attempt\": 1, \"digest\": \"d\", \"state\": \"healthy\", \"at\": \"2026-01-01T00:00:00Z\", \"plan\": {\"big\": true}}]}')",
                params![rollout, host, state, at],
            )
            .unwrap();
        };
        row("old", "superseded", "healthy", &old);
        row("new", "superseded", "healthy", &recent);
        row("old", "only-row", "healthy", &old);
        row("old", "last-healthy", "healthy", &old);
        row("new", "last-healthy", "failed", &recent);
        row("stragglers", "straggling", "healthy", &old);
        row("new", "straggling", "healthy", &recent);
        let run = |id: &str, host: &str, at: &str| {
            conn.execute(
                "INSERT INTO runs (host, revision, plan_digest, started_at, finished_at, outcome, run_id, outcome_json) VALUES (?1, 'r', 'd', ?3, ?3, 'success', ?2, '{\"big\": true}')",
                params![host, id, at],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO run_events (run_id, key, seq, event, at) VALUES (?1, 'k', 1, '{}', ?2)",
                params![id, at],
            )
            .unwrap();
        };
        run("run-old", "superseded", &old);
        run("run-new", "superseded", &recent);
        run("run-only", "only-row", &old);
    }

    fn reading(at: DateTime<Utc>, check: &str) -> HealthEventRecord {
        HealthEventRecord {
            host: "superseded".into(),
            rollout: None,
            scope: "drift".into(),
            module: "m".into(),
            check: check.into(),
            kind: "k".into(),
            required: true,
            status: "pass".into(),
            reason: None,
            at,
        }
    }

    #[tokio::test]
    async fn a_sweep_trims_old_payloads_but_keeps_what_targets_show() {
        let store = Store::in_memory().await.unwrap();
        let now = Utc::now();
        store
            .call(move |db| {
                seed(db.conn(), now);
                db.health().record(
                    "r",
                    &[
                        reading(now - HEALTH_RETENTION - chrono::Duration::days(1), "stale"),
                        reading(now, "fresh"),
                    ],
                )
            })
            .await
            .unwrap();

        let swept = store.sweep(now).await.unwrap();
        assert_eq!(
            (
                swept.health_readings,
                swept.rollout_rows,
                swept.runs,
                swept.run_events
            ),
            (1, 1, 1, 1)
        );

        let (rows, runs, events, checks) = store
            .call(|db| {
                let conn = db.conn();
                let rows: Vec<(String, String, bool, bool)> = conn
                    .prepare("SELECT rollout, host, plan IS NOT NULL, json_type(detail, '$.earlier[0].plan') IS NOT NULL FROM rollout_hosts ORDER BY rollout, host")
                    .map_err(db_error)?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(db_error)?;
                let runs: Vec<(String, bool)> = conn
                    .prepare("SELECT run_id, outcome_json IS NOT NULL FROM runs ORDER BY run_id")
                    .map_err(db_error)?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(db_error)?;
                let events: Vec<String> = conn
                    .prepare("SELECT run_id FROM run_events ORDER BY run_id")
                    .map_err(db_error)?
                    .query_map([], |r| r.get(0))
                    .map_err(db_error)?
                    .collect::<rusqlite::Result<_>>()
                    .map_err(db_error)?;
                let checks: Vec<String> = db
                    .health()
                    .for_host("superseded", 10)?
                    .into_iter()
                    .map(|r| r.check)
                    .collect();
                Ok((rows, runs, events, checks))
            })
            .await
            .unwrap();
        let trimmed: Vec<_> = rows
            .iter()
            .filter(|(_, _, plan, earlier)| !plan && !earlier)
            .map(|(rollout, host, ..)| format!("{rollout}/{host}"))
            .collect();
        assert_eq!(
            trimmed,
            ["old/superseded"],
            "only an old row a newer one replaced loses its plans: {rows:?}"
        );
        assert_eq!(
            runs,
            [
                ("run-new".to_string(), true),
                ("run-old".to_string(), false),
                ("run-only".to_string(), true)
            ]
        );
        assert_eq!(events, ["run-new", "run-only"]);
        assert_eq!(checks, ["fresh"]);
        // The trimmed row still reads.
        let old = store
            .call(|db| db.rollouts().host("old", "superseded", ""))
            .await
            .unwrap()
            .expect("the row is kept");
        assert!(old.plan.is_none());
        assert_eq!(
            old.runtime.earlier.len(),
            1,
            "earlier attempts are listed still"
        );
        assert_eq!(
            store.sweep(now).await.unwrap().rollout_rows,
            0,
            "idempotent"
        );
    }

    #[tokio::test]
    async fn a_sweep_returns_freed_space_and_drops_expired_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buckle.db");
        let store = Store::open(&path).await.unwrap();
        let mode: i64 = store
            .call(|db| {
                db.conn()
                    .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
                    .map_err(db_error)
            })
            .await
            .unwrap();
        assert_eq!(mode, 2, "incremental");
        let now = Utc::now();
        store
            .call(move |db| {
                let old = now - HEALTH_RETENTION - chrono::Duration::days(1);
                let readings: Vec<_> = (0..2000).map(|i| reading(old, &format!("c{i}"))).collect();
                db.health().record("r", &readings)
            })
            .await
            .unwrap();
        let stale = super::super::snapshot::snapshot_path(&path, 1);
        let fresh = super::super::snapshot::snapshot_path(&path, 2);
        for p in [&stale, &fresh] {
            std::fs::write(p, b"snapshot").unwrap();
        }
        std::fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(
                std::time::SystemTime::now()
                    - super::super::snapshot::RESTORE_WITHIN
                    - Duration::from_secs(60),
            )
            .unwrap();

        let swept = store.sweep(now).await.unwrap();
        assert_eq!(swept.health_readings, 2000);
        assert!(swept.freed_pages > 0);
        let free: i64 = store
            .call(|db| {
                db.conn()
                    .query_row("PRAGMA freelist_count", [], |r| r.get(0))
                    .map_err(db_error)
            })
            .await
            .unwrap();
        assert_eq!(free, 0, "freed pages went back to the file system");
        assert_eq!(swept.snapshots, 1);
        assert!(!stale.exists() && fresh.exists());
    }
}
