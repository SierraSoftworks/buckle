use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    /// Path to server.yaml.
    #[arg(
        short,
        long,
        env = "BUCKLE_SERVER_CONFIG",
        default_value = "server.yaml"
    )]
    pub config: PathBuf,
}

pub async fn run(args: ServerArgs) -> buckle_core::Result<i32> {
    let config = buckle_server::ServerConfig::load(&args.config)?;
    buckle_server::run(config, super::shutdown_token()).await
}
