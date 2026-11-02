//! The path source: a local directory watched with `notify`, each change
//! becoming a revision whose id is the digest of the tree.

use super::{CommitInfo, RevisionBuilder, copy_tree, scratch_dir};
use crate::store::RevisionRecord;
use buckle_core::Result;
use chrono::Utc;
use notify::Watcher as _;
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Watches a configuration directory.
#[derive(Debug)]
pub struct PathWatcher {
    root: PathBuf,
    builder: RevisionBuilder,
}

impl PathWatcher {
    /// Watches `root`.
    pub fn new(root: PathBuf, builder: RevisionBuilder) -> Self {
        Self { root, builder }
    }

    /// Builds a revision from the directory if its content is new.
    pub async fn scan(&self) -> Result<Option<RevisionRecord>> {
        let root = self.root.clone();
        let digest = tokio::task::spawn_blocking(move || {
            super::tarball::build(&root).map(|b| buckle_core::Digest::of(&b))
        })
        .await
        .map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "Scanning the source directory panicked.",
                &["Report this issue."],
            )
        })??;
        let id = digest.to_string();
        if self.builder.exists(&id).await? {
            return Ok(None);
        }
        let scratch = scratch_dir(self.builder.cache(), &id);
        let (from, to) = (self.root.clone(), scratch.clone());
        tokio::task::spawn_blocking(move || copy_tree(&from, &to))
            .await
            .map_err(|e| {
                buckle_core::error::wrap_system(
                    e,
                    "Copying the source directory panicked.",
                    &["Report this issue."],
                )
            })??;
        let info = CommitInfo {
            id,
            author: "local".into(),
            message: format!("directory change in {}", self.root.display()),
            time: Utc::now(),
        };
        self.builder.build(info, scratch, "path").await.map(Some)
    }

    /// Scans on start and after every burst of changes until cancelled.
    pub async fn run(self, cancel: CancellationToken) {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if res.is_ok() {
                let _ = tx.send(());
            }
        });
        let mut watcher = match watcher {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::warn!(error = %e, "could not watch the source directory; falling back to polling");
                None
            }
        };
        if let Some(w) = watcher.as_mut()
            && let Err(e) = w.watch(&self.root, notify::RecursiveMode::Recursive)
        {
            tracing::warn!(error = %e, path = %self.root.display(), "could not watch the source directory; falling back to polling");
        }
        loop {
            if let Err(e) = self.scan().await {
                tracing::warn!(error = %e, "the source directory could not be turned into a revision");
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = rx.recv() => {
                    // Debounce: editors write several events per save.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    while rx.try_recv().is_ok() {}
                }
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
        drop(watcher);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::source::cache::RevisionCache;
    use crate::store::Store;
    use std::sync::Arc;

    #[tokio::test]
    async fn directory_revisions_are_identified_by_their_tree_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cfg");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("buckle.yaml"), "version: 2\n").unwrap();
        let store = Store::in_memory().await.unwrap();
        let cache = Arc::new(RevisionCache::new(&tmp.path().join("data")));
        let watcher = PathWatcher::new(
            root.clone(),
            RevisionBuilder::new(store, cache, EventBus::default()),
        );
        let first = watcher.scan().await.unwrap().expect("first scan builds");
        assert_eq!(first.id.len(), 64);
        assert!(watcher.scan().await.unwrap().is_none(), "unchanged tree");
        std::fs::write(root.join("buckle.yaml"), "version: 2\nsecrets: {}\n").unwrap();
        let second = watcher.scan().await.unwrap().expect("changed tree builds");
        assert_ne!(first.id, second.id);
    }
}
