//! Phase approvals, each binding a set of plan digests.

use super::{db_error, from_json, parse_ts, to_json, ts};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

/// One row of `approvals`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub id: i64,
    pub rollout: String,
    pub phase: String,
    /// The OIDC subject (or `anonymous` without OIDC).
    pub approver: String,
    pub plan_digests: Vec<String>,
    pub note: String,
    pub at: DateTime<Utc>,
}

/// Repository over `approvals`.
pub struct Approvals<'c> {
    conn: &'c Connection,
}

impl<'c> Approvals<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Records an approval and returns its id.
    pub fn add(
        &self,
        rollout: &str,
        phase: &str,
        approver: &str,
        digests: &[String],
        note: &str,
        at: DateTime<Utc>,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO approvals (rollout, phase, approver, plan_digests, note, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![rollout, phase, approver, to_json(&digests)?, note, ts(at)],
            )
            .map_err(db_error)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Every approval of a rollout, oldest first.
    pub fn for_rollout(&self, rollout: &str) -> Result<Vec<ApprovalRecord>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, rollout, phase, approver, plan_digests, note, at FROM approvals WHERE rollout = ?1 ORDER BY id")
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![rollout], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .map(|(id, rollout, phase, approver, digests, note, at)| {
                Ok(ApprovalRecord {
                    id,
                    rollout,
                    phase,
                    approver,
                    plan_digests: from_json(&digests)?,
                    note,
                    at: parse_ts(&at)?,
                })
            })
            .collect()
    }
}
