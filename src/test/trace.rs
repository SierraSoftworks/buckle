use tracing::{dispatcher::DefaultGuard, metadata::LevelFilter};
use tracing_subscriber::prelude::*;

pub fn test_tracing() -> DefaultGuard {
    let default_layer = tracing_subscriber::fmt::layer()
        .with_ansi(true)
        .with_writer(std::io::stderr);

    let registry = tracing_subscriber::registry()
        .with(LevelFilter::DEBUG)
        .with(default_layer);

    tracing::subscriber::set_default(registry)
}
