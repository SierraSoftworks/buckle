use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct AgentArgs {
    /// Path to agent.yaml.
    #[arg(short, long, env = "BUCKLE_AGENT_CONFIG", default_value = default_config_path())]
    pub config: PathBuf,
    /// In local mode (`source` in agent.yaml), let the `exec` secret provider
    /// run PROGRAM (repeatable; e.g. `op`). Off unless given.
    #[arg(long = "allow-secret-exec", value_name = "PROGRAM")]
    pub allow_secret_exec: Vec<String>,
}

fn default_config_path() -> &'static str {
    if cfg!(windows) {
        r"C:\ProgramData\buckle\agent.yaml"
    } else {
        "/etc/buckle/agent.yaml"
    }
}

pub async fn run(args: AgentArgs) -> buckle_core::Result<i32> {
    let mut config = buckle_agent::AgentConfig::load(&args.config)?;
    if !args.allow_secret_exec.is_empty() {
        if !config.is_local() {
            return Err(buckle_core::error::user(
                "--allow-secret-exec only applies to an agent watching a local directory.",
                &["Agents connected to a server get their secrets from it; remove the flag."],
            ));
        }
        config.local_secret_exec = args.allow_secret_exec.clone();
    }
    buckle_agent::run_with_version(config, crate::telemetry::version(), super::shutdown_token())
        .await
}
