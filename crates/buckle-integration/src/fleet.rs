//! Fleet configuration trees: `buckle.yaml` plus modules, written into the
//! directory the server's path source watches.
//!
//! [`Fleet`] is a value; tests derive the next revision from the previous one
//! with the builder methods and [`crate::World::publish`] it. Writing only
//! touches files whose content changed and deletes files the new tree no
//! longer has, so the watcher sees one short burst of changes per revision.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The rollout policy most scenarios share: second-scale timers, no local
/// rollback unless a scenario turns it on, and wide stragglers.
pub const FAST_ROLLOUT: &str = "  settle: 1s\n  health: { bake: 1s }\n  local_rollback: { enabled: false }\n  stragglers: { parallel: 10 }\n";

/// A configuration tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fleet {
    files: BTreeMap<PathBuf, String>,
}

impl Fleet {
    /// A tree whose `buckle.yaml` is `yaml`.
    pub fn new(yaml: impl Into<String>) -> Self {
        let mut files = BTreeMap::new();
        files.insert(PathBuf::from("buckle.yaml"), yaml.into());
        Self { files }
    }

    /// `buckle.yaml` with the shared groups (`canary` from the `canary` label),
    /// [`FAST_ROLLOUT`], `rollout_extra` (more `rollout:` keys, indented two
    /// spaces; a key given here replaces the [`FAST_ROLLOUT`] one) and these
    /// `phases` (YAML list items, indented four spaces).
    pub fn with_phases(rollout_extra: &str, phases: &str) -> Self {
        let key = |line: &str| line.trim_start().split(':').next().unwrap_or("").to_owned();
        let overridden: Vec<String> = rollout_extra
            .lines()
            .filter(|l| l.starts_with("  ") && !l.starts_with("   "))
            .map(key)
            .collect();
        let defaults: String = FAST_ROLLOUT
            .lines()
            .filter(|l| !overridden.contains(&key(l)))
            .map(|l| format!("{l}\n"))
            .collect();
        Self::new(format!(
            "version: 2\nfleet:\n  groups:\n    canary: 'host.labels.canary == true'\nrollout:\n{defaults}{rollout_extra}  phases:\n{phases}"
        ))
    }

    /// Replaces `buckle.yaml`.
    pub fn buckle_yaml(mut self, yaml: impl Into<String>) -> Self {
        self.files.insert(PathBuf::from("buckle.yaml"), yaml.into());
        self
    }

    /// The current `buckle.yaml`.
    pub fn yaml(&self) -> &str {
        self.files
            .get(Path::new("buckle.yaml"))
            .map(String::as_str)
            .unwrap_or("")
    }

    /// Sets `modules/<name>/module.yaml`.
    pub fn module(mut self, name: &str, yaml: impl Into<String>) -> Self {
        self.files.insert(
            Path::new("modules").join(name).join("module.yaml"),
            yaml.into(),
        );
        self
    }

    /// Sets `modules/<module>/files/<path>`.
    pub fn file(mut self, module: &str, path: &str, content: impl Into<String>) -> Self {
        self.files.insert(
            Path::new("modules").join(module).join("files").join(path),
            content.into(),
        );
        self
    }

    /// Removes a module and its files.
    pub fn without_module(mut self, name: &str) -> Self {
        let prefix = Path::new("modules").join(name);
        self.files.retain(|p, _| !p.starts_with(&prefix));
        self
    }

    /// Writes the tree under `root`, touching only what changed.
    pub fn write(&self, root: &Path) {
        std::fs::create_dir_all(root).expect("create the source directory");
        let mut existing = Vec::new();
        collect(root, root, &mut existing);
        for rel in existing {
            if !self.files.contains_key(&rel) {
                std::fs::remove_file(root.join(&rel)).expect("remove a stale file");
            }
        }
        for (rel, content) in &self.files {
            let path = root.join(rel);
            if std::fs::read_to_string(&path).ok().as_deref() == Some(content.as_str()) {
                continue;
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create a module directory");
            }
            std::fs::write(&path, content).expect("write a configuration file");
        }
        prune_empty_dirs(root);
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            out.push(rel.to_path_buf());
        }
    }
}

fn prune_empty_dirs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            prune_empty_dirs(&path);
            if std::fs::read_dir(&path).is_ok_and(|mut d| d.next().is_none()) {
                let _ = std::fs::remove_dir(&path);
            }
        }
    }
}

/// The revision id the server's path source gives the tree under `root`: the
/// digest of its deterministic tarball.
pub fn revision_id(root: &Path) -> String {
    let tarball = buckle_server::source::tarball::build(root).expect("the tree packs");
    buckle_core::Digest::of(&tarball).to_string()
}
