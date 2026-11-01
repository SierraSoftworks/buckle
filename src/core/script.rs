use std::path::PathBuf;
use std::{collections::HashMap, path::Path};

use itertools::Itertools;
use tracing::instrument;

use crate::errors;

use super::Core;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Script {
    pub name: String,
    pub path: PathBuf,
}

#[instrument(level = "debug", name = "script.get_all", err)]
pub fn get_all_scripts(dir: &Path) -> Result<Vec<Script>, errors::Error> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let files = dir
        .read_dir()
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
            "Failed to read the list of tasks.", 
            "Read the internal error message and take the appropriate steps to resolve the issue.", 
            err)
        })?;

    Ok(files
        .map(|f| Script {
            name: f
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap(),
            path: dunce::simplified(&f).to_owned(),
        })
        .sorted_by_key(|s| s.name.clone())
        .collect())
}

impl Script {
    #[instrument(level = "info", name = "script.run", fields(task.name = %self.name, task.path = %self.path.display()), err, skip(self, core, secrets))]
    pub fn run(
        &self,
        core: &Core,
        config: &HashMap<String, String>,
        secrets: &HashMap<String, String>,
    ) -> Result<(), errors::Error> {
        let extension = match self.path.extension() {
            Some(ext) => ext.to_str().ok_or_else(|| errors::user(
                &format!("Unable to parse the file extension used by the task file '{}'", self.path.display()),
                "Make sure that the task file uses a valid file extension."
            ))?,
            None => Err(errors::user(
                &format!("Could not determine how to run the task file '{}' because it did not have a file extension.", self.path.display()), 
                "Use one of the supported file extensions to tell buckle how to execute this task file."))?
        };

        let mut config = config.clone();
        for (key, val) in secrets {
            config.insert(key.clone(), val.into());
        }

        match extension {
            "ps1" => core.launcher().run_task("pwsh", &config, &self.path)?,
            "sh" => core.launcher().run_task("bash", &config, &self.path)?,
            "bat" => core.launcher().run_task("cmd.exe", &config, &self.path)?,
            "cmd" => core.launcher().run_task("cmd.exe", &config, &self.path)?,
            _ => Err(errors::user(
                &format!(
                    "The '{extension}' extension is not supported for task files."
                ),
                "Try using a file extension that is supported by buckle.",
            ))?,
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::core::Core;
    use crate::test::{get_test_data, ConfigDir};

    use super::*;

    fn pairs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn get_all_scripts_is_empty_for_a_missing_directory() {
        let dir = ConfigDir::new();

        let scripts = get_all_scripts(&dir.path().join("scripts"))
            .expect("a missing directory should be treated as empty");

        assert!(scripts.is_empty());
    }

    #[test]
    fn get_all_scripts_sorts_by_name() {
        let dir = ConfigDir::new()
            .with_package_script("pkg", "20-second.sh", "")
            .with_package_script("pkg", "10-first.sh", "");

        let scripts = get_all_scripts(&dir.path().join("packages").join("pkg").join("scripts"))
            .expect("the scripts should be listed");
        let names: Vec<&str> = scripts.iter().map(|s| s.name.as_str()).collect();

        assert_eq!(names, vec!["10-first.sh", "20-second.sh"]);
    }

    #[test]
    fn run_dispatches_to_the_interpreter_with_config_and_secrets() {
        let cases = [("ps1", "pwsh"), ("sh", "bash"), ("bat", "cmd.exe"), ("cmd", "cmd.exe")];

        for (extension, interpreter) in cases {
            let script = Script {
                name: format!("setup.{extension}"),
                path: PathBuf::from(format!("/pkg/scripts/setup.{extension}")),
            };

            let core = Core::builder()
                .with_mock_launcher(move |mock| {
                    let expected_path = script_path(extension);
                    mock.expect_run_task()
                        .withf(move |i, env, file| {
                            i == interpreter
                                && file == expected_path
                                && env.get("FROM_CONFIG").map(String::as_str) == Some("yes")
                                && env.get("FROM_SECRET").map(String::as_str) == Some("shh")
                        })
                        .once()
                        .returning(|_, _, _| Ok(()));
                })
                .build();

            script
                .run(&core, &pairs(&[("FROM_CONFIG", "yes")]), &pairs(&[("FROM_SECRET", "shh")]))
                .unwrap_or_else(|e| panic!(".{extension} should run: {}", e.message()));
        }

        fn script_path(extension: &str) -> PathBuf {
            PathBuf::from(format!("/pkg/scripts/setup.{extension}"))
        }
    }

    #[test]
    fn run_rejects_unsupported_extensions() {
        let core = Core::builder()
            .with_mock_launcher(|mock| {
                mock.expect_run_task().never();
            })
            .build();

        for name in ["setup.py", "setup"] {
            let script = Script {
                name: name.to_string(),
                path: PathBuf::from("/pkg/scripts").join(name),
            };

            let result = script.run(&core, &pairs(&[]), &pairs(&[]));

            assert!(result.is_err(), "'{}' should not be runnable", name);
        }
    }

    #[test]
    fn test_load() {
        let path = get_test_data()
            .join("packages")
            .join("test1")
            .join("scripts");
        let scripts = get_all_scripts(&path).expect("scripts should be loaded");

        assert_eq!(scripts.len(), 1, "there should be 1 script in the package");

        let script = &scripts[0];
        assert_eq!(
            script.name, "setup.ps1",
            "the script's name should be correct"
        );
        assert_eq!(
            script.path,
            dunce::simplified(&path.join("setup.ps1")),
            "the script's path should be correct"
        );
    }
}
