//! The Git source: a bare clone under `data_dir/source.git`, fetched every
//! poll and on webhook. Change detection compares the remote-tracking ref
//! for the configured branch with the last processed commit.
//!
//! Every new commit on the branch becomes a revision, oldest first (up to
//! [`MAX_BACKFILL`] per fetch), so each one is validated and shown in the
//! UI; the rollout controller supersedes the older ones. A branch that moves
//! back to a commit already built (a force-push or revert of the ref) is
//! announced again, so the fleet converges on the branch tip.

use super::{CommitInfo, RevisionBuilder, scratch_dir};
use crate::config::GitSource;
use crate::store::{RevisionRecord, Store, kv::keys};
use buckle_core::Result;
use chrono::{TimeZone, Utc};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// The most new commits one fetch turns into revisions. A longer gap (or a
/// tip that does not descend from the last processed commit) builds the tip
/// only.
pub const MAX_BACKFILL: usize = 50;

/// How long an interrupted fetch gets to notice before the watcher moves on.
const INTERRUPT_GRACE: Duration = Duration::from_secs(10);

/// Watches a Git branch.
pub struct GitWatcher {
    source: GitSource,
    repo: PathBuf,
    builder: RevisionBuilder,
    store: Store,
    trigger: Arc<Notify>,
}

impl std::fmt::Debug for GitWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitWatcher")
            .field("branch", &self.source.branch)
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

static URL_USERINFO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b([a-z][a-z0-9+.-]*://)[^/\s@]+@").expect("static regex compiles")
});

/// Removes credentials from text that may quote a URL (`https://user:token@…`)
/// or the configured secrets themselves.
pub fn scrub(text: &str, secrets: &[&str]) -> String {
    let mut out = URL_USERINFO.replace_all(text, "${1}***@").into_owned();
    for s in secrets.iter().filter(|s| s.len() >= 4) {
        out = out.replace(s, "***");
    }
    out
}

fn git_error(e: impl std::fmt::Display, what: &str) -> buckle_core::Error {
    git_error_hiding(e, what, &[])
}

fn git_error_hiding(e: impl std::fmt::Display, what: &str, secrets: &[&str]) -> buckle_core::Error {
    buckle_core::error::user(
        scrub(&format!("{what}: {e}"), secrets),
        &[
            "Check source.git.url and credentials in server.yaml, and that the server can reach the repository.",
        ],
    )
}

fn join_error(e: tokio::task::JoinError, what: &str) -> buckle_core::Error {
    buckle_core::error::wrap_system(e, format!("{what} panicked."), &["Report this issue."])
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
        }
    }

    /// Notify this to fetch immediately (the webhook does).
    pub fn trigger(&self) -> Arc<Notify> {
        self.trigger.clone()
    }

    /// Fetches once and builds revisions when the branch moved. Returns the
    /// record of the branch tip when it was built (or announced again).
    pub async fn poll_once(&self) -> Result<Option<RevisionRecord>> {
        self.poll(&CancellationToken::new()).await
    }

    /// Fetches, interrupting the fetch when `cancel` trips or it outlives
    /// `source.git.fetch_timeout`.
    async fn fetch(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        let repo = self.repo.clone();
        let source = self.source.clone();
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = interrupt.clone();
        let mut task = tokio::task::spawn_blocking(move || fetch(&repo, &source, &flag));
        tokio::select! {
            r = &mut task => r.map_err(|e| join_error(e, "The Git fetch"))?.map(Some),
            _ = tokio::time::sleep(self.source.fetch_timeout) => {
                interrupt.store(true, Ordering::Relaxed);
                let _ = tokio::time::timeout(INTERRUPT_GRACE, &mut task).await;
                Err(buckle_core::error::user(
                    format!(
                        "The Git fetch did not finish within {}s and was interrupted.",
                        self.source.fetch_timeout.as_secs()
                    ),
                    &["Check that the server can reach the repository, or raise source.git.fetch_timeout."],
                ))
            }
            _ = cancel.cancelled() => {
                interrupt.store(true, Ordering::Relaxed);
                let _ = tokio::time::timeout(INTERRUPT_GRACE, &mut task).await;
                Ok(None)
            }
        }
    }

    async fn poll(&self, cancel: &CancellationToken) -> Result<Option<RevisionRecord>> {
        let Some((before, after)) = self.fetch(cancel).await? else {
            return Ok(None);
        };
        let Some(head) = after else {
            tracing::warn!(branch = %self.source.branch, "the configured branch does not exist on the remote");
            return Ok(None);
        };
        let last = self.store.call(|db| db.kv().get(keys::LAST_COMMIT)).await?;
        if before.as_deref() != Some(head.as_str()) {
            tracing::info!(from = ?before, to = %head, "the source branch moved");
        }
        if last.as_deref() == Some(head.as_str()) {
            return Ok(None);
        }
        if self.builder.exists(&head).await? {
            // The branch moved back to a commit already built: announce it
            // again so a rollout brings the fleet back to the branch tip.
            tracing::info!(revision = %head, "the branch points at an earlier revision again");
            let record = self.builder.reannounce(&head).await?;
            self.set_last(&head).await?;
            return Ok(record);
        }
        let repo = self.repo.clone();
        let (h, l) = (head.clone(), last.clone());
        let mut pending = tokio::task::spawn_blocking(move || new_commits(&repo, &h, l.as_deref()))
            .await
            .map_err(|e| join_error(e, "Walking the branch history"))??;
        // Oldest first; anything already built (an earlier partial pass) is skipped.
        pending.reverse();
        let mut tip = None;
        for commit in pending {
            if cancel.is_cancelled() {
                break;
            }
            if commit != head && self.builder.exists(&commit).await? {
                continue;
            }
            let record = self.build(&commit).await?;
            self.set_last(&commit).await?;
            if commit == head {
                tip = Some(record);
            }
        }
        Ok(tip)
    }

    async fn build(&self, commit: &str) -> Result<RevisionRecord> {
        let scratch = scratch_dir(self.builder.cache(), commit);
        let repo = self.repo.clone();
        let (c, s) = (commit.to_owned(), scratch.clone());
        let info = tokio::task::spawn_blocking(move || export(&repo, &c, &s))
            .await
            .map_err(|e| join_error(e, "Exporting a commit"))??;
        self.builder.build(info, scratch, "git").await
    }

    async fn set_last(&self, commit: &str) -> Result<()> {
        let c = commit.to_owned();
        self.store
            .call(move |db| db.kv().set(keys::LAST_COMMIT, &c))
            .await
    }

    /// Polls until cancelled. A fetch in progress is interrupted on cancel.
    pub async fn run(self, cancel: CancellationToken) {
        loop {
            if let Err(e) = self.poll(&cancel).await {
                tracing::warn!(error = %e, "the source fetch failed; retrying at the next poll");
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
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

/// The commits to build for a tip of `head`, newest first: the first-parent
/// chain back to (not including) `last`. When `last` is unknown or not on
/// that chain within [`MAX_BACKFILL`] commits, only `head`.
fn new_commits(repo_path: &Path, head: &str, last: Option<&str>) -> Result<Vec<String>> {
    let Some(last) = last else {
        return Ok(vec![head.to_owned()]);
    };
    let repo =
        gix::open(repo_path).map_err(|e| git_error(e, "The local clone could not be opened"))?;
    let mut chain = Vec::new();
    let mut current =
        gix::ObjectId::from_hex(head.as_bytes()).map_err(|e| git_error(e, "Invalid commit id"))?;
    while chain.len() < MAX_BACKFILL {
        let id = current.to_string();
        if id == last {
            return Ok(chain);
        }
        chain.push(id);
        let commit = repo
            .find_commit(current)
            .map_err(|e| git_error(e, "The commit could not be read"))?;
        match commit.parent_ids().next() {
            Some(parent) => current = parent.detach(),
            None => break,
        }
    }
    tracing::info!(head = %head, last = %last, "the branch tip does not descend from the last processed commit within reach; building the tip only");
    Ok(vec![head.to_owned()])
}

/// Fetches the branch into the remote-tracking ref; returns the ref before and after.
fn fetch(
    repo_path: &Path,
    source: &GitSource,
    interrupt: &AtomicBool,
) -> Result<(Option<String>, Option<String>)> {
    let secrets: Vec<&str> = source
        .credentials
        .iter()
        .flat_map(|c| [c.token.as_deref(), c.password.as_deref()])
        .flatten()
        .collect();
    let fail = |e: &dyn std::fmt::Display, what: &str| git_error_hiding(e, what, &secrets);
    let repo = if repo_path.join("HEAD").is_file() {
        gix::open(repo_path).map_err(|e| fail(&e, "The local clone could not be opened"))?
    } else {
        std::fs::create_dir_all(repo_path)
            .map_err(|e| fail(&e, "The clone directory could not be created"))?;
        gix::init_bare(repo_path).map_err(|e| fail(&e, "The local clone could not be created"))?
    };
    let tracking = tracking_ref(&source.branch);
    let before = read_ref(&repo, &tracking);
    let refspec = format!("+refs/heads/{0}:{1}", source.branch, tracking);
    let remote = repo
        .remote_at(source.url.as_str())
        .map_err(|e| fail(&e, "The source URL is invalid"))?
        .with_refspecs([refspec.as_str()], gix::remote::Direction::Fetch)
        .map_err(|e| fail(&e, "The fetch refspec is invalid"))?;
    if interrupt.load(Ordering::Relaxed) {
        return Ok((before.clone(), before));
    }
    let connection = remote
        .connect(gix::remote::Direction::Fetch)
        .map_err(|e| fail(&e, "Could not connect to the source repository"))?;
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
        .map_err(|e| fail(&e, "The fetch could not be negotiated"))?
        .receive(gix::progress::Discard, interrupt)
        .map_err(|e| fail(&e, "The fetch failed"))?;
    let repo =
        gix::open(repo_path).map_err(|e| fail(&e, "The local clone could not be reopened"))?;
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
                fetch_timeout: std::time::Duration::from_secs(60),
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

    struct Repo {
        _tmp: tempfile::TempDir,
        work: PathBuf,
        store: Store,
        watcher: GitWatcher,
        announced: Arc<std::sync::Mutex<Vec<String>>>,
    }

    async fn repo() -> Repo {
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
        let data = tmp.path().join("data");
        let store = Store::in_memory().await.unwrap();
        let cache = Arc::new(RevisionCache::new(&data));
        let announced = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = announced.clone();
        let builder = RevisionBuilder::new(store.clone(), cache, EventBus::default()).on_added(
            Arc::new(move |r: &RevisionRecord| seen.lock().unwrap().push(r.message.clone())),
        );
        let watcher = GitWatcher::new(
            GitSource {
                url: remote.to_str().unwrap().to_owned(),
                branch: "main".into(),
                poll: std::time::Duration::from_secs(30),
                credentials: None,
                webhook_secret: None,
                fetch_timeout: std::time::Duration::from_secs(60),
            },
            &data,
            builder,
            store.clone(),
        );
        Repo {
            _tmp: tmp,
            work,
            store,
            watcher,
            announced,
        }
    }

    fn head(work: &Path) -> String {
        let out = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(work)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    #[tokio::test]
    async fn every_new_commit_becomes_a_revision_in_order() {
        let r = repo().await;
        commit(&r.work, &[("buckle.yaml", "version: 2\n")], "one");
        r.watcher.poll_once().await.unwrap().expect("first");
        commit(&r.work, &[("modules/a/module.yaml", "name: a\n")], "two");
        commit(&r.work, &[("modules/b/module.yaml", "name: b\n")], "three");
        commit(&r.work, &[("modules/c/module.yaml", "name: c\n")], "four");
        let tip = r.watcher.poll_once().await.unwrap().expect("the tip");
        assert_eq!(tip.message, "four");
        assert_eq!(
            *r.announced.lock().unwrap(),
            vec!["one", "two", "three", "four"]
        );
        let count = r.store.call(|db| db.revisions().count()).await.unwrap();
        assert_eq!(count, 4);
    }

    #[tokio::test]
    async fn a_branch_moved_back_to_a_built_commit_is_announced_again() {
        let r = repo().await;
        commit(&r.work, &[("buckle.yaml", "version: 2\n")], "good");
        let good = head(&r.work);
        r.watcher.poll_once().await.unwrap().expect("good");
        commit(&r.work, &[("modules/a/module.yaml", "name: a\n")], "bad");
        r.watcher.poll_once().await.unwrap().expect("bad");
        git(&r.work, &["reset", "-q", "--hard", &good]);
        git(&r.work, &["push", "-q", "--force", "origin", "HEAD:main"]);
        let back = r
            .watcher
            .poll_once()
            .await
            .unwrap()
            .expect("announced again");
        assert_eq!(back.id, good);
        assert_eq!(*r.announced.lock().unwrap(), vec!["good", "bad", "good"]);
        assert!(r.watcher.poll_once().await.unwrap().is_none(), "only once");
    }

    #[tokio::test]
    async fn a_cancelled_fetch_builds_nothing() {
        let r = repo().await;
        commit(&r.work, &[("buckle.yaml", "version: 2\n")], "one");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(r.watcher.poll(&cancel).await.unwrap().is_none());
        assert!(r.announced.lock().unwrap().is_empty());
    }

    #[test]
    fn credentials_are_scrubbed_from_error_text() {
        let text = "could not fetch https://x-access-token:ghp_abc123@github.com/o/r.git and ssh://git@host/r (token ghp_abc123)";
        let clean = scrub(text, &["ghp_abc123"]);
        assert!(!clean.contains("ghp_abc123"), "{clean}");
        assert!(!clean.contains("x-access-token"), "{clean}");
        assert!(clean.contains("https://***@github.com/o/r.git"), "{clean}");
        assert!(clean.contains("ssh://***@host/r"), "{clean}");
    }
}
