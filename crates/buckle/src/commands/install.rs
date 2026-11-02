use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct InstallArgs {
    #[command(subcommand)]
    pub what: InstallWhat,
}

#[derive(Subcommand, Debug, Clone)]
pub enum InstallWhat {
    /// Install the agent: write agent.yaml and a service unit, then start it.
    Agent(InstallAgentArgs),
}

#[derive(Args, Debug, Clone)]
pub struct InstallAgentArgs {
    /// Server URL, e.g. https://buckle.example.ts.net:7700.
    #[arg(long, conflicts_with = "source")]
    pub server: Option<String>,
    /// Local configuration directory to watch instead of a server.
    #[arg(long)]
    pub source: Option<PathBuf>,
    /// Bootstrap token for enrolment (not needed over Tailscale).
    #[arg(long)]
    pub token: Option<String>,
    /// Pin the server certificate by fingerprint (sha256:…).
    #[arg(long)]
    pub ca_fingerprint: Option<String>,
    /// Seed labels, repeatable: --label role=worker.
    #[arg(long = "label", value_parser = parse_label)]
    pub labels: Vec<(String, String)>,
    /// Host identity override.
    #[arg(long)]
    pub host_id: Option<String>,
    /// Where to write agent.yaml.
    #[arg(long, default_value = default_config_path())]
    pub config: PathBuf,
    /// Write the files but do not install or start the service.
    #[arg(long)]
    pub no_service: bool,
}

fn default_config_path() -> &'static str {
    if cfg!(windows) {
        r"C:\ProgramData\buckle\agent.yaml"
    } else {
        "/etc/buckle/agent.yaml"
    }
}

fn parse_label(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .ok_or_else(|| format!("label '{s}' must be key=value"))
}

pub async fn run(args: InstallArgs) -> buckle_core::Result<i32> {
    match args.what {
        InstallWhat::Agent(a) => {
            let config = buckle_agent::AgentConfig {
                server: a.server,
                source: a.source,
                host_id: a.host_id,
                labels: a.labels.into_iter().collect(),
                state_dir: if cfg!(windows) {
                    r"C:\ProgramData\buckle".into()
                } else {
                    "/var/lib/buckle".into()
                },
                bootstrap_token: a.token,
                tls: buckle_agent::config::TlsConfig {
                    ca_fingerprint: a.ca_fingerprint,
                    ..Default::default()
                },
                drift: Default::default(),
                vars: Default::default(),
                telemetry: Default::default(),
            };
            config.validate()?;
            buckle_agent::install(&config, &a.config).await?;
            Ok(0)
        }
    }
}
