//! Sends the webhook outbox: each webhook's events in order, at least once,
//! signed like GitHub's (`X-Hub-Signature-256: sha256=<hex HMAC of the
//! body>`), retried with backoff for up to a day. Sending runs beside
//! everything else and never holds up a rollout.

use crate::clock::Clock;
use crate::config::WebhookConfig;
use crate::store::Store;
use crate::store::notifications::OutboxRow;
use chrono::{DateTime, Duration, SubsecRound, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// How long an event is retried before it is given up on.
pub const MAX_AGE: i64 = 24 * 60 * 60;
/// The first retry's delay; each further one doubles, up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: i64 = 15;
const MAX_BACKOFF: i64 = 60 * 60;
/// The longest the worker sleeps with nothing due.
const IDLE: i64 = 60;

/// The delay before attempt `attempts + 1`, after `attempts` failures.
pub fn backoff(attempts: u32) -> Duration {
    let secs = FIRST_BACKOFF.saturating_mul(1i64 << attempts.saturating_sub(1).min(20));
    Duration::seconds(secs.min(MAX_BACKOFF))
}

/// The `X-Hub-Signature-256` value of `body` under `secret`.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts a key of any length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// The delivery worker.
pub struct Deliveries {
    store: Store,
    hooks: Vec<WebhookConfig>,
    client: reqwest::Client,
    clock: Arc<dyn Clock>,
    wake: Arc<Notify>,
}

impl Deliveries {
    pub fn new(
        store: Store,
        hooks: Vec<WebhookConfig>,
        clock: Arc<dyn Clock>,
        wake: Arc<Notify>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("buckle/{}", crate::VERSION))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            store,
            hooks,
            client,
            clock,
            wake,
        }
    }

    /// Sends whatever is due as of `now`, each webhook's events in order,
    /// and returns when the next is due.
    pub async fn pass(&self, now: DateTime<Utc>) -> buckle_core::Result<Option<DateTime<Utc>>> {
        let now = now.trunc_subsecs(6);
        let mut next: Option<DateTime<Utc>> = None;
        for hook in &self.hooks {
            loop {
                let name = hook.name.clone();
                let Some(row) = self
                    .store
                    .call(move |db| db.notifications().head(&name))
                    .await?
                else {
                    break;
                };
                if now - row.created_at > Duration::seconds(MAX_AGE) {
                    tracing::warn!(
                        webhook = %hook.name,
                        delivery = %row.delivery,
                        event = %row.event,
                        attempts = row.attempts,
                        error = row.last_error.as_deref().unwrap_or(""),
                        "a webhook delivery was given up on after a day"
                    );
                    self.store
                        .call(move |db| db.notifications().remove(row.id))
                        .await?;
                    continue;
                }
                if row.next_at > now {
                    next = Some(next.map_or(row.next_at, |n| n.min(row.next_at)));
                    break;
                }
                match self.send(hook, &row).await {
                    Ok(()) => {
                        tracing::debug!(webhook = %hook.name, delivery = %row.delivery, event = %row.event, "webhook delivered");
                        let (id, name) = (row.id, hook.name.clone());
                        self.store
                            .call(move |db| {
                                db.notifications().remove(id)?;
                                db.notifications().hook_ok(&name, now)
                            })
                            .await?;
                    }
                    Err(error) => {
                        let at = now + backoff(row.attempts + 1);
                        tracing::info!(webhook = %hook.name, delivery = %row.delivery, %error, retry_at = %at, "a webhook delivery failed");
                        let (id, name) = (row.id, hook.name.clone());
                        self.store
                            .call(move |db| {
                                db.notifications().retry_at(id, at, &error)?;
                                db.notifications().hook_failed(&name, now, &error)
                            })
                            .await?;
                        next = Some(next.map_or(at, |n| n.min(at)));
                        break;
                    }
                }
            }
        }
        Ok(next)
    }

    async fn send(&self, hook: &WebhookConfig, row: &OutboxRow) -> Result<(), String> {
        let res = self
            .client
            .post(&hook.url)
            .timeout(hook.timeout)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("X-Buckle-Event", &row.event)
            .header("X-Buckle-Delivery", &row.delivery)
            .header(
                "X-Hub-Signature-256",
                signature(&hook.secret, row.payload.as_bytes()),
            )
            .body(row.payload.clone())
            .send()
            .await
            .map_err(|e| {
                // reqwest's message names the URL; keep only the cause.
                let e = e.without_url();
                match std::error::Error::source(&e) {
                    Some(cause) => format!("{e}: {cause}"),
                    None => e.to_string(),
                }
            })?;
        let status = res.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(format!("the receiver answered {status}"))
        }
    }

    /// Sends until `cancel`, waking when the outbox gains events.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        if self.hooks.is_empty() {
            return;
        }
        let names: Vec<String> = self.hooks.iter().map(|h| h.name.clone()).collect();
        match self
            .store
            .call(move |db| db.notifications().forget_hooks_except(&names))
            .await
        {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                events = n,
                "dropped the queued events of webhooks no longer configured"
            ),
            Err(e) => tracing::warn!(error = %e, "the webhook outbox could not be tidied"),
        }
        tracing::info!(
            webhooks = self.hooks.len(),
            "sending attention events to webhooks"
        );
        loop {
            let now = self.clock.now();
            let next = match self.pass(now).await {
                Ok(next) => next,
                Err(e) => {
                    tracing::warn!(error = %e, "the webhook outbox could not be sent");
                    None
                }
            };
            let until = next
                .unwrap_or(now + Duration::seconds(IDLE))
                .min(now + Duration::seconds(IDLE));
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.wake.notified() => {}
                _ = self.clock.sleep_until(until) => {}
            }
        }
    }
}
