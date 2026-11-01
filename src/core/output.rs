use std::io::{stdout, Write};

#[cfg(test)]
use mockall::automock;

/// Provides the writer that user-facing output is sent to.
#[cfg_attr(test, automock)]
pub trait Output: Send + Sync {
    fn writer(&self) -> Box<dyn Write + Send>;
}

/// Writes output to the process's stdout.
#[derive(Debug, Default)]
pub struct StdoutOutput;

impl Output for StdoutOutput {
    fn writer(&self) -> Box<dyn Write + Send> {
        Box::new(stdout())
    }
}

#[cfg(test)]
#[allow(dead_code)]
pub mod mocks {
    use super::*;
    use std::{
        io::ErrorKind,
        sync::{Arc, Mutex},
    };

    /// An in-memory [`Write`] implementation which collects everything written
    /// to it so that tests can inspect the output of a command.
    ///
    /// Clones share the same buffer, so a test can keep one handle and hand
    /// another to the [`Core`](crate::core::Core) under test.
    #[derive(Clone, Debug, Default)]
    pub struct OutputBuffer {
        written_data: Arc<Mutex<String>>,
    }

    impl OutputBuffer {
        pub fn clear(&self) {
            self.written_data
                .lock()
                .map(|mut m| m.clear())
                .unwrap_or_default();
        }
    }

    impl std::fmt::Display for OutputBuffer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let data = self
                .written_data
                .lock()
                .map(|m| m.to_string())
                .unwrap_or_default();
            f.write_str(&data)
        }
    }

    impl Output for OutputBuffer {
        fn writer(&self) -> Box<dyn Write + Send> {
            Box::new(self.clone())
        }
    }

    impl Write for OutputBuffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written_data
                .lock()
                .map_err(|_| ErrorKind::Other.into())
                .map(|mut data| {
                    data.push_str(&String::from_utf8_lossy(buf));
                    buf.len()
                })
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
