//! The Buckle server: watches a Git repository, turns commits into revisions,
//! orchestrates phased rollouts across the fleet, speaks gRPC to agents and
//! serves the HTTP API and the embedded UI to operators.

pub mod clock;
pub mod config;
pub mod events;
pub mod fleet;
pub mod grpc;
pub mod rollout;
pub mod secrets;
pub mod source;
pub mod store;
pub mod views;
pub mod web;

pub use config::ServerConfig;

/// Runs the server until shutdown is requested. Returns the process exit code.
pub async fn run(
    config: ServerConfig,
    shutdown: tokio_util::sync::CancellationToken,
) -> buckle_core::Result<i32> {
    let _ = (config, shutdown);
    Err(buckle_core::error::system(
        "The server is not implemented yet.",
        &["This part of Buckle is still being built."],
    ))
}
