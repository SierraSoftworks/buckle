//! Where reports go: the current session's report channel, swapped on every
//! reconnect. Reports sent while no session is open are dropped (the server
//! re-learns the host's state from the next Hello), except local rollbacks,
//! which are persisted and replayed after the next Welcome.

use crate::transport::Report;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// A handle to the current session's report channel.
#[derive(Clone, Default)]
pub struct ReportSink {
    inner: Arc<Mutex<Option<mpsc::Sender<Report>>>>,
}

impl ReportSink {
    /// Routes reports to a new session.
    pub fn attach(&self, tx: mpsc::Sender<Report>) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(tx);
        }
    }

    /// Stops routing reports (the session ended).
    pub fn detach(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            *guard = None;
        }
    }

    /// Sends a report to the current session; `false` when it was dropped.
    pub async fn send(&self, report: Report) -> bool {
        let tx = self.inner.lock().ok().and_then(|g| g.clone());
        match tx {
            Some(tx) => tx.send(report).await.is_ok(),
            None => {
                tracing::debug!("no session is open; dropping a report");
                false
            }
        }
    }
}
