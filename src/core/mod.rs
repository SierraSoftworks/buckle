use std::sync::Arc;

pub mod config;
pub mod file;
pub mod launcher;
pub mod output;
pub mod package;
pub mod retry;
pub mod script;

pub use file::FileWriter;
pub use launcher::Launcher;
pub use output::Output;

/// The set of services that buckle uses to interact with the outside world.
///
/// Each service sits behind a trait so that tests can substitute a mock in
/// place of the real filesystem, process launcher or terminal output. Build one
/// with [`Core::builder`] (or [`Core::default`] for the production wiring).
pub struct Core {
    output: Arc<dyn Output>,
    launcher: Arc<dyn Launcher>,
    files: Arc<dyn FileWriter>,
}

impl Core {
    pub fn builder() -> CoreBuilder {
        CoreBuilder::default()
    }

    /// The destination for user-facing output (normally stdout).
    pub fn output(&self) -> &dyn Output {
        self.output.as_ref()
    }

    /// Runs config and task scripts.
    pub fn launcher(&self) -> &dyn Launcher {
        self.launcher.as_ref()
    }

    /// Writes package files to their target locations.
    pub fn files(&self) -> &dyn FileWriter {
        self.files.as_ref()
    }
}

impl Default for Core {
    fn default() -> Self {
        Self::builder().build()
    }
}

#[derive(Default)]
pub struct CoreBuilder {
    output: Option<Arc<dyn Output>>,
    launcher: Option<Arc<dyn Launcher>>,
    files: Option<Arc<dyn FileWriter>>,
}

#[cfg_attr(not(test), allow(dead_code))]
impl CoreBuilder {
    pub fn build(self) -> Core {
        Core {
            output: self
                .output
                .unwrap_or_else(|| Arc::new(output::StdoutOutput)),
            launcher: self
                .launcher
                .unwrap_or_else(|| Arc::new(launcher::ProcessLauncher)),
            files: self
                .files
                .unwrap_or_else(|| Arc::new(file::LocalFileWriter)),
        }
    }

    pub fn with_output(mut self, output: Arc<dyn Output>) -> Self {
        self.output = Some(output);
        self
    }

    pub fn with_launcher(mut self, launcher: Arc<dyn Launcher>) -> Self {
        self.launcher = Some(launcher);
        self
    }

    pub fn with_files(mut self, files: Arc<dyn FileWriter>) -> Self {
        self.files = Some(files);
        self
    }

    #[cfg(test)]
    pub fn with_mock_output<F>(self, setup: F) -> Self
    where
        F: FnOnce(&mut output::MockOutput),
    {
        let mut mock = output::MockOutput::new();
        setup(&mut mock);
        self.with_output(Arc::new(mock))
    }

    #[cfg(test)]
    pub fn with_mock_launcher<F>(self, setup: F) -> Self
    where
        F: FnOnce(&mut launcher::MockLauncher),
    {
        let mut mock = launcher::MockLauncher::new();
        setup(&mut mock);
        self.with_launcher(Arc::new(mock))
    }

    #[cfg(test)]
    pub fn with_mock_files<F>(self, setup: F) -> Self
    where
        F: FnOnce(&mut file::MockFileWriter),
    {
        let mut mock = file::MockFileWriter::new();
        setup(&mut mock);
        self.with_files(Arc::new(mock))
    }
}
