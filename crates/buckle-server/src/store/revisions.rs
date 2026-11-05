//! Revisions: one row per processed commit, valid or rejected.

use super::{db_error, from_json, parse_ts, to_json, ts};
use buckle_api::RevisionStatus;
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};

/// One row of `revisions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevisionRecord {
    /// The revision id (the commit id, or a tree digest for a path source).
    pub id: String,
    pub commit: String,
    pub author: String,
    pub message: String,
    pub committed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    /// Digest of the revision tarball; `None` for rejected revisions.
    pub tarball_digest: Option<String>,
    pub status: RevisionStatus,
    pub errors: Vec<String>,
    pub module_count: u32,
    /// Workloads under `workloads/`; `None` until counted.
    pub workload_count: Option<u32>,
    /// `git` or `path`.
    pub source: String,
    /// The server version that validated it; `None` before this was recorded.
    pub validated_by: Option<String>,
}

/// Repository over `revisions` and `revision_blobs`.
pub struct Revisions<'c> {
    conn: &'c Connection,
}

const COLUMNS: &str = "id, commit_id, author, message, committed_at, created_at, tarball_digest, status, errors, module_count, source, validated_by, workload_count";

fn row(r: &Row<'_>) -> rusqlite::Result<[Option<String>; 13]> {
    Ok([
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
        r.get(7)?,
        r.get(8)?,
        r.get::<_, i64>(9).map(|n| Some(n.to_string()))?,
        r.get(10)?,
        r.get(11)?,
        r.get::<_, Option<i64>>(12)
            .map(|n| n.map(|n| n.to_string()))?,
    ])
}

fn into_record(c: [Option<String>; 13]) -> Result<RevisionRecord> {
    let [
        id,
        commit,
        author,
        message,
        committed,
        created,
        digest,
        status,
        errors,
        modules,
        source,
        validated_by,
        workloads,
    ] = c;
    Ok(RevisionRecord {
        id: id.unwrap_or_default(),
        commit: commit.unwrap_or_default(),
        author: author.unwrap_or_default(),
        message: message.unwrap_or_default(),
        committed_at: parse_ts(&committed.unwrap_or_default())?,
        created_at: parse_ts(&created.unwrap_or_default())?,
        tarball_digest: digest,
        status: if status.as_deref() == Some("valid") {
            RevisionStatus::Valid
        } else {
            RevisionStatus::Rejected
        },
        errors: from_json(errors.as_deref().unwrap_or("[]"))?,
        module_count: modules.and_then(|m| m.parse().ok()).unwrap_or(0),
        source: source.unwrap_or_default(),
        validated_by,
        workload_count: workloads.and_then(|w| w.parse().ok()),
    })
}

impl<'c> Revisions<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Inserts (or replaces) a revision.
    pub fn upsert(&self, rev: &RevisionRecord) -> Result<()> {
        self.conn
            .execute(
                &format!("INSERT OR REPLACE INTO revisions ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"),
                params![
                    rev.id,
                    rev.commit,
                    rev.author,
                    rev.message,
                    ts(rev.committed_at),
                    ts(rev.created_at),
                    rev.tarball_digest,
                    match rev.status {
                        RevisionStatus::Valid => "valid",
                        RevisionStatus::Rejected => "rejected",
                    },
                    to_json(&rev.errors)?,
                    rev.module_count as i64,
                    rev.source,
                    rev.validated_by,
                    rev.workload_count.map(i64::from),
                ],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// The revision, if known.
    pub fn get(&self, id: &str) -> Result<Option<RevisionRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM revisions WHERE id = ?1"),
                params![id],
                row,
            )
            .optional()
            .map_err(db_error)?
            .map(into_record)
            .transpose()
    }

    /// Newest first.
    pub fn list(&self, limit: u32, offset: u32) -> Result<Vec<RevisionRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM revisions ORDER BY created_at DESC, rowid DESC LIMIT ?1 OFFSET ?2"
            ))
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![limit, offset], row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter().map(into_record).collect()
    }

    /// Rejected revisions not validated by server `version`, newest first.
    pub fn rejected_unless_validated_by(&self, version: &str) -> Result<Vec<RevisionRecord>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM revisions WHERE status = 'rejected' AND validated_by IS NOT ?1 ORDER BY created_at DESC, rowid DESC"
            ))
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![version], row)
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter().map(into_record).collect()
    }

    /// Valid revisions whose workloads were never counted, newest first.
    pub fn uncounted_workloads(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM revisions WHERE status = 'valid' AND workload_count IS NULL ORDER BY created_at DESC, rowid DESC")
            .map_err(db_error)?;
        stmt.query_map([], |r| r.get(0))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<String>>>()
            .map_err(db_error)
    }

    /// Records how many workloads `id` declares.
    pub fn set_workload_count(&self, id: &str, count: u32) -> Result<()> {
        self.conn
            .execute(
                "UPDATE revisions SET workload_count = ?2 WHERE id = ?1",
                params![id, i64::from(count)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Total number of revisions.
    pub fn count(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM revisions", [], |r| r.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(db_error)
    }

    /// Records that `digest` is the file at `path` in `revision`.
    pub fn add_blob(&self, digest: &str, revision: &str, path: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO revision_blobs (digest, revision, path) VALUES (?1, ?2, ?3)",
                params![digest, revision, path],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Where a blob with this digest can be read from within one of
    /// `revisions`: `(revision, path)`.
    pub fn find_blob_in(
        &self,
        digest: &str,
        revisions: &[String],
    ) -> Result<Option<(String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT revision, path FROM revision_blobs WHERE digest = ?1")
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![digest], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()
            .map_err(db_error)?;
        Ok(rows.into_iter().find(|(rev, _)| revisions.contains(rev)))
    }

    /// Where a blob with this digest can be read from: `(revision, path)`.
    pub fn find_blob(&self, digest: &str) -> Result<Option<(String, String)>> {
        self.conn
            .query_row(
                "SELECT revision, path FROM revision_blobs WHERE digest = ?1 LIMIT 1",
                params![digest],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(db_error)
    }
}
