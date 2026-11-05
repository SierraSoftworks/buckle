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
use crate::store::{RevisionRecord, Store, kv::keys};
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

    /// Announces an already stored revision again, as if it were new (the
    /// branch moved back to it). Returns its record, if it is stored.
    pub async fn reannounce(&self, id: &str) -> Result<Option<RevisionRecord>> {
        let id = id.to_owned();
        let record = self.store.call(move |db| db.revisions().get(&id)).await?;
        if let Some(record) = &record
            && let Some(f) = &self.on_added
        {
            f(record);
        }
        Ok(record)
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
        let (record, outcome) = blocking("Building a revision", move || {
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
            let mut record = RevisionRecord {
                id: id.clone(),
                commit: id.clone(),
                author: commit.author.clone(),
                message: commit.message.clone(),
                committed_at: commit.time,
                created_at: Utc::now(),
                tarball_digest: None,
                status: RevisionStatus::Rejected,
                errors: Vec::new(),
                module_count: 0,
                source,
                validated_by: None,
            };
            let outcome = validate_tree(&cache, &mut record)?;
            Ok((record, outcome))
        })
        .await?;
        self.store_validated(record, outcome, true).await
    }

    /// Validates a rejected revision's tree again with this server's rules
    /// (a newer server may accept what an older one refused). The revision
    /// is announced when it becomes valid and `announce` is set; a valid
    /// revision is returned unchanged.
    pub async fn revalidate(&self, id: &str, announce: bool) -> Result<RevisionRecord> {
        let i = id.to_owned();
        let Some(mut record) = self.store.call(move |db| db.revisions().get(&i)).await? else {
            return Err(buckle_core::error::user(
                format!("No revision '{id}'."),
                &["Check the revision id."],
            ));
        };
        if record.status == RevisionStatus::Valid {
            return Ok(record);
        }
        let cache = self.cache.clone();
        let (record, outcome) = blocking("Validating a revision", move || {
            if !cache.dir(&record.id).join("buckle.yaml").is_file() {
                return Err(buckle_core::error::user(
                    format!("The tree of revision '{}' is no longer on disk.", record.id),
                    &["Push a new commit to build the configuration again."],
                ));
            }
            let outcome = validate_tree(&cache, &mut record)?;
            Ok((record, outcome))
        })
        .await?;
        self.store_validated(record, outcome, announce).await
    }

    /// Validates again every rejected revision an older server version
    /// validated, newest first. Only the branch tip (the last commit the
    /// Git source processed, else the newest revision) is announced, so an
    /// upgrade never starts a rollout of a superseded commit.
    pub async fn revalidate_stale(&self) -> Result<()> {
        let stale = self
            .store
            .call(|db| db.revisions().rejected_unless_validated_by(VALIDATOR))
            .await?;
        let tip = self.tip().await?;
        for rec in stale {
            let announce = tip.as_deref() == Some(rec.id.as_str());
            match self.revalidate(&rec.id, announce).await {
                Ok(r) if r.status == RevisionStatus::Valid => {
                    tracing::info!(revision = %r.id, "a rejected revision is valid for this server version")
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(revision = %rec.id, error = %e, "a rejected revision could not be validated again")
                }
            }
        }
        Ok(())
    }

    /// The revision the source points at: the last commit the Git source
    /// processed, else the newest revision.
    pub async fn tip(&self) -> Result<Option<String>> {
        self.store
            .call(|db| {
                Ok(match db.kv().get(keys::LAST_COMMIT)? {
                    Some(last) => Some(last),
                    None => db.revisions().list(1, 0)?.pop().map(|r| r.id),
                })
            })
            .await
    }

    /// Stores a validated record (with its blobs), publishes it and, when
    /// `announce` is set, hands it to the `on_added` callback.
    async fn store_validated(
        &self,
        record: RevisionRecord,
        outcome: Outcome,
        announce: bool,
    ) -> Result<RevisionRecord> {
        let Outcome { revision, blobs } = outcome;
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
        if announce && let Some(f) = &self.on_added {
            f(&record);
        }
        Ok(record)
    }
}

/// The server version recorded on each revision it validates.
pub const VALIDATOR: &str = env!("CARGO_PKG_VERSION");

/// What validating a tree produced besides its record.
struct Outcome {
    revision: Option<buckle_core::engine::Revision>,
    blobs: Vec<(String, String)>,
}

/// Loads and validates the tree of `record` under `cache`, filling in its
/// status, errors and module count. A valid tree gets its tarball and the
/// index of its files.
fn validate_tree(cache: &RevisionCache, record: &mut RevisionRecord) -> Result<Outcome> {
    let dir = cache.dir(&record.id);
    record.validated_by = Some(VALIDATOR.to_owned());
    record.status = RevisionStatus::Rejected;
    record.tarball_digest = None;
    record.errors.clear();
    let none = || Outcome {
        revision: None,
        blobs: Vec::new(),
    };
    let revision =
        match buckle_core::engine::loader::load_dir(&dir, RevisionId::new(record.id.clone())) {
            Err(e) => {
                record.errors = vec![error_text(&e)];
                return Ok(none());
            }
            Ok(r) => r,
        };
    record.module_count = revision.modules.len() as u32;
    let errors = validate::validate(&revision);
    if !errors.is_empty() {
        record.errors = errors;
        return Ok(none());
    }
    let digest = tarball::create(&dir, &cache.tarball(&record.id))?;
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
    Ok(Outcome {
        revision: Some(revision),
        blobs,
    })
}

/// Runs `f` on the blocking pool; `what` names it if it panics.
async fn blocking<T: Send + 'static>(
    what: &'static str,
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        buckle_core::error::wrap_system(e, format!("{what} panicked."), &["Report this issue."])
    })?
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
    // An empty key would make every signature computable by anyone.
    if secret.is_empty() {
        return false;
    }
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

    /// A valid tree stored as rejected by `validated_by`, `age` seconds old.
    async fn stored_rejected(
        store: &Store,
        cache: &RevisionCache,
        id: &str,
        validated_by: Option<&str>,
        age: i64,
    ) {
        let dir = cache.dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("buckle.yaml"), "version: 2\n").unwrap();
        let rec = RevisionRecord {
            id: id.into(),
            commit: id.into(),
            author: "Ada".into(),
            message: "uses a newer schema".into(),
            committed_at: Utc::now(),
            created_at: Utc::now() - chrono::Duration::seconds(age),
            tarball_digest: None,
            status: RevisionStatus::Rejected,
            errors: vec!["unknown field `resources`".into()],
            module_count: 0,
            source: "git".into(),
            validated_by: validated_by.map(str::to_owned),
        };
        store
            .call(move |db| db.revisions().upsert(&rec))
            .await
            .unwrap();
    }

    fn announcing(
        store: &Store,
        cache: &Arc<RevisionCache>,
    ) -> (RevisionBuilder, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let s = seen.clone();
        let builder = RevisionBuilder::new(store.clone(), cache.clone(), EventBus::default())
            .on_added(Arc::new(move |r| s.lock().unwrap().push(r.id.clone())));
        (builder, seen)
    }

    async fn status(store: &Store, id: &str) -> RevisionStatus {
        let id = id.to_owned();
        store
            .call(move |db| db.revisions().get(&id))
            .await
            .unwrap()
            .unwrap()
            .status
    }

    #[tokio::test]
    async fn an_upgrade_validates_stale_rejections_again_and_announces_only_the_tip() {
        let data = tempfile::tempdir().unwrap();
        let store = Store::in_memory().await.unwrap();
        let cache = Arc::new(RevisionCache::new(data.path()));
        stored_rejected(&store, &cache, "old", None, 60).await;
        stored_rejected(&store, &cache, "tip", Some("0.0.1"), 0).await;
        stored_rejected(&store, &cache, "current", Some(VALIDATOR), 30).await;
        let (builder, seen) = announcing(&store, &cache);

        builder.revalidate_stale().await.unwrap();

        assert_eq!(status(&store, "old").await, RevisionStatus::Valid);
        assert_eq!(status(&store, "tip").await, RevisionStatus::Valid);
        assert_eq!(
            status(&store, "current").await,
            RevisionStatus::Rejected,
            "a rejection by this version stands"
        );
        assert_eq!(*seen.lock().unwrap(), vec!["tip".to_owned()]);
        assert!(cache.tarball("tip").is_file(), "agents can fetch it");
    }

    #[rstest]
    #[case::announced(true)]
    #[case::quiet(false)]
    #[tokio::test]
    async fn a_manual_revalidation_rebuilds_a_rejected_revision(#[case] announce: bool) {
        let data = tempfile::tempdir().unwrap();
        let store = Store::in_memory().await.unwrap();
        let cache = Arc::new(RevisionCache::new(data.path()));
        stored_rejected(&store, &cache, "r1", Some(VALIDATOR), 0).await;
        let (builder, seen) = announcing(&store, &cache);

        let rec = builder.revalidate("r1", announce).await.unwrap();

        assert_eq!(rec.status, RevisionStatus::Valid);
        assert!(rec.errors.is_empty());
        assert_eq!(rec.validated_by.as_deref(), Some(VALIDATOR));
        assert_eq!(seen.lock().unwrap().len(), announce as usize);
    }

    #[tokio::test]
    async fn a_revision_without_its_tree_cannot_be_validated_again() {
        let data = tempfile::tempdir().unwrap();
        let store = Store::in_memory().await.unwrap();
        let cache = Arc::new(RevisionCache::new(data.path()));
        stored_rejected(&store, &cache, "gone", None, 0).await;
        std::fs::remove_dir_all(cache.dir("gone")).unwrap();
        let (builder, _) = announcing(&store, &cache);

        assert!(builder.revalidate("gone", true).await.is_err());
        assert!(builder.revalidate("missing", true).await.is_err());
        assert_eq!(status(&store, "gone").await, RevisionStatus::Rejected);
    }
}
