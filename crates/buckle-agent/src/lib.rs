//! The Buckle agent: a long-running process that connects to a server (or
//! watches a local directory), plans and applies assigned revisions, runs
//! health checks when asked, re-plans periodically for drift, and reports.
//!
//! Lane 3 (protocol and agent) owns this crate. The skeleton fixes the public
//! API the binary calls and the configuration file format.

pub mod config;
pub mod transport;

pub use config::AgentConfig;

use std::path::Path;

/// Runs the agent until shutdown is requested. Returns the process exit code.
pub async fn run(
    config: AgentConfig,
    shutdown: tokio_util::sync::CancellationToken,
) -> buckle_core::Result<i32> {
    let _ = (config, shutdown);
    Err(buckle_core::error::system(
        "The agent is not implemented yet.",
        &["This part of Buckle is still being built."],
    ))
}

/// Writes agent.yaml and installs a service unit for the current platform.
pub async fn install(config: &AgentConfig, config_path: &Path) -> buckle_core::Result<()> {
    let _ = (config, config_path);
    Err(buckle_core::error::system(
        "Agent installation is not implemented yet.",
        &["This part of Buckle is still being built."],
    ))
}
