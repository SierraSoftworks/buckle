//! The live event bus the UI's SSE stream reads from.

use buckle_api::LiveEvent;
use tokio::sync::broadcast;

/// Fan-out of [`LiveEvent`]s. Slow subscribers lose old events rather than
/// stalling publishers.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<LiveEvent>,
}

impl std::fmt::Debug for EventBus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EventBus(..)")
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

impl EventBus {
    /// A bus buffering up to `capacity` events per subscriber.
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publishes an event; dropped when nobody listens.
    pub fn publish(&self, event: LiveEvent) {
        let _ = self.tx.send(event);
    }

    /// A new subscription.
    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.tx.subscribe()
    }
}
