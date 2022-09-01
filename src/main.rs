extern crate clap;
extern crate gtmpl;
#[macro_use] extern crate tracing;

use crate::commands::CommandRunnable;
use clap::{crate_authors, Arg, ArgMatches};
use opentelemetry::trace::{SpanKind, StatusCode, TraceContextExt};
use tracing::{field, instrument, metadata::LevelFilter, Span};
use tracing_subscriber::{prelude::*, registry};
use std::sync::Arc;
use opentelemetry_otlp::WithExportConfig;
use gethostname::gethostname;
use tracing_opentelemetry::OpenTelemetrySpanExt;

#[macro_use]
mod macros;
mod commands;
mod core;
mod errors;
#[cfg(test)]
mod test;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let commands = commands::commands();
    let version = version!("v");

    let app = clap::Command::new("buckle")
        .version(version.as_str())
        .author(crate_authors!("\n"))
        .about("Taking care of your bootstrapping needs")
        
        .arg(Arg::new("appinsights-key")
                .long("appinsights-key")
                .env("APPINSIGHTS_INSTRUMENTATIONKEY")
                .help("The Application Insights API key which should be used to report telemetry.")
                .takes_value(true)
                .conflicts_with_all(&["honeycomb-key", "honeycomb-dataset"])
                .global(true)
                .requires("appinsights-endpoint"))
        .arg(Arg::new("appinsights-endpoint")
                .long("appinsights-endpoint")
                .env("APPINSIGHTS_ENDPOINT")
                .help("The Application Insights API endpoint which should be used to report telemetry.")
                .global(true)
                .takes_value(true))
        
        .arg(Arg::new("honeycomb-key")
                .long("honeycomb-key")
                .env("HONEYCOMB_APIKEY")
                .help("The Honeycomb API key which should be used to report telemetry.")
                .global(true)
                .takes_value(true))
        .arg(Arg::new("honeycomb-dataset")
                .long("honeycomb-dataset")
                .env("HONEYCOMB_DATASET")
                .help("The Honeycomb dataset which should be used to report telemetry.")
                .takes_value(true)
                .global(true)
                .default_value("buckle"))
        .subcommands(commands.iter().map(|x| x.app()));

    let matches = app.clone().get_matches();

    register_telemetry(
        matches.value_of("honeycomb-team"),);

    std::process::exit(match host(app, commands, matches) {
        Ok(status) => {
            opentelemetry::global::shutdown_tracer_provider();
            status
        }
        Err(_err) => {
            opentelemetry::global::shutdown_tracer_provider();
            1
        }
    });
}

#[instrument(name = "app.host", fields(otel.name="buckle", otel.kind=%SpanKind::Client, otel.status=?StatusCode::Unset,exception=field::Empty, host.hostname=field::Empty, exit_code=field::Empty), skip(app, commands))]
fn host<'a>(app: clap::App<'a>, commands: Vec<Arc<dyn CommandRunnable>>, matches: ArgMatches) -> Result<i32, errors::Error> {
    let command_name = format!("buckle {}", matches.subcommand_name().unwrap_or(""))
        .trim()
        .to_string();

    Span::current()
        .record("otel.name", command_name)
        .record("host.hostname", gethostname().to_string_lossy().trim());

    match run(commands, matches) {
        Ok(2) => {
            app.clone().print_help().unwrap_or_default();

            Span::current()
                .record("otel.status", &field::debug(StatusCode::Error))
                .record("exit_code", &2_u32);

            warn!("Exiting with status code {}", 2);
            Ok(2)
        }
        Ok(status) => {
            info!("Exiting with status code {}", status);
            Span::current()
                .record("otel.status", &field::debug(StatusCode::Ok))
                .record("exit_code", &status);
            Ok(status)
        }
        Err(error) => {
            println!("{}", error);

            error!("Exiting with status code {}", 1);
            Span::current()
                .record("otel.status", &field::debug(StatusCode::Error))
                .record("exit_code", &1_u32);

            if error.is_system() {
                Span::current().record("exception", &field::display(&error));
            } else {
                Span::current().record("exception", &error.description());
            }

            println!(
                "Trace ID: {:032x}",
                Span::current()
                    .context()
                    .span()
                    .span_context()
                    .trace_id()
            );

            Err(error)
        }
    }
}

#[instrument(name = "app.run", fields(otel.kind = %SpanKind::Client), skip(commands, matches), err)]
fn run<'a>(
    commands: Vec<Arc<dyn CommandRunnable>>,
    matches: ArgMatches,
) -> Result<i32, errors::Error> {
    for cmd in commands.iter() {
        if let Some(cmd_matches) = matches.subcommand_matches(cmd.name()) {
            return cmd.run(cmd_matches);
        }
    }

    Ok(2)
}

fn register_telemetry(
    honeycomb_team: Option<&str>,
) {
    match honeycomb_team {
        Some(honeycomb_team) if !honeycomb_team.is_empty() => {

            let mut tracing_metadata = tonic::metadata::MetadataMap::new();
            tracing_metadata.insert(
                "x-honeycomb-team",honeycomb_team.parse().unwrap()
            );

            let tracer = opentelemetry_otlp::new_pipeline()
                .tracing()
                .with_exporter(
                    opentelemetry_otlp::new_exporter()
                        .tonic()
                        .with_endpoint("https://api.honeycomb.io:443")
                        .with_metadata(tracing_metadata),
                )
                .with_trace_config(opentelemetry::sdk::trace::config().with_resource(
                    opentelemetry::sdk::Resource::new(vec![
                        opentelemetry::KeyValue::new("service.name", "buckle"),
                        opentelemetry::KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                        opentelemetry::KeyValue::new("host.os", std::env::consts::OS),
                        opentelemetry::KeyValue::new("host.architecture", std::env::consts::ARCH),
                    ]),
                ))
                .install_batch(opentelemetry::runtime::Tokio)
                .unwrap();

            tracing_subscriber::registry()
                .with(LevelFilter::DEBUG)
                .with(tracing_subscriber::filter::dynamic_filter_fn(
                    |_metadata, ctx| {
                        !ctx
                            .lookup_current()
                            // Exclude the rustls session "Connection" events which don't have a parent span
                            .map(|s| s.parent().is_none() && s.name() == "Connection")
                            .unwrap_or_default()
                    },
                ))
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .init();
        },
        _ => {
            let default_layer = tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(std::io::stderr);
                
            let registry = registry()
                .with(LevelFilter::DEBUG)
                .with(default_layer);

            tracing::subscriber::set_global_default(registry).unwrap();
        }
    }
}