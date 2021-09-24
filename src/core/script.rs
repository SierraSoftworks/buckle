use std::path::PathBuf;
use std::{collections::HashMap, path::Path};

use std::process;
use itertools::Itertools;
use tracing::field::display;
use tracing::{instrument, Span};

use crate::errors;

#[cfg(test)]
use mocktopus::macros::*;

#[cfg_attr(test, mockable)]
#[derive(Clone)]
pub struct Script {
    pub name: String,
    pub path: PathBuf,
}

#[instrument(level = "debug", name = "script.get_all", err)]
pub fn get_all_scripts(dir: &Path) -> Result<Vec<Script>, errors::Error> {
    if !dir.exists() {
        return Ok(Vec::new())
    }

    let files = dir.read_dir()
        .map(|dirs| dirs.map(|dir| match dir {
            Ok(d) => match d.file_type() {
                Ok(ft) if ft.is_file() => Some(d.path()),
                _ => None
            },
            _ => None,
        })
        .filter(|d| d.is_some())
        .map(|d| d.unwrap()))
        .map_err(|err| errors::user_with_internal(
            "Failed to read the list of tasks.", 
            "Read the internal error message and take the appropriate steps to resolve the issue.", 
            err))?;

    Ok(files.map(|f| Script {
        name: f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap(),
        path: dunce::simplified(&f).to_owned(),
    }).sorted_by_key(|s| s.name.clone()).collect())
}

#[cfg_attr(test, mockable)]
impl Script {
    #[instrument(level = "info", name = "script.run", fields(task.name = %self.name, task.path = %self.path.display()), err, skip(self))]
    pub fn run(&self, config: &HashMap<String, String>) -> Result<(), errors::Error> {
        let extension = match self.path.extension() {
            Some(ext) => ext.to_str().ok_or(errors::user(
                &format!("Unable to parse the file extension used by the task file '{}'", self.path.display()),
                "Make sure that the task file uses a valid file extension."
            ))?,
            None => Err(errors::user(
                &format!("Could not determine how to run the task file '{}' because it did not have a file extension.", self.path.display()), 
                "Use one of the supported file extensions to tell buckle how to execute this task file."))?
        };
    
        match extension {
            "ps1" => run_script_task("pwsh", config, &self.path)?,
            "sh" => run_script_task("bash", config, &self.path)?,
            "bat" => run_script_task("cmd.exe", config, &self.path)?,
            "cmd" => run_script_task("cmd.exe", config, &self.path)?,
            _ => Err(errors::user(
                &format!(
                    "The '{}' extension is not supported for task files.",
                    extension
                ),
                "Try using a file extension that is supported by buckle.",
            ))?,
        }
    
        Ok(())
    }
}

#[cfg_attr(test, mockable)]
#[instrument(name = "command.run", fields(stdout, stderr), skip(config), err)]
pub fn run_script_task(interpreter: &str, config: &HashMap<String, String>, file: &Path) -> Result<(), errors::Error> {
    process::Command::new(interpreter)
        .arg(file)
        .envs(config)
        .output()
        .map_err(|err| errors::user_with_internal(
            &format!("Failed to execute the command '{} {}'.", interpreter, file.display()), 
            &format!("Make sure that '{}' is installed and present on your path and that you have permission to access it.", interpreter),
            err))
        .and_then(|output| {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            Span::current().record("stdout", &display(String::from_utf8_lossy(&output.stdout)));
            Span::current().record("stderr", &display(String::from_utf8_lossy(&output.stderr)));

            if output.status.success() {
                Ok(())
            } else {
                Err(errors::user_with_internal(
                    "Failed to run script.",
                    "Read the internal error message and take the appropriate steps to resolve the issue.",
                    human_errors::detailed_message(&format!(
                        "---- STDOUT: ----\n{}\n\n---- STDERR: ----\n{}",
                        stdout,
                        stderr))))
            }
        })
}

#[cfg(test)]
mod tests {
    use crate::test::get_test_data;

    use super::get_all_scripts;

    #[test]
    fn test_load() {
        let path = get_test_data().join("packages").join("test1").join("scripts");
        let scripts = get_all_scripts(&path).expect("scripts should be loaded");
        
        assert_eq!(scripts.len(), 1, "there should be 1 script in the package");
        
        let script = &scripts[0];
        assert_eq!(script.name, "setup.ps1", "the script's name should be correct");
        assert_eq!(script.path, dunce::simplified(&path.join("setup.ps1")), "the script's path should be correct");
    }
}