use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct AgentArgs {
    /// Path to agent.yaml.
    #[arg(short, long, env = "BUCKLE_AGENT_CONFIG", default_value = default_config_path())]
    pub config: PathBuf,
}

fn default_config_path() -> &'static str {
    if cfg!(windows) {
        r"C:\ProgramData\buckle\agent.yaml"
    } else {
        "/etc/buckle/agent.yaml"
    }
}

pub async fn run(args: AgentArgs) -> buckle_core::Result<i32> {
    let config = buckle_agent::AgentConfig::load(&args.config)?;
    buckle_agent::run_with_version(config, crate::telemetry::version(), super::shutdown_token())
        .await
}
