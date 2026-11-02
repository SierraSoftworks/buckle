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
    #[arg(long, conflicts_with = "ca_file")]
    pub ca_fingerprint: Option<String>,
    /// Trust this CA bundle (PEM) instead of the system roots.
    #[arg(long, value_name = "FILE")]
    pub ca_file: Option<PathBuf>,
    /// Allow a plaintext connection (only sensible over a tailnet).
    #[arg(long)]
    pub insecure_plaintext: bool,
    /// Where the agent keeps its state.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
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
                state_dir: a.state_dir.unwrap_or_else(|| {
                    if cfg!(windows) {
                        r"C:\ProgramData\buckle".into()
                    } else {
                        "/var/lib/buckle".into()
                    }
                }),
                bootstrap_token: a.token,
                tls: buckle_agent::config::TlsConfig {
                    ca_fingerprint: a.ca_fingerprint,
                    ca_file: a.ca_file,
                    insecure_plaintext: a.insecure_plaintext,
                },
                drift: Default::default(),
                vars: Default::default(),
                telemetry: Default::default(),
                local_secret_exec: Vec::new(),
            };
            config.validate()?;
            let options = buckle_agent::InstallOptions {
                no_service: a.no_service,
                exe: None,
            };
            buckle_agent::install(&config, &a.config, &options).await?;
            if a.no_service {
                println!(
                    "Wrote {}; start the agent with `buckle agent --config {}`.",
                    a.config.display(),
                    a.config.display()
                );
            } else {
                println!(
                    "Installed and started the {} service.",
                    buckle_agent::install::SERVICE_NAME
                );
            }
            Ok(0)
        }
    }
}
