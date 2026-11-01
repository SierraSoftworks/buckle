use std::path::PathBuf;
use std::{
    collections::{BTreeMap, HashMap},
    path::Path,
};
use walkdir::WalkDir;

use gotmpl::{MissingKey, Template, Value};
use tracing::instrument;

use crate::errors;

#[cfg(test)]
use mockall::automock;

use super::Core;

/// Writes a package's files to their target location.
#[cfg_attr(test, automock)]
pub trait FileWriter: Send + Sync {
    /// Copies (or renders, for templates) `file` into the `target` directory.
    fn apply(
        &self,
        file: &File,
        target: &Path,
        config: &HashMap<String, String>,
        secrets: &HashMap<String, String>,
    ) -> Result<(), errors::Error>;
}

/// Writes files to the local filesystem.
#[derive(Debug, Default)]
pub struct LocalFileWriter;

impl FileWriter for LocalFileWriter {
    #[instrument(level = "info", name = "file.apply", fields(file.path = %file.relative_path.display()), err, skip(self, file, secrets))]
    fn apply(
        &self,
        file: &File,
        target: &Path,
        config: &HashMap<String, String>,
        secrets: &HashMap<String, String>,
    ) -> Result<(), errors::Error> {
        if file.is_template {
            file.template(target, config, secrets)
        } else {
            file.copy(target)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
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
            dirs.filter_map(|dir| match dir {
                Ok(d) => match d.file_type() {
                    Ok(ft) if ft.is_dir() => Some(d.path()),
                    _ => None,
                },
                _ => None,
            })
        })
        .map_err(|err| {
            errors::user_with_internal(
            "Failed to read the list of file groups.", 
            "Read the internal error message and take the appropriate steps to resolve the issue.", 
            err)
        })?
        .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
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

            let target_path = if is_template {
                e.path().with_extension("")
            } else {
                e.path().to_owned()
            };

            File {
                group: group.clone(),
                relative_path: target_path.strip_prefix(dir).unwrap().to_owned(),
                source_path: e.path().to_owned(),
                is_template,
            }
        })
        .collect();

    Ok(files)
}

impl File {
    /// Writes this file into the `target` directory using the file writer
    /// configured on `core`.
    pub fn apply(
        &self,
        core: &Core,
        target: &Path,
        config: &HashMap<String, String>,
        secrets: &HashMap<String, String>,
    ) -> Result<(), errors::Error> {
        core.files().apply(self, target, config, secrets)
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

        let mut context: BTreeMap<String, Value> = BTreeMap::new();
        for (key, val) in config {
            context.insert(key.clone(), Value::from(val.as_str()));
        }

        for (key, val) in secrets {
            context.insert(key.clone(), Value::from(val.as_str()));
        }

        let context = Value::from(context);

        let template_name = self.source_path.to_string_lossy();
        let rendered = Template::new(&template_name)
            .missing_key(MissingKey::Error)
            .parse(&template_content)
            .and_then(|template| template.execute_to_string(&context))
            .map_err(|e| errors::user_with_internal(
                format!("Could not render the template '{}' due to a problem in your template.", self.source_path.display()),
                "Check that your template is valid Go template syntax, that every key it references is defined in your config or secrets, and review the internal error message for more information.",
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::{get_test_data, ConfigDir};

    fn config(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn get_file_groups_lists_the_group_directories() {
        let groups = get_file_groups(&get_test_data().join("packages").join("test1").join("files"))
            .expect("the groups should be listed");

        assert_eq!(groups, vec!["conf.d".to_string()]);
    }

    #[test]
    fn get_all_files_is_empty_for_a_missing_directory() {
        let dir = ConfigDir::new();

        let files = get_all_files(&dir.path().join("files")).expect("a missing directory should be empty");

        assert!(files.is_empty());
    }

    #[test]
    fn get_all_files_detects_templates_and_strips_their_extension() {
        let dir = ConfigDir::new()
            .with_package_file("pkg", "etc", "nested/app.conf.tpl", "")
            .with_package_file("pkg", "etc", "plain.txt", "")
            .with_package_file("pkg", "home", ".profile", "");

        let mut files = get_all_files(&dir.path().join("packages").join("pkg").join("files"))
            .expect("the files should be listed");
        files.sort_by(|a, b| (&a.group, &a.relative_path).cmp(&(&b.group, &b.relative_path)));

        let summary: Vec<(String, PathBuf, bool)> = files
            .into_iter()
            .map(|f| (f.group, f.relative_path, f.is_template))
            .collect();

        assert_eq!(
            summary,
            vec![
                ("etc".into(), PathBuf::from("nested").join("app.conf"), true),
                ("etc".into(), PathBuf::from("plain.txt"), false),
                ("home".into(), PathBuf::from(".profile"), false),
            ]
        );
    }

    #[test]
    fn local_writer_renders_templates_with_config_and_secrets() {
        let dir = ConfigDir::new().with_package_file(
            "pkg",
            "etc",
            "app.conf.tpl",
            "user={{ .USER }}\ntoken={{ .TOKEN }}\n",
        );
        let target = ConfigDir::new();

        let files = get_all_files(&dir.path().join("packages").join("pkg").join("files")).unwrap();
        LocalFileWriter
            .apply(
                &files[0],
                target.path(),
                &config(&[("USER", "ben")]),
                &config(&[("TOKEN", "s3cret")]),
            )
            .expect("the template should render");

        let rendered = std::fs::read_to_string(target.path().join("app.conf")).unwrap();
        assert_eq!(rendered, "user=ben\ntoken=s3cret\n");
    }

    #[test]
    fn local_writer_copies_plain_files_into_nested_targets() {
        let dir = ConfigDir::new().with_package_file("pkg", "etc", "deep/er/plain.txt", "hello");
        let target = ConfigDir::new();

        let files = get_all_files(&dir.path().join("packages").join("pkg").join("files")).unwrap();
        LocalFileWriter
            .apply(&files[0], target.path(), &config(&[]), &config(&[]))
            .expect("the file should be copied");

        let copied = std::fs::read_to_string(target.path().join("deep").join("er").join("plain.txt")).unwrap();
        assert_eq!(copied, "hello");
    }

    #[test]
    fn local_writer_reports_template_errors() {
        let dir = ConfigDir::new()
            .with_package_file("pkg", "etc", "unterminated.conf.tpl", "{{ .Unclosed ")
            .with_package_file("pkg", "etc", "unexpected-char.conf.tpl", "{{ .USER }")
            .with_package_file("pkg", "etc", "undefined-function.conf.tpl", "{{ nosuchfunc }}");
        let target = ConfigDir::new();

        let files = get_all_files(&dir.path().join("packages").join("pkg").join("files")).unwrap();
        assert_eq!(files.len(), 3);

        for file in &files {
            let result = LocalFileWriter.apply(file, target.path(), &config(&[]), &config(&[]));

            assert!(
                result.is_err(),
                "the broken template '{}' should be reported",
                file.relative_path.display()
            );
            assert!(
                !target.path().join(&file.relative_path).exists(),
                "nothing should be written for the broken template '{}'",
                file.relative_path.display()
            );
        }
    }

    #[test]
    fn local_writer_rejects_templates_which_reference_undefined_keys() {
        let dir = ConfigDir::new().with_package_file("pkg", "etc", "app.conf.tpl", "user={{ .USER }}\nport={{ .PORT }}\n");
        let target = ConfigDir::new();

        let files = get_all_files(&dir.path().join("packages").join("pkg").join("files")).unwrap();
        let result = LocalFileWriter.apply(&files[0], target.path(), &config(&[("USER", "ben")]), &config(&[]));

        assert!(
            result.is_err(),
            "a template referencing a key that is not configured should be reported rather than rendered with a placeholder"
        );
    }
}
