//! Helpers shared by the apt, systemd, sysctl and ufw task kinds: running
//! commands through [`System`], and a tiny ownership ledger.
//!
//! The ledger exists because [`buckle_core::task::Task::apply`] cannot see the
//! resource's previous [`buckle_core::state::ResourceRecord`]. A task that
//! must remember "Buckle installed this package" across a later re-apply (for
//! example a version bump, when the package is already installed) records the
//! fact in the ledger as well as in its artefacts, so that the artefacts of the
//! new record stay complete.

use buckle_core::Result;
use buckle_core::system::{Command, Output};
use buckle_core::task::Cx;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where Buckle keeps its own files on the host (`BUCKLE_STATE_DIR`, default `/var/lib/buckle`).
pub(crate) fn state_dir(cx: &Cx) -> PathBuf {
    cx.system
        .env_var("BUCKLE_STATE_DIR")
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/buckle"))
}

/// Runs a command and fails with its stderr unless it exits 0.
pub(crate) async fn run_checked(cx: &Cx, command: Command) -> Result<Output> {
    let line = command.display_line();
    let out = cx.system.run(command).await?;
    if out.success() {
        return Ok(out);
    }
    let detail = first_lines(&out.stderr_str(), &out.stdout_str(), 6);
    let how = if out.timed_out {
        "timed out".to_owned()
    } else {
        match out.code {
            Some(c) => format!("exited with status {c}"),
            None => "was terminated by a signal".to_owned(),
        }
    };
    Err(buckle_core::error::user(
        format!("`{line}` {how}: {detail}"),
        &[
            "Run the command by hand on the host to see the full output.",
            "Check that the tool is installed and that Buckle runs with sufficient privileges.",
        ],
    ))
}

/// The first `n` non-empty lines of stderr (or stdout when stderr is empty), joined.
pub(crate) fn first_lines(stderr: &str, stdout: &str, n: usize) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(n)
        .collect();
    if lines.is_empty() {
        "no output".to_owned()
    } else {
        lines.join(" | ")
    }
}

/// A scalar that YAML authors write as a string, a number or a boolean
/// (`value: 1`, `to_port: 22`), kept as its text form.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Scalar {
    Text(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Scalar {
    pub fn text(&self) -> String {
        match self {
            Scalar::Text(s) => s.clone(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Float(f) => f.to_string(),
            Scalar::Bool(b) => b.to_string(),
        }
    }
}

/// Collapses runs of whitespace (the kernel prints tab-separated multi-value sysctls).
pub(crate) fn squash_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A persistent key → value map under `<state_dir>/ledger/<namespace>.json`.
pub(crate) struct Ledger {
    path: PathBuf,
    entries: BTreeMap<String, String>,
}

impl Ledger {
    pub(crate) async fn open(cx: &Cx, namespace: &str) -> Result<Self> {
        let path = state_dir(cx)
            .join("ledger")
            .join(format!("{namespace}.json"));
        let entries = match cx.system.read(&path).await? {
            Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            None => BTreeMap::new(),
        };
        Ok(Self { path, entries })
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    pub(crate) fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    pub(crate) fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.entries.insert(key.into(), value.into());
    }

    pub(crate) fn remove(&mut self, key: &str) -> Option<String> {
        self.entries.remove(key)
    }

    /// Persists the ledger, deleting the file when it is empty.
    pub(crate) async fn save(&self, cx: &Cx) -> Result<()> {
        if self.entries.is_empty() {
            cx.system.remove(&self.path, false).await?;
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&self.entries).map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "The ownership ledger could not be serialised.",
                &["This is a bug in Buckle; please report it."],
            )
        })?;
        cx.system
            .write_atomic(
                &self.path,
                &bytes,
                &buckle_core::system::FilePerms::mode(0o600),
            )
            .await
    }
}

/// Whether `path` is the same as what we last wrote, judged by digest. `None`
/// when there is no digest to compare against.
pub(crate) async fn file_matches_digest(
    cx: &Cx,
    path: &Path,
    digest: Option<&buckle_core::Digest>,
) -> Result<Option<bool>> {
    let Some(digest) = digest else {
        return Ok(None);
    };
    Ok(cx
        .system
        .read(path)
        .await?
        .map(|bytes| &buckle_core::Digest::of(&bytes) == digest))
}

#[cfg(test)]
pub(crate) mod testkit {
    use buckle_core::Digest;
    use buckle_core::model::RemovalPolicy;
    use buckle_core::plan::{ResourceKey, RevisionId};
    use buckle_core::state::ResourceRecord;
    use buckle_core::task::{Applied, Cx};
    use buckle_core::testing::{CxBuilder, FakeSystem};

    /// A context over a clone of `sys`, so the test can keep inspecting `sys`.
    pub fn cx(sys: &FakeSystem) -> Cx {
        CxBuilder::new(sys.clone()).module("web").build()
    }

    /// The record the engine would persist for an apply result.
    pub fn record_of(kind: &str, key: &str, applied: &Applied) -> ResourceRecord {
        ResourceRecord {
            key: ResourceKey::new("web", kind, key),
            spec_digest: Digest::of(b"spec"),
            shadow: applied.shadow.clone(),
            content_digest: applied.content_digest.clone(),
            artefacts: applied.artefacts.clone(),
            removal: RemovalPolicy::Cleanup,
            revision: RevisionId::new("r1"),
            applied_at: chrono::Utc::now(),
            introduced_in: RevisionId::new("r1"),
        }
    }

    /// Parses a spec from JSON, like the engine does after rendering.
    pub fn spec<T: serde::de::DeserializeOwned>(json: serde_json::Value) -> T {
        serde_json::from_value(json).expect("test spec parses")
    }
}

#[cfg(test)]
mod tests {
    use buckle_core::task::lookup;
    use buckle_core::{Platform, Platforms};
    use serde_json::json;

    /// Every kind this lane owns, with a minimal valid spec.
    fn kinds() -> Vec<(&'static str, serde_json::Value, &'static [&'static str])> {
        vec![
            (
                "apt.repository",
                json!({"name": "x", "uri": "https://x.example", "suites": ["s"]}),
                &["apt"],
            ),
            ("apt.package", json!({"name": "nginx"}), &["apt"]),
            ("apt.update", json!({}), &["apt"]),
            (
                "systemd.unit",
                json!({"name": "nomad", "enabled": true}),
                &["systemd"],
            ),
            (
                "systemd.dropin",
                json!({"unit": "nomad.service", "content": "[Service]\n"}),
                &["systemd"],
            ),
            ("systemd.daemon_reload", json!({}), &["systemd"]),
            ("systemd.restart", json!({"name": "nomad"}), &["systemd"]),
            ("systemd.reload", json!({"name": "nomad"}), &["systemd"]),
            (
                "sysctl.param",
                json!({"name": "vm.swappiness", "value": 10}),
                &["sysctl"],
            ),
            (
                "ufw.default",
                json!({"direction": "incoming", "policy": "deny"}),
                &["ufw"],
            ),
            (
                "ufw.rule",
                json!({"rule": "allow", "to_port": 22}),
                &["ufw"],
            ),
            ("ufw.state", json!({"enabled": true}), &["ufw"]),
        ]
    }

    #[test]
    fn every_kind_is_registered_linux_only_documented_and_has_a_schema() {
        for (kind, spec, locks) in kinds() {
            let task = lookup(kind).unwrap_or_else(|e| panic!("{kind}: {}", e.description()));
            assert_eq!(task.locks(), locks, "{kind}");
            assert_eq!(task.platforms(), Platforms::LINUX, "{kind}");
            assert!(!task.platforms().contains(Platform::MacOs), "{kind}");
            assert!(!task.description().is_empty(), "{kind} has no description");
            let parsed = task
                .parse(spec)
                .unwrap_or_else(|e| panic!("{kind}: {}", e.description()));
            assert!(!task.key(&parsed).is_empty(), "{kind} has an empty key");
            assert!(!task.describe(&parsed).is_empty(), "{kind}");
            let schema = serde_json::to_value(task.schema()).unwrap();
            assert!(schema.is_object(), "{kind}");
        }
    }

    #[test]
    fn unknown_fields_are_rejected_with_the_kind_named() {
        for (kind, mut spec, _) in kinds() {
            spec.as_object_mut()
                .unwrap()
                .insert("bogus".into(), json!(1));
            let err = lookup(kind).unwrap().parse(spec).unwrap_err();
            assert!(
                err.description().contains(kind),
                "{kind}: {}",
                err.description()
            );
        }
    }

    #[test]
    fn handlers_default_to_leave_and_packages_carry_a_removal_caution() {
        use buckle_core::model::RemovalPolicy;
        for kind in [
            "systemd.daemon_reload",
            "systemd.restart",
            "systemd.reload",
            "apt.update",
        ] {
            assert_eq!(
                lookup(kind).unwrap().removal_default(),
                RemovalPolicy::Leave,
                "{kind}"
            );
        }
        assert_eq!(
            lookup("apt.package").unwrap().removal_default(),
            RemovalPolicy::Cleanup
        );
        assert!(lookup("apt.package").unwrap().removal_caution());
        assert!(!lookup("ufw.rule").unwrap().removal_caution());
    }
}
