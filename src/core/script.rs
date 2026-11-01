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
    use crate::test::get_test_data;

    use super::get_all_scripts;

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
