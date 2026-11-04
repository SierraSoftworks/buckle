//! Loads a revision from a directory tree. Lane 1 completes validation
//! (selectors parse, templates parse, module graph acyclic, secrets declared).

use super::{LoadedModule, LoadedWorkload, Revision};
use crate::error::HumanizableResult;
use crate::model::{FleetConfig, ModuleConfig, WorkloadConfig};
use crate::plan::RevisionId;
use crate::task::DirSource;
use std::path::Path;
use std::sync::Arc;

/// A path as operators see it in errors: relative to the revision root, with `/`
/// separators on every platform, so messages never carry the server's temp layout.
fn shown(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Reads `buckle.yaml`, every `modules/*/module.yaml` and
/// `workloads/*/workload.yaml`, and the `vars` files.
pub fn load_dir(root: &Path, id: RevisionId) -> crate::Result<Revision> {
    let fleet_path = root.join("buckle.yaml");
    let fleet_yaml = std::fs::read_to_string(&fleet_path).map_err(|e| {
        crate::error::wrap_user(
            e,
            format!(
                "Could not read {} in {}.",
                shown(root, &fleet_path),
                root.display()
            ),
            &["Point Buckle at a configuration directory containing buckle.yaml."],
        )
    })?;
    let fleet = FleetConfig::parse(&fleet_yaml)?;

    let mut modules = Vec::new();
    let mut unloaded = Vec::new();
    let modules_dir = root.join("modules");
    if modules_dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&modules_dir)
            .to_human_error()?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !dir.is_dir() || name.starts_with('.') {
                continue;
            }
            let module_yaml = dir.join("module.yaml");
            if !module_yaml.is_file() {
                unloaded.push(name);
                continue;
            }
            let yaml = std::fs::read_to_string(&module_yaml).to_human_error()?;
            let config = ModuleConfig::parse(&yaml).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("The module at {} is invalid.", shown(root, &module_yaml)),
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

    let mut workloads = Vec::new();
    let mut unloaded_workloads = Vec::new();
    let workloads_dir = root.join("workloads");
    if workloads_dir.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(&workloads_dir)
            .to_human_error()?
            .filter_map(|e| e.ok())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !dir.is_dir() || name.starts_with('.') {
                continue;
            }
            let workload_yaml = dir.join("workload.yaml");
            if !workload_yaml.is_file() {
                unloaded_workloads.push(name);
                continue;
            }
            let yaml = std::fs::read_to_string(&workload_yaml).to_human_error()?;
            let config = WorkloadConfig::parse(&yaml).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!(
                        "The workload at {} is invalid.",
                        shown(root, &workload_yaml)
                    ),
                    &["Fix the workload.yaml and run `buckle validate` again."],
                )
            })?;
            if config.name != name {
                return Err(crate::error::user(
                    format!(
                        "The workload in directory '{name}' is named '{}'; the two must match.",
                        config.name
                    ),
                    &["Rename the directory or the workload so that they agree."],
                ));
            }
            let files_dir = dir.join("files");
            let mut files = Vec::new();
            let mut templates = Vec::new();
            if files_dir.is_dir() {
                for path in walk(&files_dir) {
                    let Ok(rel) = path.strip_prefix(&files_dir) else {
                        continue;
                    };
                    files.push(rel.to_path_buf());
                    if path.extension().is_some_and(|e| e == "j2")
                        && let Ok(content) = std::fs::read_to_string(&path)
                    {
                        templates.push((rel.to_path_buf(), content));
                    }
                }
            }
            workloads.push(LoadedWorkload {
                config,
                source: Arc::new(DirSource::new(files_dir)),
                files,
                templates,
            });
        }
    }

    let mut var_files = indexmap::IndexMap::new();
    for (index, overlay) in fleet.vars.iter().enumerate() {
        if let Some(file) = &overlay.file {
            let escapes = file.is_absolute()
                || file.has_root()
                || file.components().any(|c| {
                    matches!(
                        c,
                        std::path::Component::ParentDir | std::path::Component::Prefix(_)
                    )
                });
            if escapes {
                return Err(crate::error::user(
                    format!(
                        "The vars overlay {} in buckle.yaml names the file '{}', which is outside the configuration.",
                        index + 1,
                        file.display()
                    ),
                    &[
                        "Give overlay files as paths relative to the directory of buckle.yaml, without '..'.",
                    ],
                ));
            }
            let path = root.join(file);
            let yaml = std::fs::read_to_string(&path).map_err(|e| {
                crate::error::wrap_user(
                    e,
                    format!("The vars file {} could not be read.", shown(root, &path)),
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
        workloads,
        var_files,
        unloaded,
        unloaded_workloads,
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
    fn workloads_load_with_their_files_and_templates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("buckle.yaml"), "version: 2\n").unwrap();
        std::fs::create_dir_all(dir.path().join("workloads/paste/files")).unwrap();
        std::fs::write(
            dir.path().join("workloads/paste/workload.yaml"),
            "name: paste\njob: paste.nomad.hcl\nvolumes: [paste.volume.hcl]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("workloads/paste/files/paste.nomad.hcl"),
            "job \"paste\" {}",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("workloads/paste/files/paste.volume.hcl"),
            "name = \"paste\"",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("workloads/grafana/files")).unwrap();
        std::fs::write(
            dir.path().join("workloads/grafana/workload.yaml"),
            "name: grafana\nmanifests: [grafana.yaml.j2]\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("workloads/grafana/files/grafana.yaml.j2"),
            "image: grafana:{{ vars.v }}",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("workloads/half")).unwrap();
        let rev = load_dir(dir.path(), RevisionId::new("t")).unwrap();
        let names: Vec<&str> = rev
            .workloads
            .iter()
            .map(|w| w.config.name.as_str())
            .collect();
        assert_eq!(names, vec!["grafana", "paste"]);
        assert_eq!(rev.unloaded_workloads, vec!["half"]);
        assert!(rev.modules.is_empty());
        let paste = rev.workload("paste").unwrap();
        assert_eq!(paste.files.len(), 2);
        assert!(paste.templates.is_empty());
        let grafana = rev.workload("grafana").unwrap();
        assert_eq!(grafana.templates.len(), 1);
    }

    #[test]
    fn workload_directory_and_name_must_agree() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("buckle.yaml"), "version: 2\n").unwrap();
        std::fs::create_dir_all(dir.path().join("workloads/a")).unwrap();
        std::fs::write(dir.path().join("workloads/a/workload.yaml"), "name: b\n").unwrap();
        let err = load_dir(dir.path(), RevisionId::new("t")).unwrap_err();
        assert!(err.description().contains("'a'"), "{}", err.description());
    }

    #[test]
    fn module_directories_without_module_yaml_are_recorded_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("buckle.yaml"), "version: 2\n").unwrap();
        std::fs::create_dir_all(dir.path().join("modules/a")).unwrap();
        std::fs::write(dir.path().join("modules/a/module.yaml"), "name: a\n").unwrap();
        std::fs::create_dir_all(dir.path().join("modules/half/files")).unwrap();
        std::fs::create_dir_all(dir.path().join("modules/.hidden")).unwrap();
        std::fs::write(dir.path().join("modules/README.md"), "notes").unwrap();
        let rev = load_dir(dir.path(), RevisionId::new("t")).unwrap();
        assert_eq!(rev.modules.len(), 1);
        assert_eq!(rev.unloaded, vec!["half"]);
    }

    #[rstest::rstest]
    #[case("/etc/passwd")]
    #[case("../outside.yaml")]
    #[case("vars/../../outside.yaml")]
    fn vars_overlay_files_must_stay_inside_the_revision(#[case] file: &str) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("buckle.yaml"),
            format!("version: 2\nvars:\n  - file: '{file}'\n"),
        )
        .unwrap();
        let err = load_dir(dir.path(), RevisionId::new("t")).unwrap_err();
        assert!(
            err.description().contains("outside the configuration"),
            "{}",
            err.description()
        );
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
