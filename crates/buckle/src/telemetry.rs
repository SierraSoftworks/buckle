//! tracing-batteries session for the binary. OTLP export (traces and
//! metrics) goes to `--otel-endpoint` / the standard `OTEL_EXPORTER_OTLP_*`
//! environment variables, or, for `buckle server`, to `telemetry.otlp_endpoint`
//! and `telemetry.otlp_headers` in server.yaml when those are not set.

use tracing_batteries::{OpenTelemetry, Session};

pub fn session(cli: &crate::commands::Cli) -> Session {
    let file = match &cli.command {
        crate::commands::Command::Server(args) => {
            buckle_server::config::TelemetryConfig::peek(&args.config)
        }
        _ => None,
    }
    .unwrap_or_default();
    let endpoint = cli
        .otel_endpoint
        .clone()
        .or(file.otlp_endpoint)
        .unwrap_or_default();
    // Headers from OTEL_EXPORTER_OTLP_HEADERS win; with_header keeps them.
    let mut otel = OpenTelemetry::new(endpoint).with_metrics();
    for (key, value) in file.otlp_headers {
        otel = otel.with_header(key, value);
    }
    if cli.verbose {
        otel = otel.with_stdout(true);
    }
    Session::new("buckle", version())
        .with_context("host.os", std::env::consts::OS)
        .with_context("host.architecture", std::env::consts::ARCH)
        .with_battery(otel)
}

/// `0.0.0-dev` in debug builds, the crate version in release builds.
pub fn version() -> &'static str {
    if cfg!(debug_assertions) {
        "0.0.0-dev"
    } else {
        env!("CARGO_PKG_VERSION")
    }
}
