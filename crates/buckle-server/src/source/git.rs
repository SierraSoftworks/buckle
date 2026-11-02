//! The Git source: a bare clone under `data_dir/source.git`, fetched every
//! poll and on webhook. Change detection compares the remote-tracking ref
//! for the configured branch with the last processed commit.

use super::{CommitInfo, RevisionBuilder, scratch_dir};
use crate::config::GitSource;
use crate::store::{RevisionRecord, Store, kv::keys};
use buckle_core::Result;
use chrono::{TimeZone, Utc};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Watches a Git branch.
pub struct GitWatcher {
    source: GitSource,
    repo: PathBuf,
    builder: RevisionBuilder,
    store: Store,
    trigger: Arc<Notify>,
    interrupt: Arc<AtomicBool>,
}

impl std::fmt::Debug for GitWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitWatcher")
            .field("branch", &self.source.branch)
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

fn git_error(e: impl std::fmt::Display, what: &str) -> buckle_core::Error {
    buckle_core::error::user(
        format!("{what}: {e}"),
        &[
            "Check source.git.url and credentials in server.yaml, and that the server can reach the repository.",
        ],
    )
}

impl GitWatcher {
    /// A watcher keeping its bare clone at `data_dir/source.git`.
    pub fn new(source: GitSource, data_dir: &Path, builder: RevisionBuilder, store: Store) -> Self {
        Self {
            source,
            repo: data_dir.join("source.git"),
            builder,
            store,
            trigger: Arc::new(Notify::new()),
            interrupt: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Notify this to fetch immediately (the webhook does).
    pub fn trigger(&self) -> Arc<Notify> {
        self.trigger.clone()
    }

    /// Fetches once and builds a revision when the branch moved.
    pub async fn poll_once(&self) -> Result<Option<RevisionRecord>> {
        let repo = self.repo.clone();
        let source = self.source.clone();
        let interrupt = self.interrupt.clone();
        let (before, after) =
            tokio::task::spawn_blocking(move || fetch(&repo, &source, &interrupt))
                .await
                .map_err(|e| {
                    buckle_core::error::wrap_system(
                        e,
                        "The Git fetch panicked.",
                        &["Report this issue."],
                    )
                })??;
        let Some(head) = after else {
            tracing::warn!(branch = %self.source.branch, "the configured branch does not exist on the remote");
            return Ok(None);
        };
        let last = self.store.call(|db| db.kv().get(keys::LAST_COMMIT)).await?;
        if before.as_deref() != Some(head.as_str()) {
            tracing::info!(from = ?before, to = %head, "the source branch moved");
        }
        if last.as_deref() == Some(head.as_str()) || self.builder.exists(&head).await? {
            self.set_last(&head).await?;
            return Ok(None);
        }
        let scratch = scratch_dir(self.builder.cache(), &head);
        let repo = self.repo.clone();
        let (h, s) = (head.clone(), scratch.clone());
        let info = tokio::task::spawn_blocking(move || export(&repo, &h, &s))
            .await
            .map_err(|e| {
                buckle_core::error::wrap_system(
                    e,
                    "Exporting a commit panicked.",
                    &["Report this issue."],
                )
            })??;
        let record = self.builder.build(info, scratch, "git").await?;
        self.set_last(&head).await?;
        Ok(Some(record))
    }

    async fn set_last(&self, commit: &str) -> Result<()> {
        let c = commit.to_owned();
        self.store
            .call(move |db| db.kv().set(keys::LAST_COMMIT, &c))
            .await
    }

    /// Polls until cancelled.
    pub async fn run(self, cancel: CancellationToken) {
        loop {
            if let Err(e) = self.poll_once().await {
                tracing::warn!(error = %e, "the source fetch failed; retrying at the next poll");
            }
            tokio::select! {
                _ = cancel.cancelled() => {
                    self.interrupt.store(true, Ordering::Relaxed);
                    break;
                }
                _ = tokio::time::sleep(self.source.poll) => {}
                _ = self.trigger.notified() => {}
            }
        }
    }
}

fn tracking_ref(branch: &str) -> String {
    format!("refs/remotes/origin/{branch}")
}

fn read_ref(repo: &gix::Repository, name: &str) -> Option<String> {
    repo.try_find_reference(name)
        .ok()
        .flatten()
        .and_then(|mut r| r.peel_to_id().ok().map(|id| id.to_string()))
}

/// Fetches the branch into the remote-tracking ref; returns the ref before and after.
fn fetch(
    repo_path: &Path,
    source: &GitSource,
    interrupt: &AtomicBool,
) -> Result<(Option<String>, Option<String>)> {
    let repo = if repo_path.join("HEAD").is_file() {
        gix::open(repo_path).map_err(|e| git_error(e, "The local clone could not be opened"))?
    } else {
        std::fs::create_dir_all(repo_path)
            .map_err(|e| git_error(e, "The clone directory could not be created"))?;
        gix::init_bare(repo_path)
            .map_err(|e| git_error(e, "The local clone could not be created"))?
    };
    let tracking = tracking_ref(&source.branch);
    let before = read_ref(&repo, &tracking);
    let refspec = format!("+refs/heads/{0}:{1}", source.branch, tracking);
    let remote = repo
        .remote_at(source.url.as_str())
        .map_err(|e| git_error(e, "The source URL is invalid"))?
        .with_refspecs([refspec.as_str()], gix::remote::Direction::Fetch)
        .map_err(|e| git_error(e, "The fetch refspec is invalid"))?;
    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(|e| git_error(e, "Could not connect to the source repository"))?;
    let connection = match &source.credentials {
        Some(creds) if creds.token.is_some() || creds.password.is_some() => {
            let (user, pass) = match (&creds.token, &creds.username, &creds.password) {
                (Some(token), user, _) => (
                    user.clone().unwrap_or_else(|| "x-access-token".into()),
                    token.clone(),
                ),
                (None, user, Some(pass)) => (user.clone().unwrap_or_default(), pass.clone()),
                _ => (String::new(), String::new()),
            };
            connection.with_credentials(move |action| match action {
                gix::credentials::helper::Action::Get(ctx) => {
                    Ok(Some(gix::credentials::protocol::Outcome {
                        identity: gix::sec::identity::Account {
                            username: user.clone(),
                            password: pass.clone(),
                            oauth_refresh_token: None,
                        },
                        next: ctx.into(),
                    }))
                }
                _ => Ok(None),
            })
        }
        _ => connection,
    };
    connection
        .prepare_fetch(gix::progress::Discard, Default::default())
        .map_err(|e| git_error(e, "The fetch could not be negotiated"))?
        .receive(gix::progress::Discard, interrupt)
        .map_err(|e| git_error(e, "The fetch failed"))?;
    let repo =
        gix::open(repo_path).map_err(|e| git_error(e, "The local clone could not be reopened"))?;
    let after = read_ref(&repo, &tracking);
    Ok((before, after))
}

/// Writes the commit's tree to `dest` and returns its metadata.
fn export(repo_path: &Path, commit: &str, dest: &Path) -> Result<CommitInfo> {
    let repo =
        gix::open(repo_path).map_err(|e| git_error(e, "The local clone could not be opened"))?;
    let id = gix::ObjectId::from_hex(commit.as_bytes())
        .map_err(|e| git_error(e, "Invalid commit id"))?;
    let commit_obj = repo
        .find_commit(id)
        .map_err(|e| git_error(e, "The commit could not be read"))?;
    let author = commit_obj
        .author()
        .map(|a| a.name.to_string())
        .unwrap_or_default();
    let message = commit_obj
        .message_raw()
        .map(|m| m.to_string())
        .unwrap_or_default()
        .trim()
        .to_owned();
    let seconds = commit_obj.time().map(|t| t.seconds).unwrap_or(0);
    let time = Utc
        .timestamp_opt(seconds, 0)
        .single()
        .unwrap_or_else(Utc::now);
    let tree = commit_obj
        .tree()
        .map_err(|e| git_error(e, "The commit's tree could not be read"))?;
    let mut recorder = gix::traverse::tree::Recorder::default();
    tree.traverse()
        .breadthfirst(&mut recorder)
        .map_err(|e| git_error(e, "The commit's tree could not be walked"))?;
    std::fs::create_dir_all(dest)
        .map_err(|e| git_error(e, "The export directory could not be created"))?;
    for entry in recorder.records {
        if !entry.mode.is_blob() {
            continue;
        }
        let rel = entry.filepath.to_string();
        if rel
            .split('/')
            .any(|seg| seg == ".." || seg == ".git" || seg.is_empty())
        {
            continue;
        }
        let object = repo
            .find_object(entry.oid)
            .map_err(|e| git_error(e, "A file in the commit could not be read"))?;
        let path = dest.join(&rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| git_error(e, "The export directory could not be created"))?;
        }
        std::fs::write(&path, &object.data)
            .map_err(|e| git_error(e, "A file could not be exported"))?;
        #[cfg(unix)]
        if entry.mode.is_executable() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        }
    }
    Ok(CommitInfo {
        id: commit.to_owned(),
        author,
        message,
        time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::EventBus;
    use crate::source::cache::RevisionCache;
    use buckle_api::RevisionStatus;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(work: &Path, files: &[(&str, &str)], message: &str) {
        for (p, c) in files {
            let path = work.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, c).unwrap();
        }
        git(work, &["add", "-A"]);
        git(work, &["commit", "-q", "-m", message]);
        git(work, &["push", "-q", "origin", "HEAD:main"]);
    }

    #[tokio::test]
    async fn new_commits_become_revisions_and_broken_ones_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let remote = tmp.path().join("remote.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        git(&work, &["init", "-q", "-b", "main"]);
        git(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        commit(
            &work,
            &[
                ("buckle.yaml", "version: 2\n"),
                ("modules/motd/module.yaml", "name: motd\n"),
                ("modules/motd/files/motd.j2", "hello {{ host.id }}\n"),
            ],
            "first revision",
        );

        let data = tmp.path().join("data");
        let store = Store::in_memory().await.unwrap();
        let cache = Arc::new(RevisionCache::new(&data));
        let builder = RevisionBuilder::new(store.clone(), cache.clone(), EventBus::default());
        let watcher = GitWatcher::new(
            GitSource {
                url: remote.to_str().unwrap().to_owned(),
                branch: "main".into(),
                poll: std::time::Duration::from_secs(30),
                credentials: None,
                webhook_secret: None,
            },
            &data,
            builder,
            store.clone(),
        );
        let first = watcher
            .poll_once()
            .await
            .unwrap()
            .expect("first commit detected");
        assert_eq!(first.status, RevisionStatus::Valid, "{:?}", first.errors);
        assert_eq!(first.author, "Ada");
        assert_eq!(first.message, "first revision");
        assert_eq!(first.module_count, 1);
        assert!(cache.tarball(&first.id).is_file());
        assert!(
            cache
                .dir(&first.id)
                .join("modules/motd/files/motd.j2")
                .is_file()
        );
        assert!(
            watcher.poll_once().await.unwrap().is_none(),
            "no change, no revision"
        );

        commit(
            &work,
            &[("modules/motd/module.yaml", "name: motd\ntasks: [[[")],
            "break it",
        );
        let broken = watcher
            .poll_once()
            .await
            .unwrap()
            .expect("second commit detected");
        assert_ne!(broken.id, first.id);
        assert_eq!(broken.status, RevisionStatus::Rejected);
        assert!(!broken.errors.is_empty());
        let loaded = cache.load(&first.id).await.unwrap();
        assert_eq!(loaded.modules.len(), 1);
    }
}
