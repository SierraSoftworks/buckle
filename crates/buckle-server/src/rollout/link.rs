//! The controller's view of connected agents.
//!
//! The controller never touches gRPC: it sends [`ServerMessage`]s through a
//! [`HostLink`] and is told about agent messages, connects and disconnects
//! through its handle. [`Sessions`] is the production link (one channel per
//! gRPC session); tests attach scripted fake hosts to the same type.

use buckle_proto::v1::ServerMessage;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;

/// Sends messages to connected hosts.
pub trait HostLink: Send + Sync + 'static {
    /// Queues `message` for `host`. Returns `false` when the host has no session.
    fn send(&self, host: &str, message: ServerMessage) -> bool;

    /// Whether `host` currently has a session.
    fn is_connected(&self, host: &str) -> bool;
}

struct Session {
    id: u64,
    tx: mpsc::UnboundedSender<ServerMessage>,
}

/// One outbound channel per connected host.
#[derive(Default)]
pub struct Sessions {
    next: AtomicU64,
    inner: Mutex<HashMap<String, Session>>,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions")
            .field("connected", &self.connected())
            .finish()
    }
}

impl Sessions {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a session for `host`, replacing (and so closing) any
    /// previous one. Returns the session id and the receiving end.
    pub fn attach(&self, host: &str) -> (u64, mpsc::UnboundedReceiver<ServerMessage>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = mpsc::unbounded_channel();
        if let Ok(mut map) = self.inner.lock() {
            map.insert(host.to_owned(), Session { id, tx });
        }
        (id, rx)
    }

    /// Removes the session if it is still the current one for `host`.
    /// Returns whether it was.
    pub fn detach(&self, host: &str, session: u64) -> bool {
        match self.inner.lock() {
            Ok(mut map) => {
                if map.get(host).is_some_and(|s| s.id == session) {
                    map.remove(host);
                    true
                } else {
                    false
                }
            }
            Err(_) => false,
        }
    }

    /// Hosts with a session.
    pub fn connected(&self) -> Vec<String> {
        self.inner
            .lock()
            .map(|m| {
                let mut v: Vec<String> = m.keys().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }
}

impl HostLink for Sessions {
    fn send(&self, host: &str, message: ServerMessage) -> bool {
        match self.inner.lock() {
            Ok(map) => map.get(host).is_some_and(|s| s.tx.send(message).is_ok()),
            Err(_) => false,
        }
    }

    fn is_connected(&self, host: &str) -> bool {
        self.inner
            .lock()
            .map(|m| m.get(host).is_some_and(|s| !s.tx.is_closed()))
            .unwrap_or(false)
    }
}
