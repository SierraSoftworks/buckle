//! Health check results, kept for 30 days.

use super::{db_error, parse_ts, ts};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

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

    /// Records results and prunes rows older than 30 days.
    pub fn record(&self, events: &[HealthEventRecord]) -> Result<()> {
        for e in events {
            self.conn
                .execute(
                    "INSERT INTO health_events (host, rollout, scope, module, check_name, kind, required, status, reason, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![e.host, e.rollout, e.scope, e.module, e.check, e.kind, e.required, e.status, e.reason, ts(e.at)],
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
        let mut stmt = self
            .conn
            .prepare("SELECT host, rollout, scope, module, check_name, kind, required, status, reason, at FROM health_events WHERE host = ?1 ORDER BY at DESC, id DESC LIMIT ?2")
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
}
