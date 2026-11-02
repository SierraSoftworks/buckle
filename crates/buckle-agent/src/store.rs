//! What the agent keeps in its state directory:
//!
//! | path | content |
//! |---|---|
//! | `token` | the per-host token issued at enrolment (0600) |
//! | `install.key` | the key secret fingerprints are computed with (0600) |
//! | `state.cbor` | the engine's resource state, applied and stable revision |
//! | `agent.json` | session bookkeeping: last plan digest, labels, the local rollback watchdog, unreported local rollbacks (0600) |
//! | `revisions/<id>/` | extracted revision trees (the stable one is kept for local rollback) |
//! | `secrets/<id>.cbor` | the secrets scoped to this host for a revision (0600) |

use crate::transport::LocalRollbackRecord;
use crate::watchdog::WatchdogState;
use buckle_core::error::HumanizableResult;
use buckle_core::plan::RevisionId;
use buckle_core::secrets::cache::SecretCache;
use buckle_core::secrets::sinks::TransportSink;
use buckle_core::secrets::{InstallKey, SecretStore};
use buckle_core::state::{FileStateStore, State, StateStore};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Session bookkeeping persisted in `agent.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentState {
    /// Digest of the last plan this agent produced.
    #[serde(default)]
    pub last_plan_digest: Option<String>,
    /// Effective labels from the last Welcome.
    #[serde(default)]
    pub labels: IndexMap<String, String>,
    /// Host status from the last Welcome (`pending`, `active`, `paused`).
    #[serde(default)]
    pub status: Option<String>,
    /// The armed local rollback watchdog, if any.
    #[serde(default)]
    pub watchdog: Option<WatchdogState>,
    /// Local rollbacks not yet reported to a server.
    #[serde(default)]
    pub pending_rollbacks: Vec<LocalRollbackRecord>,
}

/// Writes `bytes` to `path` atomically with mode 0600.
pub fn write_private(path: &Path, bytes: &[u8]) -> buckle_core::Result<()> {
    write_atomic(path, bytes, 0o600)
}

/// Writes `bytes` to `path` atomically (temp file + rename) with `mode` on Unix.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> buckle_core::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).to_human_error()?;
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        let mut file = options.open(&tmp).to_human_error()?;
        file.write_all(bytes).to_human_error()?;
        file.sync_all().to_human_error()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode)).to_human_error()?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path).to_human_error()
}

/// Revision ids become directory names, so only a conservative alphabet is allowed.
pub fn validate_revision_id(id: &RevisionId) -> buckle_core::Result<()> {
    let ok = !id.0.is_empty()
        && id.0.len() <= 128
        && !id.0.starts_with('.')
        && id
            .0
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(buckle_core::error::user(
            format!("'{}' is not a valid revision id.", id.0),
            &[
                "Revision ids are commit hashes or content digests: letters, digits, '-', '_' and '.'.",
            ],
        ))
    }
}

/// The agent's state directory.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The store rooted at `state_dir`.
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: state_dir.into(),
        }
    }

    /// The state directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates the state directory (0700) and its subdirectories.
    pub fn ensure(&self) -> buckle_core::Result<()> {
        for dir in [
            self.root.clone(),
            self.root.join("revisions"),
            self.root.join("secrets"),
        ] {
            std::fs::create_dir_all(&dir).map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("The agent could not create its state directory {}.", dir.display()),
                    &["Run the agent as root, or point `state_dir` in agent.yaml at a writable directory."],
                )
            })?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&self.root, &self.root.join("secrets")] {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                    .to_human_error()?;
            }
        }
        Ok(())
    }

    /// Where the per-host token lives.
    pub fn token_path(&self) -> PathBuf {
        self.root.join("token")
    }

    /// The per-host token, if the host has enrolled.
    pub fn read_token(&self) -> buckle_core::Result<Option<String>> {
        match std::fs::read_to_string(self.token_path()) {
            Ok(t) if !t.trim().is_empty() => Ok(Some(t.trim().to_owned())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).to_human_error(),
        }
    }

    /// Persists the per-host token (0600).
    pub fn write_token(&self, token: &str) -> buckle_core::Result<()> {
        write_private(&self.token_path(), token.as_bytes())
    }

    fn agent_state_path(&self) -> PathBuf {
        self.root.join("agent.json")
    }

    /// Loads `agent.json`; a missing or unreadable file is empty state.
    pub fn load_agent_state(&self) -> AgentState {
        match std::fs::read(self.agent_state_path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                tracing::warn!(error = %err, "agent.json is unreadable; starting with empty session state");
                AgentState::default()
            }),
            Err(_) => AgentState::default(),
        }
    }

    /// Saves `agent.json` (0600).
    pub fn save_agent_state(&self, state: &AgentState) -> buckle_core::Result<()> {
        let bytes = serde_json::to_vec_pretty(state).map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "The agent could not serialise its session state.",
                &["This is a bug; please report it."],
            )
        })?;
        write_private(&self.agent_state_path(), &bytes)
    }

    /// The engine's state store.
    pub fn state_store(&self) -> FileStateStore {
        FileStateStore::new(self.root.join("state.cbor"))
    }

    /// Loads the engine state.
    pub fn load_state(&self) -> buckle_core::Result<State> {
        self.state_store().load()
    }

    /// Loads, modifies and saves the engine state.
    pub fn update_state(&self, f: impl FnOnce(&mut State)) -> buckle_core::Result<()> {
        let store = self.state_store();
        let mut state = store.load()?;
        f(&mut state);
        store.save(&state)
    }

    /// The directory a revision's tree is extracted to.
    pub fn revision_dir(&self, id: &RevisionId) -> buckle_core::Result<PathBuf> {
        validate_revision_id(id)?;
        Ok(self.root.join("revisions").join(&id.0))
    }

    /// Whether a revision's tree is cached.
    pub fn has_revision(&self, id: &RevisionId) -> bool {
        self.revision_dir(id)
            .map(|d| d.join("buckle.yaml").is_file())
            .unwrap_or(false)
    }

    /// Deletes cached revision trees and secrets except those in `keep`.
    pub fn prune_revisions(&self, keep: &[&RevisionId]) {
        let Ok(entries) = std::fs::read_dir(self.root.join("revisions")) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || keep.iter().any(|k| k.0 == name) {
                continue;
            }
            tracing::debug!(revision = %name, "pruning cached revision");
            let _ = std::fs::remove_dir_all(entry.path());
            let _ = std::fs::remove_file(self.root.join("secrets").join(format!("{name}.cbor")));
        }
    }

    fn secrets_path(&self, id: &RevisionId) -> buckle_core::Result<PathBuf> {
        validate_revision_id(id)?;
        Ok(self.root.join("secrets").join(format!("{}.cbor", id.0)))
    }

    /// Caches a revision's secrets ([`SecretCache`]: CBOR, HMAC-tagged under
    /// the install key, 0600, atomic).
    pub fn save_secrets(
        &self,
        id: &RevisionId,
        secrets: &SecretStore,
        key: &InstallKey,
    ) -> buckle_core::Result<()> {
        SecretCache::save(&self.secrets_path(id)?, secrets, key, &TransportSink)
    }

    /// The cached secrets of a revision, if any.
    pub fn load_secrets(
        &self,
        id: &RevisionId,
        key: &InstallKey,
    ) -> buckle_core::Result<Option<SecretStore>> {
        SecretCache::load(&self.secrets_path(id)?, key)
    }

    /// The install key used to fingerprint secrets (created on first use).
    pub fn install_key(&self) -> buckle_core::Result<InstallKey> {
        InstallKey::load_or_create(&self.root.join("install.key"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    #[test]
    fn tokens_and_secrets_are_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path());
        store.ensure().expect("ensure");
        assert_eq!(store.read_token().expect("read"), None);
        store.write_token("host-token").expect("write");
        assert_eq!(
            store.read_token().expect("read").as_deref(),
            Some("host-token")
        );
        let rev = RevisionId::new("abc123");
        let key = store.install_key().expect("key");
        let mut secrets = SecretStore::new();
        secrets.insert(buckle_core::secrets::Secret::new("db", "pw".into(), &key));
        store.save_secrets(&rev, &secrets, &key).expect("write");
        let loaded = store
            .load_secrets(&rev, &key)
            .expect("read")
            .expect("cached");
        assert_eq!(loaded.get("db"), secrets.get("db"));
        assert!(
            store
                .load_secrets(&RevisionId::new("other"), &key)
                .expect("read")
                .is_none()
        );
        #[cfg(unix)]
        {
            assert_eq!(mode(&store.token_path()), 0o600);
            assert_eq!(mode(&dir.path().join("secrets/abc123.cbor")), 0o600);
            assert_eq!(mode(&dir.path().join("secrets")), 0o700);
        }
    }

    #[rstest]
    #[case("7c2e9a1", true)]
    #[case("local-1.2_x", true)]
    #[case("../etc", false)]
    #[case("a/b", false)]
    #[case("", false)]
    #[case(".hidden", false)]
    fn revision_ids_are_safe_directory_names(#[case] id: &str, #[case] ok: bool) {
        assert_eq!(validate_revision_id(&RevisionId::new(id)).is_ok(), ok);
    }

    #[test]
    fn agent_state_round_trips_and_tolerates_corruption() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::new(dir.path());
        let state = AgentState {
            last_plan_digest: Some("d".into()),
            ..Default::default()
        };
        store.save_agent_state(&state).expect("save");
        assert_eq!(store.load_agent_state(), state);
        std::fs::write(dir.path().join("agent.json"), b"{nope").expect("write");
        assert_eq!(store.load_agent_state(), AgentState::default());
    }
}
