use std::collections::HashMap;
use std::path::Path;
use std::process;

use tracing::field::display;
use tracing::{instrument, Span};

#[cfg(test)]
use mockall::automock;

use crate::errors;

/// Runs the scripts that make up a package's config and tasks.
#[cfg_attr(test, automock)]
pub trait Launcher: Send + Sync {
    /// Runs `file` with `interpreter` and returns everything it wrote to stdout.
    ///
    /// This is used to load configuration from scripts, where the script's
    /// output is parsed as a list of `KEY=value` pairs.
    fn load_config(&self, interpreter: &str, file: &Path) -> Result<String, errors::Error>;

    /// Runs `file` with `interpreter`, passing `env` as the process's environment.
    fn run_task(
        &self,
        interpreter: &str,
        env: &HashMap<String, String>,
        file: &Path,
    ) -> Result<(), errors::Error>;
}

/// Launches scripts as child processes on the local machine.
#[derive(Debug, Default)]
pub struct ProcessLauncher;

impl Launcher for ProcessLauncher {
    #[instrument(
        level = "debug",
        name = "config.load.script",
        fields(stdout, stderr),
        skip(self),
        err
    )]
    fn load_config(&self, interpreter: &str, file: &Path) -> Result<String, errors::Error> {
        process::Command::new(interpreter)
            .arg(file)
            .output()
            .map_err(|err| errors::user_with_internal(
                format!("Failed to execute the command '{} {}'.", interpreter, file.display()),
                format!("Make sure that '{interpreter}' is installed and present on your path and that you have permission to access it."),
                err))
            .and_then(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                Span::current().record("stdout", display(&stdout));
                Span::current().record("stderr", display(&stderr));

                if output.status.success() {
                    Ok(stdout.to_string())
                } else {
                    Err(errors::user_with_internal(
                        "Failed to load configuration from script.",
                        "Read the internal error message and take the appropriate steps to resolve the issue.",
                        errors::detailed_message(format!(
                            "---- STDOUT: ----\n{stdout}\n\n---- STDERR: ----\n{stderr}"))))
                }
            })
    }

    #[instrument(name = "command.run", fields(stdout, stderr), skip(self, env), err)]
    fn run_task(
        &self,
        interpreter: &str,
        env: &HashMap<String, String>,
        file: &Path,
    ) -> Result<(), errors::Error> {
        process::Command::new(interpreter)
            .arg(file)
            .envs(env)
            .output()
            .map_err(|err| errors::user_with_internal(
                format!("Failed to execute the command '{} {}'.", interpreter, file.display()),
                format!("Make sure that '{interpreter}' is installed and present on your path and that you have permission to access it."),
                err))
            .and_then(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                Span::current().record("stdout", display(&stdout));
                Span::current().record("stderr", display(&stderr));

                if output.status.success() {
                    Ok(())
                } else {
                    Err(errors::user_with_internal(
                        "Failed to run script.",
                        "Read the internal error message and take the appropriate steps to resolve the issue.",
                        errors::detailed_message(format!(
                            "---- STDOUT: ----\n{stdout}\n\n---- STDERR: ----\n{stderr}"))))
                }
            })
    }
}
