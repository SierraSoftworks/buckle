//! Apply runs per host and the last apply event per step.

use super::{db_error, parse_ts, ts};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

/// One row of `runs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub host: String,
    pub rollout: Option<String>,
    pub revision: String,
    pub plan_digest: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    /// `succeeded`, `failed` or `stale_plan`.
    pub outcome: String,
    pub changes: u32,
    pub removals: u32,
    pub error: Option<String>,
    pub trace_id: Option<String>,
}

/// Repository over `runs` and `apply_events`.
pub struct Runs<'c> {
    conn: &'c Connection,
}

impl<'c> Runs<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Records a finished run.
    pub fn add(&self, run: &RunRecord) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO runs (host, rollout, revision, plan_digest, started_at, finished_at, outcome, changes, removals, error, trace_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    run.host,
                    run.rollout,
                    run.revision,
                    run.plan_digest,
                    ts(run.started_at),
                    ts(run.finished_at),
                    run.outcome,
                    run.changes,
                    run.removals,
                    run.error,
                    run.trace_id
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// The most recent runs for a host, newest first.
    pub fn for_host(&self, host: &str, limit: u32) -> Result<Vec<RunRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT host, rollout, revision, plan_digest, started_at, finished_at, outcome, changes, removals, error, trace_id FROM runs WHERE host = ?1 ORDER BY finished_at DESC, id DESC LIMIT ?2")
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![host, limit], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, u32>(7)?,
                    r.get::<_, u32>(8)?,
                    r.get::<_, Option<String>>(9)?,
                    r.get::<_, Option<String>>(10)?,
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
                    revision,
                    plan_digest,
                    started,
                    finished,
                    outcome,
                    changes,
                    removals,
                    error,
                    trace_id,
                )| {
                    Ok(RunRecord {
                        host,
                        rollout,
                        revision,
                        plan_digest,
                        started_at: parse_ts(&started)?,
                        finished_at: parse_ts(&finished)?,
                        outcome,
                        changes,
                        removals,
                        error,
                        trace_id,
                    })
                },
            )
            .collect()
    }

    /// Stores the latest event for one step (coalescing the stream).
    pub fn record_event(
        &self,
        host: &str,
        plan_digest: &str,
        step: &str,
        event: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO apply_events (host, plan_digest, step, event, at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(host, plan_digest, step) DO UPDATE SET event = excluded.event, at = excluded.at",
                params![host, plan_digest, step, event, ts(at)],
            )
            .map_err(db_error)?;
        Ok(())
    }
}
