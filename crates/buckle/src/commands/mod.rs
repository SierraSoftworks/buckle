//! The command-line interface. Each subcommand delegates to a library crate;
//! this crate only parses arguments and wires dependencies together.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

pub mod agent;
pub mod catalog;
pub mod install;
pub mod local;
pub mod server;
pub mod update;
#[cfg(windows)]
mod windows_service;

/// Lightweight fleet configuration with phased, health-gated rollouts.
#[derive(Parser, Debug)]
#[command(name = "buckle", version = crate::telemetry::version(), about, long_about = None, propagate_version = true)]
pub struct Cli {
    /// OpenTelemetry OTLP endpoint for traces, metrics and logs.
    #[arg(long, env = "OTEL_EXPORTER_OTLP_ENDPOINT", global = true)]
    pub otel_endpoint: Option<String>,

    /// Also write logs to stdout (always on for `server` and `agent`).
    #[arg(short, long, global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show what applying a configuration directory would change on this host.
    Plan(local::PlanArgs),
    /// Apply a configuration directory to this host.
    Apply(local::ApplyArgs),
    /// Show the last apply on this host: every module and step with its state
    /// and any errors.
    Status(local::StatusArgs),
    /// Run the health checks of the modules applied to this host.
    Health(local::HealthArgs),
    /// Print the facts collected about this host as JSON.
    Facts(local::FactsArgs),
    /// Validate a configuration directory without touching the host.
    Validate(local::ValidateArgs),
    /// Run the long-lived agent.
    Agent(agent::AgentArgs),
    /// Run the fleet server.
    Server(server::ServerArgs),
    /// Bootstrap this host as an agent: write agent.yaml, then install and
    /// start the agent service.
    Up(install::InstallAgentArgs),
    /// The former spelling of `up` (`install agent`), kept for existing scripts.
    #[command(hide = true)]
    Install(install::InstallArgs),
    /// List the task kinds, fact providers and health checks built into this binary.
    Catalog(catalog::CatalogArgs),
    /// Update this binary to the latest release.
    Update(UpdateArgs),
    /// Convert a Buckle 1.x package directory into a v2 module tree.
    Migrate(MigrateArgs),
}

#[derive(clap::Args, Debug)]
pub struct UpdateArgs {
    /// Install this release instead of the latest one.
    #[arg(long = "to", value_name = "VERSION")]
    pub to_version: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct MigrateArgs {
    /// The Buckle 1.x configuration directory (the one you gave `--config`).
    #[arg(value_name = "V1_DIR")]
    pub v1_dir: PathBuf,
    /// Where to write the v2 repository. Must be new or empty, unless `--force` is given.
    #[arg(value_name = "OUT_DIR")]
    pub out_dir: PathBuf,
    /// Replace buckle.yaml, vars/, modules/, facts.d/ and secrets.d/ in OUT_DIR.
    #[arg(long)]
    pub force: bool,
}

pub async fn run(cli: Cli) -> buckle_core::Result<i32> {
    match cli.command {
        Command::Plan(args) => local::plan(args).await,
        Command::Apply(args) => local::apply(args).await,
        Command::Status(args) => local::status(args).await,
        Command::Health(args) => local::health(args).await,
        Command::Facts(args) => local::facts(args).await,
        Command::Validate(args) => local::validate(args).await,
        Command::Agent(args) => agent::run(args).await,
        Command::Server(args) => server::run(args).await,
        Command::Up(args) => install::up(args).await,
        Command::Install(args) => install::run(args).await,
        Command::Catalog(args) => catalog::run(args),
        Command::Update(args) => update::run(args).await,
        Command::Migrate(args) => crate::migrate::run(args),
    }
}

/// A cancellation token that trips on SIGINT or SIGTERM.
pub fn shutdown_token() -> tokio_util::sync::CancellationToken {
    let token = tokio_util::sync::CancellationToken::new();
    let trip = token.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        tracing::warn!("shutdown requested");
        trip.cancel();
    });
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn subcommands_parse_their_documented_flags() {
        let cli = Cli::parse_from([
            "buckle",
            "plan",
            "--config",
            "/srv/cfg",
            "--labels",
            "role=worker,site=home",
            "--output",
            "json",
        ]);
        match cli.command {
            Command::Plan(args) => {
                assert_eq!(args.config, PathBuf::from("/srv/cfg"));
                assert_eq!(args.labels.get("site").map(String::as_str), Some("home"));
                assert_eq!(args.output, local::OutputFormat::Json);
            }
            other => panic!("unexpected {other:?}"),
        }
        let cli = Cli::parse_from(["buckle", "agent", "--config", "/etc/buckle/agent.yaml"]);
        assert!(matches!(cli.command, Command::Agent(_)));
        let args = [
            "--server",
            "https://b:7700",
            "--token",
            "t",
            "--label",
            "role=worker",
        ];
        let up = Cli::parse_from(["buckle", "up"].into_iter().chain(args));
        let Command::Up(up) = up.command else {
            panic!("`up` parses as Up");
        };
        // `install agent` is the old spelling of `up`, with the same flags.
        let legacy = Cli::parse_from(["buckle", "install", "agent"].into_iter().chain(args));
        let Command::Install(install::InstallArgs {
            what: install::InstallWhat::Agent(legacy),
        }) = legacy.command
        else {
            panic!("`install agent` still parses");
        };
        for a in [&up, &legacy] {
            assert_eq!(a.server.as_deref(), Some("https://b:7700"));
            assert_eq!(a.token.as_deref(), Some("t"));
            assert_eq!(a.labels, vec![("role".into(), "worker".into())]);
        }
    }
}
