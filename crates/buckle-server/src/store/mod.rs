//! Persistence: one SQLite database behind a single connection.
//!
//! All access goes through [`Store::call`], which runs a closure against the
//! connection on tokio's blocking pool so the async runtime never waits on
//! disk. The closure receives a [`Db`] from which the per-aggregate
//! repositories ([`Db::hosts`], [`Db::revisions`], …) are borrowed; a
//! [`Store::transaction`] wraps several repository calls atomically.

mod approvals;
mod audit;
mod health;
mod hosts;
pub mod kv;
mod reports;
mod revisions;
pub mod rollouts;
mod runs;

pub use approvals::{ApprovalRecord, Approvals};
pub use audit::{Audit, AuditFilter, NewAudit};
pub use health::{HealthEventRecord, HealthEvents};
pub(crate) use hosts::status_str as hosts_status_str;
pub use hosts::{HostRecord, Hosts, NewHost};
pub use kv::Kv;
pub use reports::AgentReports;
pub use revisions::{RevisionRecord, Revisions};
pub use rollouts::{RolloutHostRecord, RolloutRecord, Rollouts};
pub use runs::{RunRecord, Runs};

use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Schema migrations, applied in order and recorded in `migrations`.
/// Never edit an entry once released; append a new one instead.
pub const MIGRATIONS: &[&str] = &[
    // 1: the initial schema (design §09).
    r#"
CREATE TABLE hosts (
    host_id TEXT PRIMARY KEY NOT NULL,
    agent_labels TEXT NOT NULL DEFAULT '{}',
    labels TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'pending',
    notes TEXT NOT NULL DEFAULT '',
    token_hash TEXT,
    platform TEXT,
    arch TEXT,
    agent_version TEXT,
    applied_revision TEXT,
    last_plan_digest TEXT,
    connection TEXT NOT NULL DEFAULT 'offline',
    auth_method TEXT,
    first_seen TEXT NOT NULL,
    last_seen TEXT,
    health TEXT NOT NULL DEFAULT 'unknown',
    health_at TEXT,
    drifted INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX hosts_token_hash ON hosts(token_hash) WHERE token_hash IS NOT NULL;

CREATE TABLE host_facts (
    host_id TEXT NOT NULL,
    facts TEXT NOT NULL,
    collected_at TEXT NOT NULL
);
CREATE INDEX host_facts_host ON host_facts(host_id, collected_at);

CREATE TABLE revisions (
    id TEXT PRIMARY KEY NOT NULL,
    commit_id TEXT NOT NULL,
    author TEXT NOT NULL DEFAULT '',
    message TEXT NOT NULL DEFAULT '',
    committed_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    tarball_digest TEXT,
    status TEXT NOT NULL,
    errors TEXT NOT NULL DEFAULT '[]',
    module_count INTEGER NOT NULL DEFAULT 0,
    source TEXT NOT NULL DEFAULT 'git'
);
CREATE INDEX revisions_created ON revisions(created_at);

CREATE TABLE revision_blobs (
    digest TEXT NOT NULL,
    revision TEXT NOT NULL,
    path TEXT NOT NULL,
    PRIMARY KEY (digest, revision)
);

CREATE TABLE rollouts (
    id TEXT PRIMARY KEY NOT NULL,
    revision TEXT NOT NULL,
    state TEXT NOT NULL,
    policy TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    triggered_by TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX rollouts_started ON rollouts(started_at);

CREATE TABLE rollout_hosts (
    rollout TEXT NOT NULL,
    host TEXT NOT NULL,
    phase TEXT NOT NULL,
    wave INTEGER,
    state TEXT NOT NULL,
    plan_digest TEXT,
    plan TEXT,
    result TEXT,
    health TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (rollout, host)
);

CREATE TABLE approvals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rollout TEXT NOT NULL,
    phase TEXT NOT NULL,
    approver TEXT NOT NULL,
    plan_digests TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    at TEXT NOT NULL
);
CREATE INDEX approvals_rollout ON approvals(rollout, phase);

CREATE TABLE health_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    host TEXT NOT NULL,
    rollout TEXT,
    scope TEXT NOT NULL,
    module TEXT NOT NULL,
    check_name TEXT NOT NULL,
    kind TEXT NOT NULL DEFAULT '',
    required INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL,
    reason TEXT,
    at TEXT NOT NULL
);
CREATE INDEX health_events_host ON health_events(host, at);

CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    outcome TEXT NOT NULL,
    subject TEXT,
    message TEXT NOT NULL,
    detail TEXT,
    trace_id TEXT
);

CREATE TABLE kv (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

CREATE TABLE runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    host TEXT NOT NULL,
    rollout TEXT,
    revision TEXT NOT NULL,
    plan_digest TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT NOT NULL,
    outcome TEXT NOT NULL,
    changes INTEGER NOT NULL DEFAULT 0,
    removals INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    trace_id TEXT,
    detail TEXT
);
CREATE INDEX runs_host ON runs(host, finished_at);

CREATE TABLE apply_events (
    host TEXT NOT NULL,
    plan_digest TEXT NOT NULL,
    step TEXT NOT NULL,
    event TEXT NOT NULL,
    at TEXT NOT NULL,
    PRIMARY KEY (host, plan_digest, step)
);
"#,
    // 2: durable agent reports (ApplyResult, LocalRollback) seen per host, so
    // a report the agent re-sends after a reconnect is stored once.
    r#"
CREATE TABLE agent_reports (
    host TEXT NOT NULL,
    report_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    at TEXT NOT NULL,
    PRIMARY KEY (host, report_id)
);
"#,
];

/// A handle to the database. Cheap to clone.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store(..)")
    }
}

/// Borrowed access to the connection, handed to [`Store::call`] closures.
pub struct Db<'c> {
    conn: &'c Connection,
}

impl<'c> Db<'c> {
    /// Wraps a connection (for code already holding one).
    pub fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// The raw connection.
    pub fn conn(&self) -> &'c Connection {
        self.conn
    }

    /// Hosts and their enrolment.
    pub fn hosts(&self) -> Hosts<'c> {
        Hosts::new(self.conn)
    }

    /// Revisions built from the source.
    pub fn revisions(&self) -> Revisions<'c> {
        Revisions::new(self.conn)
    }

    /// Rollouts and their per-host rows.
    pub fn rollouts(&self) -> Rollouts<'c> {
        Rollouts::new(self.conn)
    }

    /// Phase approvals.
    pub fn approvals(&self) -> Approvals<'c> {
        Approvals::new(self.conn)
    }

    /// Health check results.
    pub fn health(&self) -> HealthEvents<'c> {
        HealthEvents::new(self.conn)
    }

    /// The audit log.
    pub fn audit(&self) -> Audit<'c> {
        Audit::new(self.conn)
    }

    /// Small key/value state.
    pub fn kv(&self) -> Kv<'c> {
        Kv::new(self.conn)
    }

    /// Apply runs per host.
    pub fn runs(&self) -> Runs<'c> {
        Runs::new(self.conn)
    }

    /// Durable agent reports already stored.
    pub fn reports(&self) -> AgentReports<'c> {
        AgentReports::new(self.conn)
    }
}

impl Store {
    /// Opens (creating if needed) the database at `path` and migrates it.
    pub async fn open(path: &Path) -> Result<Self> {
        let path = path.to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    buckle_core::error::wrap_user(
                        e,
                        format!("Could not create the data directory {}.", parent.display()),
                        &["Check that `data_dir` in server.yaml is writable by the server."],
                    )
                })?;
            }
            let conn = Connection::open(&path).map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("Could not open the database at {}.", path.display()),
                    &["Check that `data_dir` in server.yaml is writable by the server."],
                )
            })?;
            configure(&conn, true)?;
            Ok(conn)
        })
        .await
        .map_err(join_error)??;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// An in-memory database, migrated (tests and ephemeral runs).
    pub async fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(db_error)?;
        configure(&conn, false)?;
        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate().await?;
        Ok(store)
    }

    /// Applies every migration not yet recorded. Idempotent.
    pub async fn migrate(&self) -> Result<()> {
        self.call(|db| migrate(db.conn)).await
    }

    /// Runs `f` against the connection on the blocking pool.
    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'c> FnOnce(&Db<'c>) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().map_err(|_| {
                buckle_core::error::system(
                    "The database connection lock was poisoned by an earlier panic.",
                    &["Restart the server; report the panic that preceded this."],
                )
            })?;
            f(&Db::new(&guard))
        })
        .await
        .map_err(join_error)?
    }

    /// Runs `f` inside a transaction, committing when it returns `Ok`.
    pub async fn transaction<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: for<'c> FnOnce(&Db<'c>) -> Result<T> + Send + 'static,
    {
        self.call(move |db| {
            db.conn.execute_batch("BEGIN IMMEDIATE").map_err(db_error)?;
            match f(db) {
                Ok(v) => {
                    db.conn.execute_batch("COMMIT").map_err(db_error)?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = db.conn.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
        .await
    }
}

fn configure(conn: &Connection, on_disk: bool) -> Result<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(db_error)?;
    if on_disk {
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(db_error)?;
    }
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(db_error)?;
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(db_error)?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS migrations (id INTEGER PRIMARY KEY NOT NULL, applied_at TEXT NOT NULL)",
    )
    .map_err(db_error)?;
    let applied: i64 = conn
        .query_row("SELECT COALESCE(MAX(id), 0) FROM migrations", [], |r| {
            r.get(0)
        })
        .map_err(db_error)?;
    for (index, sql) in MIGRATIONS.iter().enumerate() {
        let id = index as i64 + 1;
        if id <= applied {
            continue;
        }
        conn.execute_batch("BEGIN IMMEDIATE").map_err(db_error)?;
        let result = conn.execute_batch(sql).and_then(|_| {
            conn.execute(
                "INSERT INTO migrations (id, applied_at) VALUES (?1, ?2)",
                rusqlite::params![id, Utc::now().to_rfc3339()],
            )
        });
        match result {
            Ok(_) => conn.execute_batch("COMMIT").map_err(db_error)?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(buckle_core::error::wrap_system(
                    e,
                    format!("Database migration {id} failed."),
                    &[
                        "The database may be from a newer Buckle; restore a backup or upgrade the server.",
                    ],
                ));
            }
        }
    }
    Ok(())
}

/// Converts a SQLite error into a system error.
pub(crate) fn db_error(e: rusqlite::Error) -> buckle_core::Error {
    buckle_core::error::wrap_system(
        e,
        "A database operation failed.",
        &[
            "Check the server's data directory for disk space and permissions; report the issue if it persists.",
        ],
    )
}

fn join_error(e: tokio::task::JoinError) -> buckle_core::Error {
    buckle_core::error::wrap_system(
        e,
        "A database task panicked or was cancelled.",
        &["Report this issue with the server logs."],
    )
}

/// Serialises a value to JSON for a TEXT column.
pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| {
        buckle_core::error::wrap_system(
            e,
            "A value could not be serialised for the database.",
            &["Report this issue."],
        )
    })
}

/// Parses a JSON TEXT column, falling back to the default on corruption is
/// not acceptable: corruption is reported as a system error.
pub(crate) fn from_json<T: serde::de::DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|e| {
        buckle_core::error::wrap_system(
            e,
            "A value stored in the database could not be read.",
            &["The database may be corrupt or from an incompatible Buckle version."],
        )
    })
}

/// Formats a timestamp for storage (RFC 3339, sortable).
pub(crate) fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

/// Parses a stored timestamp.
pub(crate) fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                format!("A timestamp stored in the database ('{s}') could not be read."),
                &["The database may be corrupt."],
            )
        })
}

pub(crate) fn parse_ts_opt(s: Option<String>) -> Result<Option<DateTime<Utc>>> {
    s.map(|s| parse_ts(&s)).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_are_idempotent_on_an_existing_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buckle.db");
        {
            let store = Store::open(&path).await.unwrap();
            store.call(|db| db.kv().set("k", "v")).await.unwrap();
        }
        let store = Store::open(&path).await.unwrap();
        store.migrate().await.unwrap();
        let (count, value) = store
            .call(|db| {
                let count: i64 = db
                    .conn()
                    .query_row("SELECT COUNT(*) FROM migrations", [], |r| r.get(0))
                    .map_err(db_error)?;
                Ok((count, db.kv().get("k")?))
            })
            .await
            .unwrap();
        assert_eq!(count, MIGRATIONS.len() as i64);
        assert_eq!(value.as_deref(), Some("v"));
    }

    #[tokio::test]
    async fn failed_transactions_roll_back() {
        let store = Store::in_memory().await.unwrap();
        let result: Result<()> = store
            .transaction(|db| {
                db.kv().set("a", "1")?;
                Err(buckle_core::error::user("nope", &[]))
            })
            .await;
        assert!(result.is_err());
        assert_eq!(store.call(|db| db.kv().get("a")).await.unwrap(), None);
    }
}
