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

    /// Print the SHA-256 fingerprint of the gRPC TLS certificate (generating
    /// it in `auto` mode) for `buckle install agent --ca-fingerprint`, then exit.
    #[arg(long)]
    pub print_fingerprint: bool,
}

pub async fn run(args: ServerArgs) -> buckle_core::Result<i32> {
    let config = buckle_server::ServerConfig::load(&args.config)?;
    if args.print_fingerprint {
        return match buckle_server::fingerprint(&config)? {
            Some(fp) => {
                println!("sha256:{fp}");
                Ok(0)
            }
            None => Err(buckle_core::error::user(
                "The gRPC listener has TLS disabled (listen.tls.mode: none), so there is no fingerprint.",
                &["Set listen.tls.mode to auto or files."],
            )),
        };
    }
    buckle_server::run(config, super::shutdown_token()).await
}
