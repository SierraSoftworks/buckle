//! The audit log: every operator action and controller transition.

use super::{db_error, from_json, parse_ts, to_json, ts};
use buckle_api::AuditEntry;
use buckle_core::{FilterValue, Filterable, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

/// A new audit row.
#[derive(Debug, Clone)]
pub struct NewAudit {
    pub at: DateTime<Utc>,
    pub actor: String,
    pub action: String,
    pub outcome: String,
    pub subject: Option<String>,
    pub message: String,
    pub detail: Option<serde_json::Value>,
    pub trace_id: Option<String>,
}

/// What to list.
#[derive(Default)]
pub struct AuditFilter {
    /// A filt-rs expression over the entry's fields (`actor`, `action`,
    /// `outcome`, `subject`, `message`, `trace_id`, `at`, `id`, `detail.*`).
    pub filter: Option<buckle_core::Filter>,
    /// Only entries with an id below this (pagination).
    pub before: Option<u64>,
    pub limit: u32,
}

/// Repository over `audit_log`.
pub struct Audit<'c> {
    conn: &'c Connection,
}

/// The filter view of an audit entry.
pub struct AuditView<'a>(pub &'a AuditEntry);

impl Filterable for AuditView<'_> {
    fn get(&self, key: &str) -> FilterValue<'_> {
        let e = self.0;
        match key.to_ascii_lowercase().as_str() {
            "id" => FilterValue::Number(e.id as f64),
            "at" => FilterValue::DateTime(e.at),
            "actor" => e.actor.as_str().into(),
            "action" => e.action.as_str().into(),
            "outcome" => e.outcome.as_str().into(),
            "subject" => e.subject.as_deref().into(),
            "message" => e.message.as_str().into(),
            "trace_id" => e.trace_id.as_deref().into(),
            other => match (other.strip_prefix("detail."), &e.detail) {
                (Some(path), Some(detail)) => buckle_core::selectors::lookup_path(detail, path)
                    .map(buckle_core::selectors::json_to_filter_value)
                    .unwrap_or(FilterValue::Null),
                _ => FilterValue::Null,
            },
        }
    }
}

impl<'c> Audit<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Appends an entry and returns it with its id.
    pub fn add(&self, entry: &NewAudit) -> Result<AuditEntry> {
        let detail = entry.detail.as_ref().map(to_json).transpose()?;
        self.conn
            .execute(
                "INSERT INTO audit_log (at, actor, action, outcome, subject, message, detail, trace_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    ts(entry.at),
                    entry.actor,
                    entry.action,
                    entry.outcome,
                    entry.subject,
                    entry.message,
                    detail,
                    entry.trace_id
                ],
            )
            .map_err(db_error)?;
        Ok(AuditEntry {
            id: self.conn.last_insert_rowid() as u64,
            at: entry.at,
            actor: entry.actor.clone(),
            action: entry.action.clone(),
            outcome: entry.outcome.clone(),
            subject: entry.subject.clone(),
            message: entry.message.clone(),
            detail: entry.detail.clone(),
            trace_id: entry.trace_id.clone(),
        })
    }

    /// Newest first, filtered. Returns the page and the total matching count.
    pub fn list(&self, filter: &AuditFilter) -> Result<(Vec<AuditEntry>, u64)> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, at, actor, action, outcome, subject, message, detail, trace_id FROM audit_log WHERE id < ?1 ORDER BY id DESC")
            .map_err(db_error)?;
        let before = filter.before.map(|b| b as i64).unwrap_or(i64::MAX);
        let rows = stmt
            .query_map(params![before], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })
            .map_err(db_error)?;
        let limit = if filter.limit == 0 { 100 } else { filter.limit } as usize;
        let mut out = Vec::new();
        let mut total = 0u64;
        for row in rows {
            let (id, at, actor, action, outcome, subject, message, detail, trace_id) =
                row.map_err(db_error)?;
            let entry = AuditEntry {
                id: id as u64,
                at: parse_ts(&at)?,
                actor,
                action,
                outcome,
                subject,
                message,
                detail: detail.as_deref().map(from_json).transpose()?,
                trace_id,
            };
            let keep = match &filter.filter {
                Some(f) => f.matches(&AuditView(&entry)).unwrap_or(false),
                None => true,
            };
            if keep {
                total += 1;
                if out.len() < limit {
                    out.push(entry);
                }
            }
        }
        Ok((out, total))
    }
}

impl Audit<'_> {
    /// Entries about one subject (e.g. a rollout id), newest first.
    pub fn for_subject(&self, subject: &str, limit: u32) -> Result<Vec<AuditEntry>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, at, actor, action, outcome, subject, message, detail, trace_id FROM audit_log WHERE subject = ?1 ORDER BY id DESC LIMIT ?2")
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![subject, limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .map(
                |(id, at, actor, action, outcome, subject, message, detail, trace_id)| {
                    Ok(AuditEntry {
                        id: id as u64,
                        at: parse_ts(&at)?,
                        actor,
                        action,
                        outcome,
                        subject,
                        message,
                        detail: detail.as_deref().map(from_json).transpose()?,
                        trace_id,
                    })
                },
            )
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::Store;
    use super::*;
    use rstest::rstest;

    fn entry(actor: &str, action: &str, subject: &str) -> NewAudit {
        NewAudit {
            at: Utc::now(),
            actor: actor.into(),
            action: action.into(),
            outcome: "ok".into(),
            subject: Some(subject.into()),
            message: format!("{actor} did {action}"),
            detail: Some(serde_json::json!({"phase": "canary"})),
            trace_id: None,
        }
    }

    #[rstest]
    #[case(None, 3)]
    #[case(Some(r#"actor == "alice""#), 2)]
    #[case(Some(r#"action == "rollout.approve" && subject == "r1""#), 1)]
    #[case(Some(r#"detail.phase == "canary""#), 3)]
    #[case(Some(r#"message contains "pause""#), 1)]
    #[tokio::test]
    async fn audit_entries_filter_with_filt_rs(#[case] expr: Option<&str>, #[case] expected: u64) {
        let store = Store::in_memory().await.unwrap();
        let filter = expr.map(|e| buckle_core::Filter::new(e).unwrap());
        let (items, total) = store
            .call(move |db| {
                db.audit().add(&entry("alice", "rollout.approve", "r1"))?;
                db.audit().add(&entry("alice", "rollout.pause", "r2"))?;
                db.audit()
                    .add(&entry("controller", "rollout.start", "r1"))?;
                db.audit().list(&AuditFilter {
                    filter,
                    before: None,
                    limit: 50,
                })
            })
            .await
            .unwrap();
        assert_eq!(total, expected);
        assert_eq!(items.len() as u64, expected);
        assert!(items.windows(2).all(|w| w[0].id > w[1].id), "newest first");
    }
}
