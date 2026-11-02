//! Loads a revision from a directory tree. Lane 1 completes validation
//! (selectors parse, templates parse, module graph acyclic, secrets declared).

use super::{LoadedModule, Revision};
use crate::error::HumanizableResult;
use crate::model::{FleetConfig, ModuleConfig};
use crate::plan::RevisionId;
use crate::task::DirSource;
use std::path::Path;
use std::sync::Arc;

/// Reads `buckle.yaml`, every `modules/*/module.yaml` and the `vars` files.
pub fn load_dir(root: &Path, id: RevisionId) -> crate::Result<Revision> {
    let fleet_path = root.join("buckle.yaml");
    let fleet_yaml = std::fs::read_to_string(&fleet_path).map_err(|e| {
        crate::error::wrap_user(
            e,
            format!("Could not read {}.", fleet_path.display()),
            &["Point Buckle at a configuration directory containing buckle.yaml."],
        )
    })?;
    let fleet = FleetConfig::parse(&fleet_yaml)?;

    let mut modules = Vec::new();
    let modules_dir = root.join("modules");
    if modules_dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&modules_dir)
            .to_human_error()?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let dir = entry.path();
            let module_yaml = dir.join("module.yaml");
            if !module_yaml.is_file() {
                continue;
            }
            let yaml = std::fs::read_to_string(&module_yaml).to_human_error()?;
            let config = ModuleConfig::parse(&yaml).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("The module at {} is invalid.", module_yaml.display()),
                    &["Fix the module.yaml and run `buckle validate` again."],
                )
            })?;
            let dir_name = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if config.name != dir_name {
                return Err(crate::error::user(
                    format!(
                        "The module in directory '{dir_name}' is named '{}'; the two must match.",
                        config.name
                    ),
                    &["Rename the directory or the module so that they agree."],
                ));
            }
            let files = dir.join("files");
            let mut templates = Vec::new();
            if files.is_dir() {
                for path in walk(&files) {
                    if path.extension().is_some_and(|e| e == "j2")
                        && let Ok(content) = std::fs::read_to_string(&path)
                        && let Ok(rel) = path.strip_prefix(&files)
                    {
                        templates.push((rel.to_path_buf(), content));
                    }
                }
            }
            modules.push(LoadedModule {
                config,
                source: Arc::new(DirSource::new(files)),
                templates,
            });
        }
    }

    let mut var_files = indexmap::IndexMap::new();
    for overlay in &fleet.vars {
        if let Some(file) = &overlay.file {
            let path = root.join(file);
            let yaml = std::fs::read_to_string(&path).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("The vars file {} could not be read.", path.display()),
                    &["Check the `vars` list in buckle.yaml."],
                )
            })?;
            let value: serde_yaml::Value = serde_yaml::from_str(&yaml).to_human_error()?;
            var_files.insert(file.clone(), value);
        }
    }

    Ok(Revision {
        id,
        fleet,
        modules,
        var_files,
    })
}

fn walk(root: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_the_minimal_fixture() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/config/minimal");
        let rev = load_dir(&root, RevisionId::new("test")).expect("fixture loads");
        assert_eq!(rev.fleet.version, 2);
        let names: Vec<&str> = rev.modules.iter().map(|m| m.config.name.as_str()).collect();
        assert_eq!(names, vec!["motd", "sysctl"]);
        assert!(
            rev.modules[0]
                .templates
                .iter()
                .any(|(p, _)| p.ends_with("motd.j2"))
        );
        assert_eq!(rev.var_files.len(), 1);
    }

    #[test]
    fn module_directory_and_name_must_agree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("buckle.yaml"), "version: 2\n").unwrap();
        std::fs::create_dir_all(dir.path().join("modules/a")).unwrap();
        std::fs::write(dir.path().join("modules/a/module.yaml"), "name: b\n").unwrap();
        let err = load_dir(dir.path(), RevisionId::new("t")).unwrap_err();
        assert!(err.description().contains("'a'"));
    }
}
