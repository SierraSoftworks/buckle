//! tracing-batteries session for the binary. OTLP export is configured from the
//! standard `OTEL_EXPORTER_OTLP_*` environment variables (or `--otel-endpoint`).

use tracing_batteries::{OpenTelemetry, Session};

pub fn session(cli: &crate::commands::Cli) -> Session {
    let endpoint: &'static str = Box::leak(
        cli.otel_endpoint
            .clone()
            .unwrap_or_default()
            .into_boxed_str(),
    );
    let mut otel = OpenTelemetry::new(endpoint);
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
