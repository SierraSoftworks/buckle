extern crate clap;
extern crate gtmpl;
#[macro_use]
extern crate tracing;

use crate::commands::CommandRunnable;
use clap::{crate_authors, value_parser, Arg, ArgMatches, ArgAction};
use gethostname::gethostname;
use std::sync::Arc;
use tracing::{field, instrument, Span};
use tracing_batteries::prelude::opentelemetry::trace::SpanKind;
use tracing_batteries::{OpenTelemetry, OpenTelemetryLevel, Session};

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

    let telemetry = register_telemetry(&matches);

    std::process::exit(match host(app, commands, matches) {
        Ok(status) => {
            telemetry.shutdown();
            status
        }
        Err(_err) => {
            telemetry.shutdown();
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
fn register_telemetry(matches: &ArgMatches) -> Session {
    // In release builds the endpoint is sourced from the --otel-endpoint flag (or the
    // OTEL_EXPORTER_OTLP_ENDPOINT environment variable it is bound to). An empty endpoint
    // disables OTLP export and falls back to logging on stdout.
    #[cfg(not(debug_assertions))]
    let endpoint = matches
        .get_one::<String>("otel-endpoint")
        .cloned()
        .unwrap_or_default();

    // Debug builds default to shipping traces to the shared Honeycomb environment.
    #[cfg(debug_assertions)]
    let endpoint = "https://api.honeycomb.io:443".to_string();

    let telemetry = OpenTelemetry::new(endpoint).with_default_level(OpenTelemetryLevel::DEBUG);

    // Additional OTLP headers are read automatically from OTEL_EXPORTER_OTLP_HEADERS; in debug
    // builds we also attach the Honeycomb API key used by the shared environment.
    #[cfg(debug_assertions)]
    let telemetry = telemetry.with_header("x-honeycomb-team", "X6naTEMkzy10PMiuzJKifF");

    Session::new("buckle", version!("v"))
        .with_context("host.os", std::env::consts::OS)
        .with_context("host.architecture", std::env::consts::ARCH)
        .with_battery(telemetry)
}
