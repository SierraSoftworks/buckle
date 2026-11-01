use std::{collections::HashMap, path::Path};

use std::fs::read_to_string;
use tracing::instrument;

use crate::errors;

use super::Core;

#[instrument(level = "debug", name = "config.load_all", skip(core), err)]
pub fn load_all_config(core: &Core, dir: &Path) -> Result<HashMap<String, String>, errors::Error> {
    if !dir.exists() {
        return Ok(HashMap::new());
    }

    dir.read_dir()
        .map(|dirs| {
            dirs.filter_map(|dir| match dir {
                Ok(d) => match d.file_type() {
                    Ok(ft) if ft.is_file() => Some(d.path()),
                    _ => None,
                },
                _ => None,
            })
        })
        .map_err(|err| {
            errors::user_with_internal(
            "Failed to read the list of configuration files.", 
            "Read the internal error message and take the appropriate steps to resolve the issue.", 
            err)
        })
        .and_then(|files| {
            let mut output = HashMap::new();

            let mut errs: Vec<errors::Error> = files
                .map(|file| {
                    load_config(core, dunce::simplified(&file)).map(|config| {
                        for (key, val) in config {
                            output.insert(key, val);
                        }
                    })
                })
                .filter(|r| r.is_err())
                .map(|r| r.unwrap_err())
                .collect();

            match errs.pop() {
                Some(err) => Err(err),
                None => Ok(output),
            }
        })
}

#[instrument(level = "info", name = "config.load", skip(core), err)]
pub fn load_config(core: &Core, file: &Path) -> Result<HashMap<String, String>, errors::Error> {
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
        "ps1" => core.launcher().load_config("pwsh", file)?,
        "sh" => core.launcher().load_config("bash", file)?,
        "bat" => core.launcher().load_config("cmd.exe", file)?,
        "cmd" => core.launcher().load_config("cmd.exe", file)?,
        _ => Err(errors::user(
            &format!(
                "The '{extension}' extension is not supported for config files."
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Core;
    use crate::test::{get_test_data, ConfigDir};

    fn core() -> Core {
        Core::builder()
            .with_mock_launcher(|mock| {
                mock.expect_load_config().never();
            })
            .build()
    }

    #[test]
    fn parse_config_reads_key_value_pairs() {
        let config = parse_config("A=1\n  B = two \r\nC=x=y\nnot a pair\n\nA=overridden\n");

        assert_eq!(config.get("A").map(String::as_str), Some("overridden"));
        assert_eq!(config.get("B ").map(String::as_str), Some(" two"));
        assert_eq!(config.get("C").map(String::as_str), Some("x=y"));
        assert_eq!(config.len(), 3, "lines without an '=' should be ignored");
    }

    #[test]
    fn load_config_reads_env_files() {
        let config = load_config(&core(), &get_test_data().join("config").join("defaults.env"))
            .expect("the env file should load");

        assert!(!config.is_empty(), "the env file should contain config");
    }

    #[test]
    fn load_config_runs_scripts_through_the_launcher() {
        let cases = [("ps1", "pwsh"), ("sh", "bash"), ("bat", "cmd.exe"), ("cmd", "cmd.exe")];

        for (extension, interpreter) in cases {
            let dir = ConfigDir::new().with_config(&format!("values.{extension}"), "");
            let file = dir.path().join("config").join(format!("values.{extension}"));

            let core = Core::builder()
                .with_mock_launcher(move |mock| {
                    let expected = file.clone();
                    mock.expect_load_config()
                        .withf(move |i, f| i == interpreter && f == expected)
                        .once()
                        .returning(|_, _| Ok("FROM_SCRIPT=yes\n".to_string()));
                })
                .build();

            let config = load_config(&core, &dir.path().join("config").join(format!("values.{extension}")))
                .unwrap_or_else(|e| panic!("the .{extension} file should load: {}", e.message()));

            assert_eq!(
                config.get("FROM_SCRIPT").map(String::as_str),
                Some("yes"),
                "the script output for .{extension} should be parsed as config"
            );
        }
    }

    #[test]
    fn load_config_rejects_unsupported_files() {
        let dir = ConfigDir::new()
            .with_config("notes.txt", "A=1")
            .with_config("noextension", "A=1");

        for name in ["notes.txt", "noextension"] {
            let result = load_config(&core(), &dir.path().join("config").join(name));
            assert!(result.is_err(), "'{}' should not be loadable as config", name);
        }
    }

    #[test]
    fn load_all_config_merges_every_file_in_the_directory() {
        let dir = ConfigDir::new()
            .with_config("a.env", "A=1\nSHARED=a")
            .with_config("b.env", "B=2\nSHARED=b");

        let config = load_all_config(&core(), &dir.path().join("config"))
            .expect("the config directory should load");

        assert_eq!(config.get("A").map(String::as_str), Some("1"));
        assert_eq!(config.get("B").map(String::as_str), Some("2"));
        assert!(
            matches!(config.get("SHARED").map(String::as_str), Some("a") | Some("b")),
            "a key defined in two files should take one of their values"
        );
    }

    #[test]
    fn load_all_config_is_empty_for_a_missing_directory() {
        let dir = ConfigDir::new();

        let config = load_all_config(&core(), &dir.path().join("config"))
            .expect("a missing directory should be treated as empty");

        assert!(config.is_empty());
    }

    #[test]
    fn load_all_config_fails_on_an_unsupported_file() {
        let dir = ConfigDir::new()
            .with_config("a.env", "A=1")
            .with_config("README.md", "not config");

        let result = load_all_config(&core(), &dir.path().join("config"));

        assert!(result.is_err(), "an unsupported file should fail the load");
    }
}
