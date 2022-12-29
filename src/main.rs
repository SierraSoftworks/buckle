extern crate clap;
extern crate gtmpl;
#[macro_use]
extern crate tracing;

use crate::commands::CommandRunnable;
use clap::{crate_authors, value_parser, Arg, ArgMatches, ArgAction};
use gethostname::gethostname;
use opentelemetry::trace::SpanKind;
use opentelemetry_otlp::WithExportConfig;
use std::sync::Arc;
use tracing::{field, instrument, Span, Collect};
use tracing_subscriber::{prelude::*, registry::LookupSpan, Subscribe};

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

    let app = clap::Command::new("buckle")
        .version(version!("v"))
        .author(crate_authors!("\n"))
        .about("Taking care of your bootstrapping needs")
        .subcommands(commands.iter().map(|x| x.app()))
        .arg(Arg::new("otel-endpoint")
            .help("The OpenTelemetry endpoint to send traces to. This endpoint must support the OTLP protocol.")
            .env("OTEL_EXPORTER_OTLP_ENDPOINT")
            .long("otel-endpoint")
            .action(ArgAction::Set)
            .value_parser(value_parser!(String)))
        .arg(Arg::new("otel-headers")
            .help("The list of headers to send to the OTLP endpoint. Headers are specified as a comma-separated list of key=value pairs.")
            .env("OTEL_EXPORTER_OTLP_HEADERS")
            .long("otel-headers")
            .action(ArgAction::Set)
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

#[instrument(name = "app.host", fields(otel.name="buckle", otel.kind=?SpanKind::Client, exception=field::Empty, host.hostname=field::Empty, exit_code=field::Empty), skip(app, commands, matches), ret, err)]
fn host(
    app: clap::Command,
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
                .record("exit_code", status);
            Ok(0)
        }
        Ok(status@2) => {
            app.clone().print_help().unwrap_or_default();
            Span::current()
                .record("exit_code", status);
            Ok(0)
        }
        Ok(status) => {
            info!("Exiting with status code {}", status);
            Span::current()
                .record("exit_code", status);
            Ok(status)
        }
        Err(error) => {
            println!("{error}");

            error!("Exiting with status code {}", 1);
            Span::current()
                .record("exit_code", 1_u32);

            if error.is_system() {
                Span::current().record("exception", &field::display(&error));
            } else {
                Span::current().record("exception", &error.description());
            }

            Err(error)
        }
    }
}

#[instrument(name = "app.run", fields(otel.kind = ?SpanKind::Client), skip(commands, matches), ret, err)]
fn run(
    commands: Vec<Arc<dyn CommandRunnable>>,
    matches: ArgMatches,
) -> Result<i32, errors::Error> {
    for cmd in commands.iter() {
        if let Some(cmd_matches) = matches.subcommand_matches(&cmd.name()) {
            return cmd.run(cmd_matches);
        }
    }

    Ok(2)
}

#[allow(unused_variables)]
fn register_telemetry(matches: &ArgMatches) {
    #[cfg(not(debug_assertions))]
    let tracing_endpoint = matches.get_one::<String>("otel-endpoint").cloned();

    #[cfg(debug_assertions)]
    let tracing_endpoint = Some("https://api.honeycomb.io:443".to_string());

    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::LevelFilter::DEBUG)
        .with(tracing_subscriber::filter::dynamic_filter_fn(
            |_metadata, ctx| {
                !ctx.lookup_current()
                    // Exclude the rustls session "Connection" events which don't have a parent span
                    .map(|s| s.parent().is_none() && s.name() == "Connection")
                    .unwrap_or_default()
            },
        ))
        .with(load_output_layer(tracing_endpoint))
        .init();
}

fn load_otlp_headers() -> tonic::metadata::MetadataMap {
    let mut tracing_metadata = tonic::metadata::MetadataMap::new();

    #[cfg(debug_assertions)]
    tracing_metadata.insert(
        "x-honeycomb-team",
        "X6naTEMkzy10PMiuzJKifF".parse().unwrap(),
    );

    match std::env::var("OTEL_EXPORTER_OTLP_HEADERS").ok() {
        Some(headers) if !headers.is_empty() => {
            for header in headers.split_terminator(',') {
                if let Some((key, value)) = header.split_once('=') {
                    let key: &str = Box::leak(key.to_string().into_boxed_str());
                    let value = value.to_owned();
                    if let Ok(value) = value.parse() {
                        tracing_metadata.insert(key, value);
                    } else {
                        eprintln!("Could not parse value for header {key}.");
                    }
                }
            }
        }
        _ => {}
    }

    tracing_metadata
}

fn load_output_layer<S>(tracing_endpoint: Option<String>) -> Box<dyn Subscribe<S> + Send + Sync + 'static> 
where S: Collect + Send + Sync,
for<'a> S: LookupSpan<'a>,
{
    if let Some(endpoint) = tracing_endpoint {
        let metadata = load_otlp_headers();
        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint(endpoint)
                    .with_metadata(metadata),
            )
            .with_trace_config(opentelemetry::sdk::trace::config().with_resource(
                opentelemetry::sdk::Resource::new(vec![
                    opentelemetry::KeyValue::new("service.name", "buckle"),
                    opentelemetry::KeyValue::new("service.version", version!("v")),
                    opentelemetry::KeyValue::new("host.os", std::env::consts::OS),
                    opentelemetry::KeyValue::new("host.architecture", std::env::consts::ARCH),
                ]),
            ))
            .install_batch(opentelemetry::runtime::Tokio)
            .unwrap();

        tracing_opentelemetry::subscriber()
            .with_tracer(tracer)
            .boxed()
    } else {
        tracing_subscriber::fmt::subscriber()
            .boxed()
    }
}
