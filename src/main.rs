extern crate clap;
extern crate gtmpl;
extern crate tracing;

use crate::commands::CommandRunnable;
use clap::{crate_authors, App, Arg, ArgMatches};
use opentelemetry::{sdk, KeyValue, trace::SpanKind};
use tracing::{error, field, info_span, instrument, metadata::LevelFilter};
use tracing_subscriber::{prelude::*, registry};
use std::sync::Arc;
use opentelemetry_otlp::WithExportConfig;
use tonic::{metadata::*};

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

    let app = App::new("buckle")
        .version(version.as_str())
        .author(crate_authors!("\n"))
        .about("Taking care of your bootstrapping needs")
        
        .arg(Arg::new("appinsights-key")
                .long("appinsights-key")
                .env("APPINSIGHTS_INSTRUMENTATIONKEY")
                .about("The Application Insights API key which should be used to report telemetry.")
                .takes_value(true)
                .conflicts_with_all(&vec!["honeycomb-key", "honeycomb-dataset"])
                .global(true)
                .requires("appinsights-endpoint"))
        .arg(Arg::new("appinsights-endpoint")
                .long("appinsights-endpoint")
                .env("APPINSIGHTS_ENDPOINT")
                .about("The Application Insights API endpoint which should be used to report telemetry.")
                .global(true)
                .takes_value(true))
        
        .arg(Arg::new("honeycomb-key")
                .long("honeycomb-key")
                .env("HONEYCOMB_APIKEY")
                .about("The Honeycomb API key which should be used to report telemetry.")
                .global(true)
                .takes_value(true))
        .arg(Arg::new("honeycomb-dataset")
                .long("honeycomb-dataset")
                .env("HONEYCOMB_DATASET")
                .about("The Honeycomb dataset which should be used to report telemetry.")
                .takes_value(true)
                .global(true)
                .default_value("buckle"))
        .subcommands(commands.iter().map(|x| x.app()));

    let matches = app.clone().get_matches();

    register_telemetry(
        matches.value_of("appinsights-key"), 
        matches.value_of("appinsights-endpoint"), 
        matches.value_of("honeycomb-key"), 
        matches.value_of("honeycomb-dataset"));

    let result = {
        let span = info_span!("app.main", otel.kind=%SpanKind::Client, exit_code = field::Empty);

        span.in_scope(|| match run(app, commands, matches) {
            Result::Ok(status) => {
                span.record("exit_code", &status);

                Ok(())
            }
            Result::Err(err) => {
                span.record("exit_code", &1);
                error!("{}", err.message());
                Err(err)
            }
        })
    };

    opentelemetry::global::shutdown_tracer_provider();

    result
}

#[instrument(name = "app.run", fields(otel.kind = %SpanKind::Client), skip(app, commands, matches), err)]
fn run<'a>(
    mut app: App<'a>,
    commands: Vec<Arc<dyn CommandRunnable>>,
    matches: ArgMatches,
) -> Result<i32, errors::Error> {
    for cmd in commands.iter() {
        if let Some(cmd_matches) = matches.subcommand_matches(cmd.name()) {
            return cmd.run(cmd_matches);
        }
    }

    app.print_help().unwrap_or_default();
    Ok(-1)
}

fn register_telemetry(
    appinsights_key: Option<&str>,
    appinsights_endpoint: Option<&str>,
    honeycomb_key: Option<&str>,
    honeycomb_dataset: Option<&str>
) {
    match (appinsights_key, appinsights_endpoint, honeycomb_key, honeycomb_dataset) {
        (Some(appinsights_key), Some(appinsights_endpoint), _, _) if !appinsights_key.is_empty() && !appinsights_endpoint.is_empty() => {
            let tracer = opentelemetry_application_insights::new_pipeline(appinsights_key.to_string())
                .with_service_name("buckle")
                .with_client(reqwest::blocking::Client::new())
                .with_trace_config(sdk::trace::config().with_resource(sdk::Resource::new(
                    vec![
                        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                    ],
                )))
                .with_endpoint(appinsights_endpoint).unwrap()
                .install_batch(opentelemetry::runtime::Tokio);

            let layer = tracing_opentelemetry::layer()
                .with_tracer(tracer);

            let default_layer = tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(std::io::stderr);

            let registry = registry()
                .with(LevelFilter::DEBUG)
                .with(layer)
                .with(default_layer);

            tracing::subscriber::set_global_default(registry).unwrap();
        },
        (_, _, Some(honeycomb_key), Some(honeycomb_dataset)) if !honeycomb_key.is_empty() && !honeycomb_dataset.is_empty() => {
            let mut metadata_map = MetadataMap::new();
            metadata_map.insert("x-honeycomb-team", honeycomb_key.parse().unwrap());
            metadata_map.insert("x-honeycomb-dataset", honeycomb_dataset.parse().unwrap());

            let mut tls_config = rustls::ClientConfig::new();
            tls_config.root_store.add_server_trust_anchors(&webpki_roots::TLS_SERVER_ROOTS);
            tls_config.set_protocols(&["h2".into()]);

            let exporter = opentelemetry_otlp::new_exporter()
                .tonic()
                .with_endpoint("https://api.honeycomb.io:443")
                .with_metadata(metadata_map)
                .with_tls_config(tonic::transport::channel::ClientTlsConfig::new().rustls_client_config(tls_config));

            let tracer = opentelemetry_otlp::new_pipeline()
                .tracing()
                .with_exporter(exporter)
                .with_trace_config(sdk::trace::config().with_resource(sdk::Resource::new(
                        vec![
                            KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
                        ],
                    )))
                .install_batch(opentelemetry::runtime::Tokio)
                .unwrap();

                let layer = tracing_opentelemetry::layer()
                .with_tracer(tracer);

            let default_layer = tracing_subscriber::fmt::layer()
                .with_ansi(true)
                .with_writer(std::io::stderr);

            let registry = registry()
                .with(LevelFilter::DEBUG)
                .with(layer)
                .with(default_layer);

            tracing::subscriber::set_global_default(registry).unwrap();
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