//! Small key/value state: the last fetched commit, the stable revision, the
//! server install key.

use super::db_error;
use buckle_core::Result;
use rusqlite::{Connection, OptionalExtension, params};

/// Well-known keys.
pub mod keys {
    /// The fleet's stable revision id.
    pub const STABLE_REVISION: &str = "stable_revision";
    /// The last commit the source watcher processed.
    pub const LAST_COMMIT: &str = "source.last_commit";
    /// Hex of the 32-byte server install key (fingerprints, approval tokens).
    pub const INSTALL_KEY: &str = "install_key";
    /// Prefix of durable agent reports stored by the gRPC layer that the
    /// rollout controller has not yet persisted its reaction to (replayed at
    /// start-up); `<prefix><host>/<report id>`.
    pub const PENDING_REPORT_PREFIX: &str = "controller.pending_report.";
    /// Prefix of each cluster's last executor (the host agent speaking for
    /// it), `<prefix><cluster>`; a pick stays with it while it qualifies.
    pub const EXECUTOR_PREFIX: &str = "executor/";
}

/// Repository over the `kv` table.
pub struct Kv<'c> {
    conn: &'c Connection,
}

impl<'c> Kv<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// The value for `key`, if set.
    pub fn get(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
            .map_err(db_error)
    }

    /// Sets `key` to `value`.
    pub fn set(&self, key: &str, value: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Every `(key, value)` whose key starts with `prefix`, in key order.
    pub fn with_prefix(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let mut stmt = self
            .conn
            .prepare("SELECT key, value FROM kv WHERE key LIKE ?1 ESCAPE '\\' ORDER BY key")
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![format!("{escaped}%")], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        Ok(rows)
    }

    /// Removes `key`.
    pub fn delete(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM kv WHERE key = ?1", params![key])
            .map_err(db_error)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::store::Store;

    #[tokio::test]
    async fn with_prefix_lists_only_matching_keys_literally() {
        let store = Store::in_memory().await.unwrap();
        let rows = store
            .call(|db| {
                db.kv().set("a.b_1", "1")?;
                db.kv().set("a.b_2", "2")?;
                db.kv().set("a.bx3", "3")?;
                db.kv().set("z", "4")?;
                db.kv().with_prefix("a.b_")
            })
            .await
            .unwrap();
        assert_eq!(
            rows,
            vec![("a.b_1".into(), "1".into()), ("a.b_2".into(), "2".into())]
        );
    }
}
