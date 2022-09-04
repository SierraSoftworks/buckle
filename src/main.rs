extern crate clap;
extern crate gtmpl;
#[macro_use]
extern crate tracing;

use crate::commands::CommandRunnable;
use clap::{crate_authors, value_parser, Arg, ArgMatches};
use gethostname::gethostname;
use opentelemetry::trace::{
    StatusCode, SpanKind,
};
use opentelemetry_otlp::WithExportConfig;
use std::sync::Arc;
use tracing::{field, instrument, metadata::LevelFilter, Span};
use tracing_subscriber::{prelude::*, registry};

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
        .subcommands(commands.iter().map(|x| x.app()))
        .arg(Arg::new("otel-endpoint")
            .help("The OpenTelemetry endpoint to send traces to. This endpoint must support the OTLP protocol.")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT")
            .long("otel-endpoint")
            .takes_value(true)
            .value_parser(value_parser!(String)))
        .arg(Arg::new("otel-headers")
            .help("The list of headers to send to the OTLP endpoint. Headers are specified as a comma-separated list of key=value pairs.")
            .env("OTEL_EXPORTER_OTLP_HEADERS")
            .long("otel-headers")
            .takes_value(true)
            .value_parser(value_parser!(String)));

    let matches = app.clone().get_matches();

    register_telemetry(&matches);

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

#[instrument(name = "app.host", fields(otel.name="buckle", otel.kind=%SpanKind::Client, otel.status=?StatusCode::Unset,exception=field::Empty, host.hostname=field::Empty, exit_code=field::Empty), skip(app, commands, matches))]
fn host<'a>(
    app: clap::App<'a>,
    commands: Vec<Arc<dyn CommandRunnable>>,
    matches: ArgMatches,
) -> Result<i32, errors::Error> {
    let command_name = format!("buckle {}", matches.subcommand_name().unwrap_or(""))
        .trim()
        .to_string();

    Span::current()
        .record("otel.name", command_name)
        .record("host.hostname", gethostname().to_string_lossy().trim());

    match run(commands, matches) {
        Ok(status@0) => {
            Span::current()
                .record("otel.status", &field::debug(StatusCode::Ok))
                .record("exit_code", &status);
            Ok(0)
        }
        Ok(status@2) => {
            app.clone().print_help().unwrap_or_default();
            Span::current()
                .record("otel.status", &field::debug(StatusCode::Ok))
                .record("exit_code", &status);
            Ok(0)
        }
        Ok(status) => {
            info!("Exiting with status code {}", status);
            Span::current()
                .record("otel.status", &field::debug(StatusCode::Error))
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

fn register_telemetry(matches: &ArgMatches) {
    match matches.get_one::<String>("otel-endpoint") {
        Some(otel_endpoint) if !otel_endpoint.is_empty() => {
            let mut metadata = tonic::metadata::MetadataMap::new();

            if let Some(headers) = matches.get_one::<String>("otel-headers") {
                let leaked_headers = Box::leak(headers.clone().into_boxed_str());
                leaked_headers.split_terminator(',').for_each(|x| {
                    if let Some((name, value)) = x.split_once('=') {
                        if let Some(value) = value.parse().ok() {
                            metadata.insert(name, value);
                        }
                    }
                });
            }

            let tracer = opentelemetry_otlp::new_pipeline()
                .tracing()
                .with_exporter(
                    opentelemetry_otlp::new_exporter()
                        .tonic()
                        .with_endpoint(otel_endpoint)
                        .with_metadata(metadata)
                        .with_env(),
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
                        !ctx.lookup_current()
                            // Exclude the rustls session "Connection" events which don't have a parent span
                            .map(|s| s.parent().is_none() && s.name() == "Connection")
                            .unwrap_or_default()
                    },
                ))
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .init();
        }
        _ => {
            let default_layer = tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(std::io::stderr);

            let registry = registry().with(LevelFilter::DEBUG).with(default_layer);

            tracing::subscriber::set_global_default(registry).unwrap();
        }
    }
}
