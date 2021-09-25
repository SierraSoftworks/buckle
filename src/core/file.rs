use std::path::PathBuf;
use std::{collections::HashMap, path::Path};
use walkdir::WalkDir;

use gtmpl::{template, Value};
use tracing::{instrument};

use crate::errors;

#[cfg(test)]
use mocktopus::macros::*;

#[cfg_attr(test, mockable)]
#[derive(Clone)]
pub struct File {
    pub group: String,
    pub relative_path: PathBuf,
    pub source_path: PathBuf,
    pub is_template: bool,
}

#[instrument(level = "debug", name = "file.get_all", err)]
pub fn get_all_files(dir: &Path) -> Result<Vec<File>, errors::Error> {
    let mut files = Vec::new();

    for group in get_file_groups(dir)? {
        let group_files = get_files(&dir.join(group))?;
        files.extend(group_files);
    }

    Ok(files)
}

#[instrument(level = "debug", name = "file.get_groups", err)]
pub fn get_file_groups(dir: &Path) -> Result<Vec<String>, errors::Error> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let groups = dir
        .read_dir()
        .map(|dirs| {
            dirs.map(|dir| match dir {
                Ok(d) => match d.file_type() {
                    Ok(ft) if ft.is_dir() => Some(d.path()),
                    _ => None,
                },
                _ => None,
            })
            .filter(|d| d.is_some())
            .map(|d| d.unwrap())
        })
        .map_err(|err| {
            errors::user_with_internal(
            "Failed to read the list of file groups.", 
            "Read the internal error message and take the appropriate steps to resolve the issue.", 
            err)
        })?
        .map(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        .filter(|p| p.is_some())
        .map(|p| p.unwrap())
        .collect();

    Ok(groups)
}

#[instrument(level = "debug", name = "file.get_files", err)]
pub fn get_files(dir: &Path) -> Result<Vec<File>, errors::Error> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let group = dir
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap();

    let files = WalkDir::new(dir)
        .follow_links(true)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            let name = e.file_name().to_string_lossy();
            let is_template = name.ends_with(".tpl");

            File {
                group: group.clone(),
                relative_path: e.path().strip_prefix(dir).unwrap().to_owned(),
                source_path: e.path().to_owned(),
                is_template,
            }
        })
        .collect();

    Ok(files)
}

#[cfg_attr(test, mockable)]
impl File {
    #[instrument(level = "info", name = "file.apply", fields(file.path = %self.relative_path.display()), err, skip(self, secrets))]
    pub fn apply(
        &self,
        target: &Path,
        config: &HashMap<String, String>,
        secrets: &HashMap<String, String>,
    ) -> Result<(), errors::Error> {
        if self.is_template {
            self.template(target, config, secrets)
        } else {
            self.copy(target)
        }
    }

    #[instrument(level = "debug", name = "file.template", fields(file.path = %self.relative_path.display()), err, skip(self, secrets))]
    fn template(
        &self,
        target: &Path,
        config: &HashMap<String, String>,
        secrets: &HashMap<String, String>,
    ) -> Result<(), errors::Error> {
        let output_path = target.join(&self.relative_path);

        match output_path.parent() {
            Some(path) if !path.exists() => std::fs::create_dir_all(path)?,
            _ => {}
        };

        let template_content = std::fs::read_to_string(&self.source_path)?;

        let mut context = HashMap::new();
        for (key, val) in config {
            context.insert(key.clone(), Value::String(val.clone()));
        }

        for (key, val) in secrets {
            context.insert(key.clone(), Value::String(val.clone()));
        }

        let context = Value::Object(context);

        let rendered = template(&template_content, context)
            .map_err(|e| errors::user_with_internal(
                &format!("Could not render the template '{}' due to a problem in your template.", self.source_path.display()),
                "Check that your template is valid and review the internal error message for more information.", 
                e))?;

        std::fs::write(output_path, rendered)?;

        Ok(())
    }

    #[instrument(level = "debug", name = "file.copy", fields(file.path = %self.relative_path.display()), err, skip(self))]
    fn copy(&self, target: &Path) -> Result<(), errors::Error> {
        let output_path = target.join(&self.relative_path);

        match output_path.parent() {
            Some(path) if !path.exists() => std::fs::create_dir_all(path)?,
            _ => {}
        };

        std::fs::copy(&self.source_path, &output_path)
            .map_err(|e| errors::user_with_internal(
                &format!("Failed to copy file '{}' to the target directory '{}'.", self.source_path.display(), output_path.display()),
                "Check that you have permission to write the file to this directory and that there is space available on the drive.",
                e))?;

        Ok(())
    }
}
