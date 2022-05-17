use std::{collections::HashMap, path::Path};

use std::fs::read_to_string;
use std::process;
use tracing::field::display;
use tracing::{Span, instrument};

use crate::errors;

#[cfg(test)]
use mocktopus::macros::*;

#[instrument(level = "debug", name = "config.load_all", err)]
pub fn load_all_config(dir: &Path) -> Result<HashMap<String, String>, errors::Error> {
    if !dir.exists() {
        return Ok(HashMap::new())
    }

    dir.read_dir()
        .map(|dirs| dirs.filter_map(|dir| match dir {
            Ok(d) => match d.file_type() {
                Ok(ft) if ft.is_file() => Some(d.path()),
                _ => None
            },
            _ => None,
        }))
        .map_err(|err| errors::user_with_internal(
            "Failed to read the list of configuration files.", 
            "Read the internal error message and take the appropriate steps to resolve the issue.", 
            err))
        .and_then(|files| {
            let mut output = HashMap::new();

            let mut errs: Vec<errors::Error> = files.map(|file| load_config(dunce::simplified(&file)).map(|config| {
                for (key, val) in config {
                    output.insert(key,val);
                }
            })).filter(|r| r.is_err()).map(|r| r.unwrap_err()).collect();

            match errs.pop() {
                Some(err) => Err(err),
                None => Ok(output)
            }
        })
}

#[instrument(level = "info", name = "config.load", err)]
pub fn load_config(file: &Path) -> Result<HashMap<String, String>, errors::Error> {
    let extension = match file.extension() {
        Some(ext) => ext.to_str().ok_or_else(|| errors::user(
            &format!("Unable to parse the file extension used by the config file '{}'", file.display()),
            "Make sure that the config file uses a valid file extension."
        ))?,
        None => Err(errors::user(
            &format!("Could not determine how to load the config file {} because it did not have a file extension.", file.display()), 
            "Use one of the supported file extensions to tell buckle how to read this config file."))?
    };

    let content = match extension {
        "env" => load_env_config(file)?,
        "ps1" => load_script_config("pwsh", file)?,
        "sh" => load_script_config("bash", file)?,
        "bat" => load_script_config("cmd.exe", file)?,
        "cmd" => load_script_config("cmd.exe", file)?,
        _ => Err(errors::user(
            &format!(
                "The '{}' extension is not supported for config files.",
                extension
            ),
            "Try using a file extension that is supported by buckle.",
        ))?,
    };

    Ok(parse_config(&content))
}

#[instrument(level = "debug", name = "config.load.env", err)]
fn load_env_config(file: &Path) -> Result<String, errors::Error> {
    read_to_string(file).map_err(|e| {
        errors::user_with_internal(
            "Unable to read configuration file due to an OS-level error.",
            "Read the internal error message and take the appropriate steps to resolve the issue.",
            e,
        )
    })
}

#[cfg_attr(test, mockable)]
#[instrument(level = "debug", name = "config.load.script", fields(stdout, stderr), err)]
pub fn load_script_config(interpreter: &str, file: &Path) -> Result<String, errors::Error> {
    process::Command::new(interpreter)
        .arg(file)
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
                Ok(stdout.to_string())
            } else {
                Err(errors::user_with_internal(
                    "Failed to load configuration from script.",
                    "Read the internal error message and take the appropriate steps to resolve the issue.",
                    human_errors::detailed_message(&format!(
                        "---- STDOUT: ----\n{}\n\n---- STDERR: ----\n{}",
                        stdout,
                        stderr))))
            }
        })
}

fn parse_config(content: &str) -> HashMap<String, String> {
    let mut output = HashMap::new();

    let pairs = content
        .split_terminator('\n')
        .map(|line| line.trim())
        .filter_map(|line| line.split_once('='));

    for (key, value) in pairs {
        output.insert(key.to_owned(), value.to_owned());
    }

    output
}
