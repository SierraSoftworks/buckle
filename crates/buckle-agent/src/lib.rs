//! The Buckle agent: a long-running process that connects to a server (or
//! watches a local directory), plans and applies assigned revisions, runs
//! health checks when asked, re-plans periodically for drift, and reports.
//!
//! * [`transport`] — the control plane: gRPC to a server, or a watched directory.
//! * [`session`] — the session loop, operations, drift, heartbeats and the
//!   local rollback watchdog.
//! * [`runner`] — the seam to the engine ([`runner::EngineRunner`]).
//! * [`install`] and [`update`] — `buckle up` and self-update.

pub mod backoff;
pub mod bundle;
pub mod cluster;
pub mod coalesce;
pub mod config;
pub mod identity;
pub mod install;
pub mod runner;
pub mod session;
pub mod sink;
pub mod store;
pub mod transport;
pub mod update;
pub mod watchdog;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;

pub use config::AgentConfig;
pub use install::{InstallOptions, InstallOutcome};
pub use runner::{RunContext, Runner};
pub use session::{Agent, AgentOptions};

use std::path::Path;
use std::sync::Arc;

/// Runs the agent until shutdown is requested. Returns the process exit code
/// (0 after a self-upgrade, so the service manager restarts the new binary).
pub async fn run(
    config: AgentConfig,
    shutdown: tokio_util::sync::CancellationToken,
) -> buckle_core::Result<i32> {
    run_with_version(config, env!("CARGO_PKG_VERSION"), shutdown).await
}

/// [`run`], reporting `agent_version` in Hello and to the updater.
pub async fn run_with_version(
    config: AgentConfig,
    agent_version: &str,
    shutdown: tokio_util::sync::CancellationToken,
) -> buckle_core::Result<i32> {
    run_with_system(
        config,
        agent_version,
        Arc::new(buckle_core::system::RealSystem::new()),
        shutdown,
    )
    .await
}

/// [`run_with_version`] over a caller-supplied [`buckle_core::system::System`],
/// which is how the binary attaches its HTTP client to the real host system.
pub async fn run_with_system(
    config: AgentConfig,
    agent_version: &str,
    system: Arc<dyn buckle_core::system::System>,
    shutdown: tokio_util::sync::CancellationToken,
) -> buckle_core::Result<i32> {
    config.validate()?;
    let transport: Arc<dyn transport::Transport> = match &config.source {
        Some(source) => {
            let store = store::Store::new(&config.state_dir);
            store.ensure()?;
            Arc::new(
                transport::local::LocalTransport::new(source.clone())
                    .with_secrets(store.install_key()?, config.local_secret_exec.clone()),
            )
        }
        None => Arc::new(transport::grpc::GrpcTransport::new(&config)?),
    };
    let store = store::Store::new(&config.state_dir);
    store.ensure()?;
    let state = Arc::new(store.state_store());
    let runner =
        Arc::new(runner::EngineRunner::new(system, state).with_scratch_dir(store.scratch_dir()));
    let updater = Arc::new(update::GitHubUpdater::new(agent_version));
    let options = AgentOptions {
        agent_version: agent_version.to_owned(),
        ..Default::default()
    };
    Agent::new(config, transport, runner, updater, options)?
        .run(shutdown)
        .await
}

/// Writes agent.yaml, creates the state directory and installs a service unit
/// for the current platform (unless `options.no_service`).
pub async fn install(
    config: &AgentConfig,
    config_path: &Path,
    options: &InstallOptions,
) -> buckle_core::Result<InstallOutcome> {
    install::install(config, config_path, options).await
}
