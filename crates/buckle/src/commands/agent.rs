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
    let system = std::sync::Arc::new(
        buckle_core::system::RealSystem::new()
            .with_http(std::sync::Arc::new(crate::http::ReqwestClient::new())),
    );
    buckle_agent::run_with_system(
        config,
        crate::telemetry::version(),
        system,
        super::shutdown_token(),
    )
    .await
}
