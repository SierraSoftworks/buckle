//! tracing-batteries session for the binary. OTLP export (traces and
//! metrics) goes to `--otel-endpoint` / the standard `OTEL_EXPORTER_OTLP_*`
//! environment variables or, when those are not set, to
//! `telemetry.otlp_endpoint` and `telemetry.otlp_headers` from server.yaml
//! (`buckle server`) or agent.yaml (`buckle agent`).

use tracing_batteries::{OpenTelemetry, Session};

/// The agent's telemetry section, when the command is `buckle agent` and its
/// agent.yaml can be read (a broken file is reported by the command itself).
fn agent_telemetry(cli: &crate::commands::Cli) -> Option<buckle_agent::config::TelemetryConfig> {
    match &cli.command {
        crate::commands::Command::Agent(args) => buckle_agent::AgentConfig::load(&args.config)
            .ok()
            .map(|c| c.telemetry),
        _ => None,
    }
}

/// The endpoint and headers to export to: the flag (or
/// `OTEL_EXPORTER_OTLP_ENDPOINT`) first, then agent.yaml.
fn otlp_target(
    flag: Option<&str>,
    file: Option<&buckle_agent::config::TelemetryConfig>,
) -> (String, Vec<(String, String)>) {
    let endpoint = flag
        .map(str::to_owned)
        .or_else(|| file.and_then(|t| t.otlp_endpoint.clone()))
        .unwrap_or_default();
    let headers = file
        .map(|t| {
            t.otlp_headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    (endpoint, headers)
}

pub fn session(cli: &crate::commands::Cli) -> Session {
    let (endpoint, headers) = match &cli.command {
        crate::commands::Command::Server(args) => {
            let file =
                buckle_server::config::TelemetryConfig::peek(&args.config).unwrap_or_default();
            (
                cli.otel_endpoint
                    .clone()
                    .or(file.otlp_endpoint)
                    .unwrap_or_default(),
                file.otlp_headers.into_iter().collect(),
            )
        }
        _ => otlp_target(cli.otel_endpoint.as_deref(), agent_telemetry(cli).as_ref()),
    };
    // Headers from OTEL_EXPORTER_OTLP_HEADERS win; with_header keeps them.
    let mut otel = OpenTelemetry::new(endpoint).with_metrics();
    for (key, value) in headers {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_wins_over_agent_yaml_and_headers_come_from_the_file() {
        let file = buckle_agent::config::TelemetryConfig {
            otlp_endpoint: Some("https://collector.example:4317".into()),
            otlp_headers: [("x-honeycomb-team".to_string(), "key".to_string())]
                .into_iter()
                .collect(),
        };
        let (endpoint, headers) = otlp_target(None, Some(&file));
        assert_eq!(endpoint, "https://collector.example:4317");
        assert_eq!(headers, vec![("x-honeycomb-team".into(), "key".into())]);
        let (endpoint, _) = otlp_target(Some("http://flag:4317"), Some(&file));
        assert_eq!(endpoint, "http://flag:4317");
        assert_eq!(otlp_target(None, None).0, "");
    }
}
