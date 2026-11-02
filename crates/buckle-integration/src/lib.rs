//! End-to-end test harness for Buckle: one real `buckle-server` and several
//! real `buckle-agent` sessions in one process.
//!
//! * [`World`] owns a temporary directory holding the server's data
//!   directory, the configuration directory its path source watches and the
//!   secrets root; a [`proxy::Proxy`] in front of the gRPC port; the server
//!   (restartable over the same data directory); and the agents.
//! * [`fleet::Fleet`] builds `buckle.yaml` and module trees.
//! * [`agent::AgentSpec`] describes an agent: its id, seed labels, drift
//!   settings and the [`buckle_core::testing::FakeSystem`] the production
//!   engine runner drives instead of the real host.
//! * [`api::Api`] is the operator API (CSRF handled, every body journaled),
//!   [`api::Sse`] the live event stream, [`logs`] process-wide log capture.
//!
//! Time is real: the server's HTTP front runs on actix worker threads and
//! everything talks over localhost sockets, which tokio's paused clock cannot
//! drive. Scenarios shorten the design's timers through configuration
//! (`settle`, `bake`, `local_rollback.after`, drift interval, backoff).

pub mod agent;
pub mod api;
pub mod fleet;
pub mod logs;
pub mod proxy;
pub mod server;
pub mod world;

pub use agent::{AgentSpec, AgentView};
pub use fleet::Fleet;
pub use world::{World, describe, phase, rollout_host};

use std::time::Duration;

/// How long a single wait may take before the test fails.
pub const WAIT: Duration = Duration::from_secs(20);

/// Links the standard library's registrations, installs the TLS provider and
/// the log capture. Idempotent.
pub fn init() {
    buckle_std::ensure_linked();
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    logs::init();
}

/// Polls `probe` every 50 ms until it returns `Ok`, failing the test with
/// the last `Err` after `timeout`.
pub async fn eventually<T>(
    what: &str,
    timeout: Duration,
    mut probe: impl AsyncFnMut() -> Result<T, String>,
) -> T {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let last = match probe().await {
            Ok(v) => return v,
            Err(e) => e,
        };
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out after {timeout:?} waiting for {what}; last observed:\n{last}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
