//! Durable agent reports (ApplyResult, LocalRollback) already stored, by
//! `(host, report_id)`. Agents keep such a report until the server
//! acknowledges it and re-send it after every Welcome, so the same report can
//! arrive more than once; only its first arrival has effects.

use super::{db_error, ts};
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

/// Repository over `agent_reports`.
pub struct AgentReports<'c> {
    conn: &'c Connection,
}

impl<'c> AgentReports<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Records the arrival of a report; `false` when it was already stored.
    pub fn first_arrival(
        &self,
        host: &str,
        report_id: u64,
        kind: &str,
        at: DateTime<Utc>,
    ) -> Result<bool> {
        let n = self
            .conn
            .execute(
                "INSERT OR IGNORE INTO agent_reports (host, report_id, kind, at) VALUES (?1, ?2, ?3, ?4)",
                params![host, report_id as i64, kind, ts(at)],
            )
            .map_err(db_error)?;
        Ok(n == 1)
    }

    /// Forgets a host's reports (the host was forgotten).
    pub fn forget_host(&self, host: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM agent_reports WHERE host = ?1", params![host])
            .map_err(db_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    #[tokio::test]
    async fn a_report_is_stored_once_per_host() {
        let store = Store::in_memory().await.unwrap();
        let now = chrono::Utc::now();
        let seen = store
            .call(move |db| {
                Ok((
                    db.reports().first_arrival("h1", 7, "apply_result", now)?,
                    db.reports().first_arrival("h1", 7, "apply_result", now)?,
                    db.reports().first_arrival("h2", 7, "apply_result", now)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(seen, (true, false, true));
        store
            .call(|db| db.reports().forget_host("h1"))
            .await
            .unwrap();
        let again = store
            .call(move |db| db.reports().first_arrival("h1", 7, "apply_result", now))
            .await
            .unwrap();
        assert!(again);
    }
}
