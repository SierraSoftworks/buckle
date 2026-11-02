//! The source watcher (design §09): turns commits, or changes to a local
//! directory, into stored revisions.
//!
//! Each new commit is exported to `data_dir/revisions/<id>/`, loaded and
//! validated. Valid revisions get a deterministic tarball (what agents
//! fetch) and an index of their files by digest (for `FetchBlob`); invalid
//! ones are stored as rejected with their errors so the UI can show what
//! broke while the fleet stays on the last valid revision.

pub mod cache;
pub mod git;
pub mod path;
pub mod tarball;
pub mod validate;

use crate::events::EventBus;
use crate::store::{RevisionRecord, Store};
use buckle_api::{LiveEvent, RevisionStatus, RevisionSummary};
use buckle_core::plan::RevisionId;
use buckle_core::{Digest, Result};
use cache::RevisionCache;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Metadata of the commit a revision was built from.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: String,
    pub author: String,
    pub message: String,
    pub time: DateTime<Utc>,
}

/// Called with the id of every newly stored revision (valid or not).
pub type RevisionCallback = Arc<dyn Fn(&RevisionRecord) + Send + Sync>;

/// Builds and stores revisions from exported trees.
#[derive(Clone)]
pub struct RevisionBuilder {
    store: Store,
    cache: Arc<RevisionCache>,
    events: EventBus,
    on_added: Option<RevisionCallback>,
}

impl std::fmt::Debug for RevisionBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevisionBuilder").finish_non_exhaustive()
    }
}

impl RevisionBuilder {
    /// A builder writing into `cache`'s directory.
    pub fn new(store: Store, cache: Arc<RevisionCache>, events: EventBus) -> Self {
        Self {
            store,
            cache,
            events,
            on_added: None,
        }
    }

    /// Calls `f` for every revision stored.
    pub fn on_added(mut self, f: RevisionCallback) -> Self {
        self.on_added = Some(f);
        self
    }

    /// The revision cache.
    pub fn cache(&self) -> &Arc<RevisionCache> {
        &self.cache
    }

    /// Whether a revision with this id is already stored.
    pub async fn exists(&self, id: &str) -> Result<bool> {
        let id = id.to_owned();
        self.store
            .call(move |db| Ok(db.revisions().get(&id)?.is_some()))
            .await
    }

    /// Builds a revision from `tree` (which is moved under the revision
    /// cache) and stores it. Returns the stored record.
    pub async fn build(
        &self,
        commit: CommitInfo,
        tree: PathBuf,
        source: &str,
    ) -> Result<RevisionRecord> {
        let cache = self.cache.clone();
        let id = commit.id.clone();
        let source = source.to_owned();
        let (record, revision, blobs) = tokio::task::spawn_blocking(move || -> Result<_> {
            let dir = cache.dir(&id);
            if dir != tree {
                if dir.exists() {
                    std::fs::remove_dir_all(&dir).map_err(io_error)?;
                }
                if let Some(parent) = dir.parent() {
                    std::fs::create_dir_all(parent).map_err(io_error)?;
                }
                std::fs::rename(&tree, &dir).map_err(io_error)?;
            }
            let now = Utc::now();
            let mut record = RevisionRecord {
                id: id.clone(),
                commit: id.clone(),
                author: commit.author.clone(),
                message: commit.message.clone(),
                committed_at: commit.time,
                created_at: now,
                tarball_digest: None,
                status: RevisionStatus::Rejected,
                errors: Vec::new(),
                module_count: 0,
                source,
            };
            match buckle_core::engine::loader::load_dir(&dir, RevisionId::new(id.clone())) {
                Err(e) => {
                    record.errors = vec![error_text(&e)];
                    Ok((record, None, Vec::new()))
                }
                Ok(revision) => {
                    record.module_count = revision.modules.len() as u32;
                    let errors = validate::validate(&revision);
                    if !errors.is_empty() {
                        record.errors = errors;
                        return Ok((record, None, Vec::new()));
                    }
                    let digest = tarball::create(&dir, &cache.tarball(&id))?;
                    record.tarball_digest = Some(digest.to_string());
                    record.status = RevisionStatus::Valid;
                    let mut blobs = Vec::new();
                    for rel in tarball::files(&dir)? {
                        let bytes = std::fs::read(dir.join(&rel)).map_err(io_error)?;
                        blobs.push((
                            Digest::of(&bytes).to_string(),
                            rel.to_string_lossy().replace('\\', "/"),
                        ));
                    }
                    Ok((record, Some(revision), blobs))
                }
            }
        })
        .await
        .map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "Building a revision panicked.",
                &["Report this issue."],
            )
        })??;
        if let Some(revision) = revision {
            self.cache.insert(&record.id, Arc::new(revision));
        }
        let rec = record.clone();
        self.store
            .transaction(move |db| {
                db.revisions().upsert(&rec)?;
                for (digest, path) in &blobs {
                    db.revisions().add_blob(digest, &rec.id, path)?;
                }
                Ok(())
            })
            .await?;
        tracing::info!(revision = %record.id, status = ?record.status, errors = record.errors.len(), "revision stored");
        self.events.publish(LiveEvent::RevisionAdded {
            revision: summary(&record, false, None),
        });
        if let Some(f) = &self.on_added {
            f(&record);
        }
        Ok(record)
    }
}

/// The UI view of a revision.
pub fn summary(
    record: &RevisionRecord,
    is_stable: bool,
    rollout_id: Option<String>,
) -> RevisionSummary {
    RevisionSummary {
        id: record.id.clone(),
        commit: record.commit.clone(),
        author: record.author.clone(),
        message: record.message.clone(),
        created_at: record.created_at,
        status: record.status,
        validation_errors: record.errors.clone(),
        is_stable,
        rollout_id,
        module_count: record.module_count,
    }
}

fn error_text(e: &buckle_core::Error) -> String {
    let mut text = e.description().to_string();
    let mut cause: Option<&dyn std::error::Error> = std::error::Error::source(e);
    while let Some(c) = cause {
        text.push_str(": ");
        text.push_str(&c.to_string());
        cause = c.source();
    }
    text
}

fn io_error(e: std::io::Error) -> buckle_core::Error {
    buckle_core::error::wrap_system(
        e,
        "A revision could not be written to the data directory.",
        &["Check the server's data directory for disk space and permissions."],
    )
}

/// Verifies a GitHub-style `X-Hub-Signature-256: sha256=<hex>` header over
/// `body` in constant time.
pub fn verify_webhook_signature(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(hex_sig) = header.trim().strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// A scratch directory under the revisions root for an export in progress.
pub(crate) fn scratch_dir(cache: &RevisionCache, id: &str) -> PathBuf {
    cache
        .root()
        .join(format!(".tmp-{}-{}", sanitize(id), uuid::Uuid::new_v4()))
}

fn sanitize(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(40)
        .collect()
}

/// Copies every file under `from` (except `.git`) into `to`.
pub(crate) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for rel in tarball::files(from)? {
        let dest = to.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        std::fs::copy(from.join(&rel), &dest).map_err(io_error)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[rstest]
    #[case("s3cret", true)]
    #[case("wrong", false)]
    fn webhook_signatures_are_checked(#[case] signer: &str, #[case] ok: bool) {
        let body = br#"{"ref":"refs/heads/main"}"#;
        assert_eq!(
            verify_webhook_signature("s3cret", body, &sign(signer, body)),
            ok
        );
    }

    #[rstest]
    #[case("")]
    #[case("sha1=abcd")]
    #[case("sha256=nothex")]
    fn malformed_signatures_are_rejected(#[case] header: &str) {
        assert!(!verify_webhook_signature("s", b"x", header));
    }

    #[tokio::test]
    async fn broken_revisions_are_stored_as_rejected_with_their_errors() {
        let data = tempfile::tempdir().unwrap();
        let store = Store::in_memory().await.unwrap();
        let cache = Arc::new(RevisionCache::new(data.path()));
        let builder = RevisionBuilder::new(store.clone(), cache.clone(), EventBus::default());
        let tree = data.path().join("tree");
        std::fs::create_dir_all(tree.join("modules/a")).unwrap();
        std::fs::write(tree.join("buckle.yaml"), "version: 2\n").unwrap();
        std::fs::write(
            tree.join("modules/a/module.yaml"),
            "name: a\ntasks: {{{ broken",
        )
        .unwrap();
        let rec = builder
            .build(
                CommitInfo {
                    id: "abc123".into(),
                    author: "Ada".into(),
                    message: "break it".into(),
                    time: Utc::now(),
                },
                tree,
                "git",
            )
            .await
            .unwrap();
        assert_eq!(rec.status, RevisionStatus::Rejected);
        assert!(
            rec.errors[0].contains("modules/a/module.yaml"),
            "{:?}",
            rec.errors
        );
        assert!(rec.tarball_digest.is_none());
        let stored = store
            .call(|db| db.revisions().get("abc123"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.errors, rec.errors);
    }
}
