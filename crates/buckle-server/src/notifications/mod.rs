//! The attention list, server-side (`GET /api/v1/attention`), and the
//! webhooks it feeds.
//!
//! The [`Notifier`] works the list out from the store, compares it with what
//! it last announced (`attention_open`) and queues an `attention.opened` or
//! `attention.resolved` event per configured webhook for each change, in the
//! same transaction. [`webhooks::Deliveries`] sends the queue. The UI's bell
//! reads the same list, so the two cannot disagree.

pub mod webhooks;

use crate::clock::Clock;
use crate::config::WebhookConfig;
use crate::events::EventBus;
use crate::source::cache::RevisionCache;
use crate::store::Store;
use buckle_api::attention::{
    self, AttentionInputs, AttentionItem, AttentionList, IdentityRefusal, TargetChecks,
    WebhookFailure,
};
use buckle_api::{LiveEvent, TargetKind};
use chrono::{DateTime, Duration, SubsecRound, Utc};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// How often the list is worked out without a live event to prompt it, so
/// that conditions which only age (an agent offline) are noticed.
const TICK: std::time::Duration = std::time::Duration::from_secs(15);
/// How long an open item must be missing before it resolves, so an agent
/// that reconnects at once (a restart, an upgrade) repeats nothing.
const RESOLVE_AFTER: i64 = 60;
/// How long after start nothing resolves, while agents reconnect.
const STARTUP_GRACE: i64 = 2 * 60;
/// How long a webhook fails before it is on the list itself.
const HOOK_FAILING_AFTER: i64 = 5 * 60;

/// `attention.opened` / `attention.resolved`.
pub const OPENED: &str = "attention.opened";
pub const RESOLVED: &str = "attention.resolved";

/// Works out the attention list and announces its edges.
pub struct Notifier {
    store: Store,
    revisions: Arc<RevisionCache>,
    events: EventBus,
    clock: Arc<dyn Clock>,
    hooks: Vec<WebhookConfig>,
    /// `web.base_url`, for links in webhook payloads.
    base_url: Option<String>,
    /// The agent version hosts should run.
    expected_version: String,
    /// When the server started watching agents.
    started: DateTime<Utc>,
    /// Woken when the outbox gains events.
    outbox: Arc<Notify>,
    /// One pass at a time, so an edge is never announced twice.
    pass: tokio::sync::Mutex<PassState>,
    /// The list as last worked out.
    last: Mutex<Vec<AttentionItem>>,
}

#[derive(Default)]
struct PassState {
    /// Open items missing from the latest passes, since when.
    missing: HashMap<String, DateTime<Utc>>,
}

/// What [`Notifier::new`] needs.
pub struct NotifierDeps {
    pub store: Store,
    pub revisions: Arc<RevisionCache>,
    pub events: EventBus,
    pub clock: Arc<dyn Clock>,
    pub hooks: Vec<WebhookConfig>,
    pub base_url: Option<String>,
    pub expected_version: String,
    pub outbox: Arc<Notify>,
}

impl Notifier {
    pub fn new(deps: NotifierDeps) -> Self {
        let started = deps.clock.now();
        Self {
            store: deps.store,
            revisions: deps.revisions,
            events: deps.events,
            clock: deps.clock,
            hooks: deps.hooks,
            base_url: deps.base_url,
            expected_version: deps.expected_version,
            started,
            outbox: deps.outbox,
            pass: Default::default(),
            last: Default::default(),
        }
    }

    /// The list as of now (worked out afresh, edges announced).
    pub async fn list(&self) -> buckle_core::Result<AttentionList> {
        let items = self.pass(self.clock.now()).await?;
        Ok(AttentionList { items })
    }

    /// Works the list out as of `now`, announces what opened and resolved
    /// since the last pass, and returns the list.
    pub async fn pass(&self, now: DateTime<Utc>) -> buckle_core::Result<Vec<AttentionItem>> {
        // As the store keeps it, so a stored time compares equal.
        let now = now.trunc_subsecs(6);
        let mut state = self.pass.lock().await;
        let current = self.compute(now).await?;
        let hooks: Vec<String> = self.hooks.iter().map(|h| h.name.clone()).collect();
        let base_url = self.base_url.clone();
        let missing = state.missing.clone();
        let settled = now - self.started >= Duration::seconds(STARTUP_GRACE);
        let (items, missing, queued) = self
            .store
            .transaction(move |db| {
                let n = db.notifications();
                let open: BTreeMap<String, crate::store::notifications::OpenItem> = n
                    .open_items()?
                    .into_iter()
                    .map(|o| (o.item.id.clone(), o))
                    .collect();
                let mut queued = 0;
                let mut items = Vec::new();
                let mut still_missing = HashMap::new();
                for mut item in current {
                    match open.get(&item.id) {
                        Some(o) => {
                            item.opened_at = Some(o.opened_at);
                            if o.item != item {
                                n.upsert_open(&item, o.opened_at)?;
                            }
                        }
                        None => {
                            item.opened_at = Some(now);
                            n.upsert_open(&item, now)?;
                            for hook in &hooks {
                                let (delivery, body) =
                                    payload(OPENED, &item, now, base_url.as_deref());
                                n.enqueue(hook, &delivery, OPENED, &body, now)?;
                                queued += 1;
                            }
                        }
                    }
                    items.push(item);
                }
                for (id, o) in open {
                    if items.iter().any(|i| i.id == id) {
                        continue;
                    }
                    let since = missing.get(&id).copied().unwrap_or(now);
                    if !settled || now - since < Duration::seconds(RESOLVE_AFTER) {
                        still_missing.insert(id, since);
                        items.push(o.item);
                        continue;
                    }
                    n.resolve(&id)?;
                    for hook in &hooks {
                        let (delivery, body) = payload(RESOLVED, &o.item, now, base_url.as_deref());
                        n.enqueue(hook, &delivery, RESOLVED, &body, now)?;
                        queued += 1;
                    }
                }
                Ok((items, still_missing, queued))
            })
            .await?;
        state.missing = missing;
        let mut items = items;
        attention::sort(&mut items);
        if queued > 0 {
            self.outbox.notify_one();
        }
        let changed = {
            let mut last = self.last.lock().unwrap();
            let changed = *last != items;
            last.clone_from(&items);
            changed
        };
        if changed {
            self.events.publish(LiveEvent::Attention {
                list: AttentionList {
                    items: items.clone(),
                },
            });
        }
        Ok(items)
    }

    /// The list as the store has it now, before edges.
    async fn compute(&self, now: DateTime<Utc>) -> buckle_core::Result<Vec<AttentionItem>> {
        let fleet = crate::views::reference_revision(&self.store, &self.revisions).await;
        let hooks: Vec<String> = self.hooks.iter().map(|h| h.name.clone()).collect();
        let refusals_since = now - Duration::seconds(attention::REFUSAL_ATTENTION);
        let gathered = self
            .store
            .call(move |db| {
                let mut targets = Vec::new();
                let mut checks = Vec::new();
                for h in db.hosts().targets()? {
                    let facts = db
                        .hosts()
                        .latest_facts(&h.id)?
                        .map(|(f, _)| f)
                        .unwrap_or(serde_json::Value::Null);
                    let mut s = crate::views::host_summary(&h, &facts, fleet.as_deref());
                    s.cluster_status =
                        crate::views::cluster_status(db, &h, &facts, fleet.as_deref())?;
                    if s.kind == TargetKind::Host {
                        // Only the checks of the revision the host runs: a
                        // check since removed keeps its last reading.
                        let current: Vec<_> = db
                            .health()
                            .checks(&h.id)?
                            .into_iter()
                            .filter(|c| {
                                s.applied_revision.as_ref().is_none_or(|applied| {
                                    c.history.first().is_none_or(|e| &e.revision == applied)
                                })
                            })
                            .collect();
                        checks.push((h.id.clone(), current));
                    }
                    targets.push(s);
                }
                let active = db
                    .rollouts()
                    .list(50, 0)?
                    .into_iter()
                    .find(|r| r.is_active());
                let rollout = match active {
                    Some(r) => {
                        let hosts = db.rollouts().hosts(&r.id)?;
                        let approvals = db.approvals().for_rollout(&r.id)?;
                        Some(crate::rollout::view::detail(&r, &hosts, &approvals, &[]))
                    }
                    None => None,
                };
                let refusals: Vec<IdentityRefusal> = db
                    .notifications()
                    .identity_decisions(refusals_since)?
                    .into_iter()
                    .filter(|d| !d.granted)
                    .map(|d| IdentityRefusal {
                        reason: d
                            .message
                            .split_once(": ")
                            .map(|(_, r)| r.to_owned())
                            .unwrap_or(d.message),
                        host: d.host,
                        target: d.target,
                        purpose: d.purpose,
                        at: d.at,
                    })
                    .collect();
                let mut webhooks = Vec::new();
                for hook in hooks {
                    let st = db.notifications().hook_status(&hook)?;
                    if let Some(since) = st.failing_since
                        && now - since >= Duration::seconds(HOOK_FAILING_AFTER)
                    {
                        webhooks.push(WebhookFailure {
                            name: hook,
                            since,
                            error: st.last_error.unwrap_or_default(),
                            queued: st.queued,
                        });
                    }
                }
                Ok((targets, checks, rollout, refusals, webhooks))
            })
            .await?;
        let (mut hosts, checks, rollout, refusals, webhooks) = gathered;
        let clusters: Vec<_> = hosts
            .extract_if(.., |h| h.kind == TargetKind::Cluster)
            .collect();
        let skipped = crate::schedules::skipped(&self.store, &self.revisions).await?;
        let paused = self.store.call(|db| db.paused().list()).await?;
        let checks: Vec<TargetChecks<'_>> = checks
            .iter()
            .map(|(target, checks)| TargetChecks { target, checks })
            .collect();
        Ok(attention::build(
            &AttentionInputs {
                hosts: &hosts,
                clusters: &clusters,
                expected_version: Some(&self.expected_version),
                rollout: rollout.as_ref(),
                checks: &checks,
                skipped: &skipped,
                refusals: &refusals,
                webhooks: &webhooks,
                paused: &paused,
                watching_since: Some(self.started),
            },
            now,
        ))
    }

    /// Works the list out every [`TICK`], and soon after live events.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let mut rx = self.events.subscribe();
        loop {
            if let Err(e) = self.pass(self.clock.now()).await {
                tracing::warn!(error = %e, "the attention list could not be worked out");
            }
            let woke = tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(TICK) => false,
                r = rx.recv() => !matches!(r, Err(tokio::sync::broadcast::error::RecvError::Closed)),
            };
            if woke {
                // Let a burst of events settle into one pass; skip our own.
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                }
                while rx.try_recv().is_ok() {}
            }
        }
    }
}

/// A webhook body: a fresh delivery id and the JSON as it will be signed.
fn payload(
    event: &str,
    item: &AttentionItem,
    now: DateTime<Utc>,
    base_url: Option<&str>,
) -> (String, String) {
    let delivery = uuid::Uuid::new_v4().to_string();
    let url = item.path().map(|p| match base_url {
        Some(base) => format!("{}{p}", base.trim_end_matches('/')),
        None => p,
    });
    let mut body = serde_json::json!({
        "event": event,
        "delivery": delivery,
        "at": now,
        "item": item,
        "url": url,
        "server": { "url": base_url, "version": crate::VERSION },
    });
    if event == RESOLVED {
        body["resolved_at"] = serde_json::json!(now);
    }
    (delivery, body.to_string())
}

#[cfg(test)]
mod tests;
