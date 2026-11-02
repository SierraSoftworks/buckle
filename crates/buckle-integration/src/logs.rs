//! Process-wide log capture.
//!
//! The server, the agents and the engine all run in the test process, on
//! tokio worker threads and actix worker threads, so a thread-local
//! subscriber would miss most of them. One global subscriber formats every
//! event from a `buckle*` target (all levels, with span fields) and every
//! other event at INFO or above into a shared buffer. Tests that care about
//! what was logged (the secret leak checks) search the whole buffer for
//! values that only they know.
//!
//! Set `BUCKLE_IT_LOG=1` to also echo the captured lines to stderr.

use std::io::Write;
use std::sync::{Mutex, OnceLock};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

static BUFFER: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static INIT: OnceLock<bool> = OnceLock::new();

struct CaptureWriter {
    echo: bool,
}

impl Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        BUFFER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        if self.echo {
            let _ = std::io::stderr().write_all(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the capturing subscriber once per process.
pub fn init() {
    INIT.get_or_init(|| {
        let echo = std::env::var_os("BUCKLE_IT_LOG").is_some();
        let filter = tracing_subscriber::filter::filter_fn(|meta| {
            meta.target().starts_with("buckle") || *meta.level() <= tracing::Level::INFO
        });
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_thread_names(true)
            .with_writer(move || CaptureWriter { echo })
            .with_filter(filter);
        tracing_subscriber::registry()
            .with(layer)
            .try_init()
            .is_ok()
    });
}

/// Everything captured so far, from every test in the process.
pub fn snapshot() -> String {
    String::from_utf8_lossy(&BUFFER.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
}

/// Whether any captured line contains `needle`.
pub fn contains(needle: &str) -> bool {
    let buffer = BUFFER.lock().unwrap_or_else(|e| e.into_inner());
    let needle = needle.as_bytes();
    !needle.is_empty() && buffer.windows(needle.len()).any(|w| w == needle)
}

/// The captured lines that mention `needle` (for failure messages).
pub fn lines_with(needle: &str) -> Vec<String> {
    snapshot()
        .lines()
        .filter(|l| l.contains(needle))
        .map(str::to_owned)
        .collect()
}
