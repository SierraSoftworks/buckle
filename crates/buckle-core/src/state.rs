//! What the agent remembers: every resource it applied, with the redacted
//! shadow and content digest that make diffs and drift detection safe, plus
//! the runs it performed.

use crate::Digest;
use crate::model::RemovalPolicy;
use crate::plan::{ResourceKey, RevisionId};
use crate::secrets::Redacted;
use crate::task::Artefact;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRecord {
    pub key: ResourceKey,
    pub spec_digest: Digest,
    /// Redacted render of written content, for future plan diffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Redacted<String>>,
    /// Digest of the real content written, compared with the file on disk to detect drift.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<Digest>,
    #[serde(default)]
    pub artefacts: Vec<Artefact>,
    pub removal: RemovalPolicy,
    pub revision: RevisionId,
    pub applied_at: chrono::DateTime<chrono::Utc>,
    /// The revision that first created this resource.
    pub introduced_in: RevisionId,
}

impl ResourceRecord {
    /// Everything this resource is known to own (see [`Artefact::identity`]
    /// and [`crate::task::key_identity`]).
    pub fn identities(&self) -> std::collections::BTreeSet<String> {
        let mut ids: std::collections::BTreeSet<String> = crate::task::key_identity(&self.key.key)
            .into_iter()
            .chain(self.artefacts.iter().filter_map(Artefact::identity))
            .collect();
        for a in &self.artefacts {
            if let (Artefact::Line { path, .. }, Some(line)) = (a, a.line_text()) {
                ids.insert(format!("line-text:{}:{line}", path.to_string_lossy()));
            }
        }
        ids
    }
}

/// Outcome of a run for the local history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub revision: RevisionId,
    pub plan_digest: Digest,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    pub outcome: RunOutcome,
    pub changes: usize,
    pub removals: usize,
    #[serde(default)]
    pub failed_module: Option<String>,
    #[serde(default)]
    pub rolled_back_modules: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Succeeded,
    Failed,
    Unhealthy,
    RolledBack,
    /// Nothing failed, but steps stayed blocked: the revision is not fully
    /// applied and an operator needs to look.
    NeedsAttention,
}

/// The persisted state document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub resources: BTreeMap<String, ResourceRecord>,
    /// The revision this host currently has applied (last successful run).
    #[serde(default)]
    pub applied_revision: Option<RevisionId>,
    /// The fleet's stable revision as last told by the server; used for local rollback.
    #[serde(default)]
    pub stable_revision: Option<RevisionId>,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
    /// Cached facts with collection times.
    #[serde(default)]
    pub facts: crate::facts::Facts,
    /// The revision each module last applied successfully (or was rolled back
    /// to). A failed run leaves `applied_revision` behind while the modules
    /// that succeeded move on; drift uses this to tell them apart.
    #[serde(default)]
    pub module_revisions: BTreeMap<String, RevisionId>,
    /// Modules whose most recent run failed, with the revision they failed
    /// on; cleared when the module next applies successfully.
    #[serde(default)]
    pub failed_modules: BTreeMap<String, RevisionId>,
    /// The handlers (`module:handler`) each resource's task notified, keyed
    /// like `resources`; a removal runs them (design §04).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notifies: BTreeMap<String, Vec<String>>,
}

impl State {
    pub fn record(&mut self, record: ResourceRecord) {
        self.resources.insert(record.key.to_string(), record);
    }

    pub fn get(&self, key: &ResourceKey) -> Option<&ResourceRecord> {
        self.resources.get(&key.to_string())
    }

    pub fn forget(&mut self, key: &ResourceKey) -> Option<ResourceRecord> {
        self.notifies.remove(&key.to_string());
        self.resources.remove(&key.to_string())
    }

    /// The handlers (`module:handler`) the task of `key` notified.
    pub fn notifies_of(&self, key: &ResourceKey) -> &[String] {
        self.notifies
            .get(&key.to_string())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Resources in state that are not in `desired`.
    pub fn orphans<'a>(
        &'a self,
        desired: &'a [ResourceKey],
    ) -> impl Iterator<Item = &'a ResourceRecord> + 'a {
        self.resources
            .values()
            .filter(move |r| !desired.contains(&r.key))
    }

    /// Whether `module` is applied at `revision` on this host: its recorded
    /// module revision is `revision` (or, for state written before module
    /// revisions were recorded, the host's applied revision is) and its last
    /// run did not fail.
    pub fn module_at(&self, module: &str, revision: &RevisionId) -> bool {
        if self.failed_modules.contains_key(module) {
            return false;
        }
        match self.module_revisions.get(module) {
            Some(r) => r == revision,
            None => self.applied_revision.as_ref() == Some(revision),
        }
    }

    /// Appends a run, keeping the most recent `keep`.
    pub fn push_run(&mut self, run: RunRecord, keep: usize) {
        self.runs.push(run);
        if self.runs.len() > keep {
            let drop = self.runs.len() - keep;
            self.runs.drain(..drop);
        }
    }
}

/// Persistence for [`State`].
pub trait StateStore: Send + Sync {
    fn load(&self) -> crate::Result<State>;
    fn save(&self, state: &State) -> crate::Result<()>;
}

/// A single CBOR file, written atomically: a `0600` temp file
/// (`.<name>.buckle-tmp-<pid>`) in the same directory, fsynced, renamed over
/// the state file, then the directory fsynced. Leftover temp files of a
/// process that died mid-write are removed on load.
#[derive(Debug, Clone)]
pub struct FileStateStore {
    path: PathBuf,
}

impl FileStateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn temp_prefix(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        format!(".{name}.buckle-tmp-")
    }

    fn temp_path(&self) -> PathBuf {
        self.path
            .with_file_name(format!("{}{}", self.temp_prefix(), std::process::id()))
    }

    /// Removes temp files a crashed earlier write left next to the state file.
    fn remove_leftovers(&self) {
        let Some(dir) = self.path.parent() else {
            return;
        };
        crate::system::remove_temp_leftovers(dir, &self.temp_prefix());
    }
}

/// fsyncs a directory so that a rename inside it is durable (no-op where
/// directories cannot be opened, e.g. Windows).
pub(crate) fn sync_dir(dir: Option<&Path>) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = dir {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        std::fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

impl StateStore for FileStateStore {
    fn load(&self) -> crate::Result<State> {
        use crate::error::HumanizableResult;
        self.remove_leftovers();
        match std::fs::read(&self.path) {
            Ok(bytes) => ciborium::from_reader(bytes.as_slice()).map_err(|e| {
                crate::error::wrap_system(e, format!("The Buckle state file at {} is corrupt.", self.path.display()), &[
                    "Move the file aside to start with empty state; Buckle will re-adopt managed resources on the next apply.",
                ])
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(e) => Err(e).to_human_error(),
        }
    }

    fn save(&self, state: &State) -> crate::Result<()> {
        use crate::error::HumanizableResult;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).to_human_error()?;
        }
        let tmp = self.temp_path();
        let mut bytes = Vec::new();
        ciborium::into_writer(state, &mut bytes).map_err(|e| {
            crate::error::wrap_system(
                e,
                "Buckle could not serialise its state.",
                &["This is a bug; please report it."],
            )
        })?;
        let written = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, &self.path)?;
            sync_dir(self.path.parent())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written.to_human_error()
    }
}

/// An in-memory store for tests and plan-only runs.
#[derive(Debug, Default)]
pub struct MemoryStateStore {
    state: std::sync::Mutex<State>,
}

impl MemoryStateStore {
    pub fn with(state: State) -> Self {
        Self {
            state: std::sync::Mutex::new(state),
        }
    }
}

impl StateStore for MemoryStateStore {
    fn load(&self) -> crate::Result<State> {
        Ok(self.state.lock().expect("state mutex").clone())
    }

    fn save(&self, state: &State) -> crate::Result<()> {
        *self.state.lock().expect("state mutex") = state.clone();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(module: &str, key: &str) -> ResourceRecord {
        ResourceRecord {
            key: ResourceKey::new(module, "file.copy", key),
            spec_digest: Digest::of(b"spec"),
            shadow: Some(Redacted::plain("content".into())),
            content_digest: Some(Digest::of(b"content")),
            artefacts: vec![Artefact::File { path: key.into() }],
            removal: RemovalPolicy::Cleanup,
            revision: RevisionId::new("r2"),
            applied_at: chrono::Utc::now(),
            introduced_in: RevisionId::new("r1"),
        }
    }

    #[test]
    fn file_store_round_trips_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStateStore::new(dir.path().join("nested/state.cbor"));
        assert_eq!(store.load().unwrap(), State::default());
        let mut state = State::default();
        state.record(record("m", "/etc/a"));
        state.applied_revision = Some(RevisionId::new("r2"));
        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.resources.len(), 1);
        assert_eq!(loaded.applied_revision, Some(RevisionId::new("r2")));
        let names: Vec<String> = std::fs::read_dir(dir.path().join("nested"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["state.cbor"], "temp file is renamed away");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn leftover_temp_files_of_a_crashed_write_are_removed_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileStateStore::new(dir.path().join("state.cbor"));
        store.save(&State::default()).unwrap();
        let leftover = dir.path().join(".state.cbor.buckle-tmp-999999");
        std::fs::write(&leftover, b"half a state").unwrap();
        let unrelated = dir.path().join(".other.buckle-tmp-1");
        std::fs::write(&unrelated, b"x").unwrap();
        store.load().unwrap();
        assert!(!leftover.exists());
        assert!(
            unrelated.exists(),
            "only this store's temp files are touched"
        );
    }

    #[test]
    fn corrupt_state_is_a_system_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.cbor");
        std::fs::write(&path, b"\xff\xff not cbor").unwrap();
        let err = FileStateStore::new(&path).load().unwrap_err();
        assert!(err.is(crate::error::Kind::System));
    }

    #[test]
    fn orphans_are_resources_outside_the_desired_set() {
        let mut state = State::default();
        state.record(record("m", "/keep"));
        state.record(record("m", "/gone"));
        let desired = vec![ResourceKey::new("m", "file.copy", "/keep")];
        let orphans: Vec<_> = state.orphans(&desired).map(|r| r.key.key.clone()).collect();
        assert_eq!(orphans, vec!["/gone"]);
    }

    #[test]
    fn run_history_is_bounded() {
        let mut state = State::default();
        for i in 0..5 {
            state.push_run(
                RunRecord {
                    revision: RevisionId::new(format!("r{i}")),
                    plan_digest: Digest::of(b"p"),
                    started_at: chrono::Utc::now(),
                    finished_at: chrono::Utc::now(),
                    outcome: RunOutcome::Succeeded,
                    changes: 0,
                    removals: 0,
                    failed_module: None,
                    rolled_back_modules: vec![],
                    error: None,
                },
                3,
            );
        }
        assert_eq!(state.runs.len(), 3);
        assert_eq!(state.runs[0].revision, RevisionId::new("r2"));
    }
}
