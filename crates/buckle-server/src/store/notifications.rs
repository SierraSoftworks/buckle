//! What the attention list last held, and the webhook outbox.
//!
//! `attention_open` holds each item the server has announced as opened and
//! not yet resolved, so a restart neither repeats nor loses an edge.
//! `webhook_outbox` holds each event per webhook until it is delivered, in
//! order; `webhook_status` remembers whether a webhook has been failing.

use super::{db_error, from_json, parse_ts, parse_ts_opt, to_json, ts};
use buckle_api::attention::AttentionItem;
use buckle_core::Result;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

/// An item announced as opened.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenItem {
    pub item: AttentionItem,
    pub opened_at: DateTime<Utc>,
}

/// One event waiting for a webhook.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxRow {
    pub id: i64,
    pub hook: String,
    /// Sent as `X-Buckle-Delivery`; the same on every attempt.
    pub delivery: String,
    pub event: String,
    /// The exact body, so every attempt is signed alike.
    pub payload: String,
    pub created_at: DateTime<Utc>,
    pub attempts: u32,
    pub next_at: DateTime<Utc>,
    pub last_error: Option<String>,
}

/// A webhook's delivery health.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookStatus {
    /// The first failure since the last success.
    pub failing_since: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_success: Option<DateTime<Utc>>,
    /// Events waiting for it.
    pub queued: u32,
}

/// An identity decision from the audit log.
#[derive(Debug, Clone, PartialEq)]
pub struct IdentityDecision {
    pub host: String,
    pub target: String,
    pub purpose: String,
    pub granted: bool,
    pub message: String,
    pub at: DateTime<Utc>,
}

/// Repository over `attention_open`, `webhook_outbox` and `webhook_status`.
pub struct Notifications<'c> {
    conn: &'c Connection,
}

impl<'c> Notifications<'c> {
    pub(super) fn new(conn: &'c Connection) -> Self {
        Self { conn }
    }

    /// Every item announced as opened and not resolved.
    pub fn open_items(&self) -> Result<Vec<OpenItem>> {
        let mut stmt = self
            .conn
            .prepare("SELECT item, opened_at FROM attention_open ORDER BY opened_at, id")
            .map_err(db_error)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        rows.into_iter()
            .map(|(item, at)| {
                Ok(OpenItem {
                    item: from_json(&item)?,
                    opened_at: parse_ts(&at)?,
                })
            })
            .collect()
    }

    /// Records `item` as open (or its latest wording, when already open).
    pub fn upsert_open(&self, item: &AttentionItem, opened_at: DateTime<Utc>) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO attention_open (id, item, opened_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET item = excluded.item",
                params![item.id, to_json(item)?, ts(opened_at)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn resolve(&self, id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM attention_open WHERE id = ?1", params![id])
            .map_err(db_error)?;
        Ok(())
    }

    /// Queues `payload` for `hook`, due now.
    pub fn enqueue(
        &self,
        hook: &str,
        delivery: &str,
        event: &str,
        payload: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO webhook_outbox (hook, delivery, event, payload, created_at, next_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)",
                params![hook, delivery, event, payload, ts(at)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// The oldest event waiting for `hook`: deliveries go out in order.
    pub fn head(&self, hook: &str) -> Result<Option<OutboxRow>> {
        self.conn
            .query_row(
                "SELECT id, hook, delivery, event, payload, created_at, attempts, next_at, last_error FROM webhook_outbox WHERE hook = ?1 ORDER BY id LIMIT 1",
                params![hook],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, u32>(6)?,
                        r.get::<_, String>(7)?,
                        r.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?
            .map(
                |(id, hook, delivery, event, payload, created, attempts, next, last_error)| {
                    Ok(OutboxRow {
                        id,
                        hook,
                        delivery,
                        event,
                        payload,
                        created_at: parse_ts(&created)?,
                        attempts,
                        next_at: parse_ts(&next)?,
                        last_error,
                    })
                },
            )
            .transpose()
    }

    /// Removes an event: delivered, or given up on.
    pub fn remove(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM webhook_outbox WHERE id = ?1", params![id])
            .map_err(db_error)?;
        Ok(())
    }

    /// Records a failed attempt and when to try again.
    pub fn retry_at(&self, id: i64, next_at: DateTime<Utc>, error: &str) -> Result<()> {
        self.conn
            .execute(
                "UPDATE webhook_outbox SET attempts = attempts + 1, next_at = ?2, last_error = ?3 WHERE id = ?1",
                params![id, ts(next_at), error],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// Drops the events of webhooks no longer configured.
    pub fn forget_hooks_except(&self, hooks: &[String]) -> Result<usize> {
        let keep = to_json(&hooks)?;
        let n = self
            .conn
            .execute(
                "DELETE FROM webhook_outbox WHERE hook NOT IN (SELECT value FROM json_each(?1))",
                params![keep],
            )
            .map_err(db_error)?;
        self.conn
            .execute(
                "DELETE FROM webhook_status WHERE hook NOT IN (SELECT value FROM json_each(?1))",
                params![keep],
            )
            .map_err(db_error)?;
        Ok(n)
    }

    /// A delivery to `hook` succeeded.
    pub fn hook_ok(&self, hook: &str, at: DateTime<Utc>) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO webhook_status (hook, failing_since, last_error, last_success) VALUES (?1, NULL, NULL, ?2)
                 ON CONFLICT(hook) DO UPDATE SET failing_since = NULL, last_error = NULL, last_success = excluded.last_success",
                params![hook, ts(at)],
            )
            .map_err(db_error)?;
        Ok(())
    }

    /// A delivery to `hook` failed; it has been failing since the first
    /// failure after its last success.
    pub fn hook_failed(&self, hook: &str, at: DateTime<Utc>, error: &str) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO webhook_status (hook, failing_since, last_error) VALUES (?1, ?2, ?3)
                 ON CONFLICT(hook) DO UPDATE SET failing_since = COALESCE(failing_since, excluded.failing_since), last_error = excluded.last_error",
                params![hook, ts(at), error],
            )
            .map_err(db_error)?;
        Ok(())
    }

    pub fn hook_status(&self, hook: &str) -> Result<HookStatus> {
        let row = self
            .conn
            .query_row(
                "SELECT failing_since, last_error, last_success FROM webhook_status WHERE hook = ?1",
                params![hook],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(db_error)?;
        let queued: u32 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM webhook_outbox WHERE hook = ?1",
                params![hook],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        let (failing_since, last_error, last_success) = row.unwrap_or_default();
        Ok(HookStatus {
            failing_since: parse_ts_opt(failing_since)?,
            last_error,
            last_success: parse_ts_opt(last_success)?,
            queued,
        })
    }

    /// The latest identity decision per host, target and purpose since
    /// `since`, from the audit log (`identity.issued`, `identity.denied`).
    pub fn identity_decisions(&self, since: DateTime<Utc>) -> Result<Vec<IdentityDecision>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT actor, COALESCE(subject, ''), COALESCE(json_extract(detail, '$.purpose'), ''), action, message, at
                 FROM audit_log WHERE action IN ('identity.issued', 'identity.denied') AND at >= ?1 ORDER BY id",
            )
            .map_err(db_error)?;
        let rows = stmt
            .query_map(params![ts(since)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db_error)?;
        let mut latest: indexmap::IndexMap<(String, String, String), IdentityDecision> =
            indexmap::IndexMap::new();
        for (host, target, purpose, action, message, at) in rows {
            let key = (host.clone(), target.clone(), purpose.clone());
            latest.shift_remove(&key);
            latest.insert(
                key,
                IdentityDecision {
                    host,
                    target,
                    purpose,
                    granted: action == "identity.issued",
                    message,
                    at: parse_ts(&at)?,
                },
            );
        }
        Ok(latest.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewAudit, Store};
    use buckle_api::attention::{AttentionKind, AttentionSubject, AttentionTone, SubjectKind};
    use chrono::Duration;

    fn now() -> DateTime<Utc> {
        use chrono::SubsecRound;
        Utc::now().trunc_subsecs(6)
    }

    fn item(id: &str, summary: &str) -> AttentionItem {
        AttentionItem {
            id: id.into(),
            kind: AttentionKind::Offline,
            tone: AttentionTone::Warn,
            subject: AttentionSubject::new(SubjectKind::Host, "pi-1"),
            summary: summary.into(),
            since: None,
            opened_at: None,
        }
    }

    #[tokio::test]
    async fn open_items_keep_their_first_opening_and_latest_wording() {
        let store = Store::in_memory().await.unwrap();
        let t0 = now();
        let open = store
            .call(move |db| {
                let n = db.notifications();
                n.upsert_open(&item("a", "one"), t0)?;
                n.upsert_open(&item("a", "two"), t0 + Duration::minutes(1))?;
                n.upsert_open(&item("b", "b"), t0)?;
                n.resolve("b")?;
                n.open_items()
            })
            .await
            .unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(
            (open[0].item.summary.as_str(), open[0].opened_at),
            ("two", t0)
        );
    }

    #[tokio::test]
    async fn the_outbox_is_ordered_per_hook_and_tracks_failures() {
        let store = Store::in_memory().await.unwrap();
        let t0 = now();
        let (head, status, after) = store
            .call(move |db| {
                let n = db.notifications();
                n.enqueue("a", "d1", "attention.opened", "{}", t0)?;
                n.enqueue("b", "d2", "attention.opened", "{}", t0)?;
                n.enqueue("a", "d3", "attention.resolved", "{}", t0)?;
                let head = n.head("a")?.unwrap();
                n.retry_at(head.id, t0 + Duration::seconds(30), "boom")?;
                n.hook_failed("a", t0, "boom")?;
                n.hook_failed("a", t0 + Duration::seconds(30), "again")?;
                let status = n.hook_status("a")?;
                n.remove(head.id)?;
                n.hook_ok("a", t0 + Duration::minutes(1))?;
                n.forget_hooks_except(&["a".into()])?;
                Ok((
                    head,
                    status,
                    (n.head("a")?, n.head("b")?, n.hook_status("a")?),
                ))
            })
            .await
            .unwrap();
        assert_eq!(head.delivery, "d1");
        assert_eq!(status.failing_since, Some(t0));
        assert_eq!(status.last_error.as_deref(), Some("again"));
        assert_eq!(status.queued, 2);
        assert_eq!(after.0.unwrap().delivery, "d3");
        assert!(
            after.1.is_none(),
            "an unconfigured hook's events are dropped"
        );
        assert_eq!(after.2.failing_since, None);
    }

    #[tokio::test]
    async fn identity_decisions_keep_the_latest_per_host_target_and_purpose() {
        let store = Store::in_memory().await.unwrap();
        let t0 = now();
        let entry = |action: &str, target: &str, at| NewAudit {
            at,
            actor: "pi-1".into(),
            action: action.into(),
            outcome: String::new(),
            subject: Some(target.into()),
            message: format!("{action} {target}"),
            detail: Some(serde_json::json!({ "purpose": "deploy" })),
            trace_id: None,
        };
        let list = store
            .call(move |db| {
                for e in [
                    entry("identity.denied", "old", t0 - Duration::days(2)),
                    entry("identity.denied", "a", t0),
                    entry("identity.issued", "a", t0 + Duration::seconds(1)),
                    entry("identity.denied", "b", t0),
                ] {
                    db.audit().add(&e)?;
                }
                db.notifications()
                    .identity_decisions(t0 - Duration::days(1))
            })
            .await
            .unwrap();
        let got: Vec<_> = list
            .iter()
            .map(|d| (d.target.as_str(), d.purpose.as_str(), d.granted))
            .collect();
        assert_eq!(got, vec![("a", "deploy", true), ("b", "deploy", false)]);
    }
}
