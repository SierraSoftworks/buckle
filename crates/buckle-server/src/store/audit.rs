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

/// The most rows one filtered audit query examines.
pub const SCAN_LIMIT: usize = 5000;

/// Controller transitions that only show a host moving through a rollout's
/// ordinary steps (`progress` in filters); outcomes such as healthy, failed,
/// held, skipped or rolled back are not progress.
pub const PROGRESS_ACTIONS: &[&str] = &[
    "rollout.host.plan_pending",
    "rollout.host.planned",
    "rollout.host.baseline",
    "rollout.host.applying",
    "rollout.host.baking",
    "rollout.host.rolling_back",
];

fn is_progress(actor: &str, action: &str) -> bool {
    actor == "controller" && PROGRESS_ACTIONS.contains(&action)
}

fn progress_sql() -> String {
    let actions = PROGRESS_ACTIONS
        .iter()
        .map(|a| format!("'{a}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("actor = 'controller' AND action IN ({actions})")
}

/// One page of the audit log.
#[derive(Debug, Clone)]
pub struct AuditPage {
    pub items: Vec<AuditEntry>,
    /// Pass as `before` for the next page; `None` at the end.
    pub next: Option<u64>,
    /// Matching entries below `before` (with an expression: among the rows
    /// examined).
    pub total: u64,
}

/// What to list.
#[derive(Default)]
pub struct AuditFilter {
    /// A filt-rs expression over the entry's fields (`actor`, `action`,
    /// `outcome`, `subject`, `message`, `trace_id`, `at`, `id`, `detail.*`,
    /// and `progress`, true for a controller progress transition).
    pub filter: Option<buckle_core::Filter>,
    /// Only entries with an id below this (pagination).
    pub before: Option<u64>,
    pub limit: u32,
    /// Leave out controller progress transitions ([`PROGRESS_ACTIONS`]).
    pub hide_progress: bool,
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
            "progress" => FilterValue::Bool(is_progress(&e.actor, &e.action)),
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

    /// Newest first, filtered: the page and the matching count. See
    /// [`Audit::page`], which this wraps.
    pub fn list(&self, filter: &AuditFilter) -> Result<(Vec<AuditEntry>, u64)> {
        let page = self.page(filter)?;
        Ok((page.items, page.total))
    }

    /// One page, newest first, below `filter.before`.
    ///
    /// Without an expression the database does the work (`LIMIT`, `COUNT`).
    /// A filt-rs expression can only be evaluated here, so at most
    /// [`SCAN_LIMIT`] rows are examined per call: `total` then counts the
    /// matches among the rows examined, and `next` continues from where the
    /// scan stopped even when the page is not full.
    pub fn page(&self, filter: &AuditFilter) -> Result<AuditPage> {
        let before = filter.before.map(|b| b as i64).unwrap_or(i64::MAX);
        let limit = if filter.limit == 0 { 100 } else { filter.limit } as usize;
        let progress = if filter.hide_progress {
            format!(" AND NOT ({})", progress_sql())
        } else {
            String::new()
        };
        let Some(expr) = &filter.filter else {
            let total: i64 = self
                .conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM audit_log WHERE id < ?1{progress}"),
                    params![before],
                    |r| r.get(0),
                )
                .map_err(db_error)?;
            let mut items = self.rows(&progress, before, limit as i64 + 1)?;
            let more = items.len() > limit;
            items.truncate(limit);
            return Ok(AuditPage {
                next: more.then(|| items.last().map(|e| e.id)).flatten(),
                items,
                total: total as u64,
            });
        };
        let scanned = self.rows(&progress, before, SCAN_LIMIT as i64)?;
        let exhausted = scanned.len() < SCAN_LIMIT;
        let last_scanned = scanned.last().map(|e| e.id);
        let mut items = Vec::new();
        let mut total = 0u64;
        let mut more = false;
        for entry in scanned {
            if !expr.matches(&AuditView(&entry)).unwrap_or(false) {
                continue;
            }
            total += 1;
            if items.len() < limit {
                items.push(entry);
            } else {
                more = true;
            }
        }
        let next = if more {
            items.last().map(|e| e.id)
        } else if !exhausted {
            last_scanned
        } else {
            None
        };
        Ok(AuditPage { items, next, total })
    }

    fn rows(&self, extra_where: &str, before: i64, limit: i64) -> Result<Vec<AuditEntry>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT id, at, actor, action, outcome, subject, message, detail, trace_id FROM audit_log WHERE id < ?1{extra_where} ORDER BY id DESC LIMIT ?2"))
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![before, limit], |r| {
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
                    ..Default::default()
                })
            })
            .await
            .unwrap();
        assert_eq!(total, expected);
        assert_eq!(items.len() as u64, expected);
        assert!(items.windows(2).all(|w| w[0].id > w[1].id), "newest first");
    }

    async fn seeded(n: usize) -> Store {
        let store = Store::in_memory().await.unwrap();
        store
            .call(move |db| {
                for i in 0..n {
                    let action = if i % 2 == 0 {
                        "rollout.host.applying"
                    } else {
                        "rollout.host.healthy"
                    };
                    db.audit().add(&entry("controller", action, "r1"))?;
                }
                Ok(())
            })
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn pages_follow_the_cursor_newest_first() {
        let store = seeded(25).await;
        let mut seen = Vec::new();
        let mut before = None;
        loop {
            let page = store
                .call(move |db| {
                    db.audit().page(&AuditFilter {
                        before,
                        limit: 10,
                        ..Default::default()
                    })
                })
                .await
                .unwrap();
            seen.extend(page.items.iter().map(|e| e.id));
            match page.next {
                Some(n) => before = Some(n),
                None => break,
            }
        }
        assert_eq!(seen.len(), 25);
        assert!(seen.windows(2).all(|w| w[0] > w[1]));
    }

    #[tokio::test]
    async fn progress_transitions_can_be_left_out() {
        let store = seeded(6).await;
        let (hidden, by_expr) = store
            .call(|db| {
                Ok((
                    db.audit().page(&AuditFilter {
                        limit: 50,
                        hide_progress: true,
                        ..Default::default()
                    })?,
                    db.audit().page(&AuditFilter {
                        filter: Some(buckle_core::Filter::new("progress == false").unwrap()),
                        limit: 50,
                        ..Default::default()
                    })?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(hidden.total, 3);
        assert!(
            hidden
                .items
                .iter()
                .all(|e| e.action == "rollout.host.healthy")
        );
        assert_eq!(by_expr.total, 3);
    }

    #[tokio::test]
    async fn filtered_scans_are_bounded_and_resumable() {
        let store = seeded(SCAN_LIMIT + 20).await;
        let filter = || Some(buckle_core::Filter::new(r#"action == "nothing.matches""#).unwrap());
        let first = store
            .call(move |db| {
                db.audit().page(&AuditFilter {
                    filter: filter(),
                    limit: 10,
                    ..Default::default()
                })
            })
            .await
            .unwrap();
        assert!(first.items.is_empty());
        let next = first.next.expect("the scan stopped early and says where");
        let second = store
            .call(move |db| {
                db.audit().page(&AuditFilter {
                    filter: filter(),
                    before: Some(next),
                    limit: 10,
                    ..Default::default()
                })
            })
            .await
            .unwrap();
        assert_eq!(second.next, None, "the rest fits in one scan");
    }
}
