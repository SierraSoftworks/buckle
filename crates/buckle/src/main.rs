//! `buckle`: one binary that plans, applies, runs as an agent or as the server.

mod commands;
mod http;
mod render;
mod telemetry;

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = commands::Cli::parse();
    let session = telemetry::session(&cli);
    let code = match commands::run(cli).await {
        Ok(code) => code,
        Err(err) => {
            if err.is(human_errors::Kind::System) {
                session.record_error(&err);
            }
            eprintln!("{}", human_errors::pretty(&err));
            1
        }
    };
    session.shutdown();
    std::process::exit(code);
}
