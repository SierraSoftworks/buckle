//! Revision trees on disk and a cache of loaded revisions.
//!
//! Each valid revision lives at `data_dir/revisions/<id>/` (the exported
//! tree) next to `data_dir/revisions/<id>.tar.gz` (what agents fetch).

use buckle_core::Result;
use buckle_core::engine::Revision;
use buckle_core::plan::RevisionId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Loaded revisions by id.
pub struct RevisionCache {
    root: PathBuf,
    loaded: Mutex<HashMap<String, Arc<Revision>>>,
}

impl std::fmt::Debug for RevisionCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RevisionCache")
            .field("root", &self.root)
            .finish()
    }
}

impl RevisionCache {
    /// Revisions under `data_dir/revisions`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("revisions"),
            loaded: Mutex::new(HashMap::new()),
        }
    }

    /// The directory holding every revision.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the tree of `id` is exported.
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(sanitize(id))
    }

    /// Where the tarball of `id` is stored.
    pub fn tarball(&self, id: &str) -> PathBuf {
        self.root.join(format!("{}.tar.gz", sanitize(id)))
    }

    /// Remembers an already-loaded revision.
    pub fn insert(&self, id: &str, revision: Arc<Revision>) {
        self.loaded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(sanitize(id), revision);
    }

    /// The revision, loading it from disk (extracting the tarball when the
    /// tree is missing) on first use. Cached under the same sanitised id
    /// that names its directory, so two spellings of an id cannot load one
    /// tree under different names.
    pub async fn load(&self, id: &str) -> Result<Arc<Revision>> {
        let key = sanitize(id);
        if key.is_empty() {
            return Err(buckle_core::error::user(
                format!("'{id}' is not a revision id."),
                &["Revision ids are commit ids or tree digests (letters and digits)."],
            ));
        }
        if let Some(r) = self
            .loaded
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .cloned()
        {
            return Ok(r);
        }
        let dir = self.dir(id);
        let tarball = self.tarball(id);
        let rev_id = key.clone();
        let revision = tokio::task::spawn_blocking(move || -> Result<Revision> {
            if !dir.join("buckle.yaml").is_file() && tarball.is_file() {
                super::tarball::extract(&tarball, &dir)?;
            }
            buckle_core::engine::loader::load_dir(&dir, RevisionId::new(rev_id))
        })
        .await
        .map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "Loading a revision panicked.",
                &["Report this issue."],
            )
        })??;
        let revision = Arc::new(revision);
        self.insert(id, revision.clone());
        Ok(revision)
    }
}

/// Revision ids are hex commit ids or digests; refuse anything that could
/// escape the revisions directory.
fn sanitize(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn revisions_are_cached_under_their_sanitised_id() {
        let data = tempfile::tempdir().unwrap();
        let cache = RevisionCache::new(data.path());
        let dir = cache.dir("abc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("buckle.yaml"), "version: 2\n").unwrap();
        let plain = cache.load("abc").await.unwrap();
        let odd = cache.load("a/b.c").await.unwrap();
        assert!(Arc::ptr_eq(&plain, &odd), "one tree, one cache entry");
        assert_eq!(odd.id.0, "abc");
        assert!(
            cache.load("../..").await.is_err(),
            "nothing names the root itself"
        );
    }
}
