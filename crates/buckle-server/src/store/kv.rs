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

    /// Removes `key`.
    pub fn delete(&self, key: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM kv WHERE key = ?1", params![key])
            .map_err(db_error)?;
        Ok(())
    }
}
