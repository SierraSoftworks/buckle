//! Rollout timelines on the live stream.
//!
//! The controller (and the reject route) record timeline entries as audit
//! rows whose subject is the rollout, and publish an [`LiveEvent::Audit`]
//! reference for each. This relay turns those references into
//! [`LiveEvent::RolloutTimeline`] events carrying the entry as the rollout
//! page shows it, so the page appends instead of refetching.

use crate::events::EventBus;
use crate::store::Store;
use buckle_api::LiveEvent;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::sync::CancellationToken;

/// Starts the relay; it stops with `cancel` or when the bus closes.
pub fn spawn(
    store: Store,
    events: EventBus,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let mut rx = events.subscribe();
    tokio::spawn(async move {
        loop {
            let received = tokio::select! {
                _ = cancel.cancelled() => return,
                r = rx.recv() => r,
            };
            let id = match received {
                Ok(LiveEvent::Audit { entry }) => entry.id,
                Ok(_) => continue,
                // Streams that lagged tell their clients to refetch.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            match store.call(move |db| db.audit().rollout_entry(id)).await {
                Ok(Some(entry)) => {
                    if let Some(rollout_id) = entry.subject.clone() {
                        events.publish(LiveEvent::RolloutTimeline {
                            rollout_id,
                            event: crate::views::rollout_event(&entry),
                        });
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(error = %e, "a timeline entry could not be read"),
            }
        }
    })
}
