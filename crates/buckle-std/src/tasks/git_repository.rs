//! `git.repository`: clone a git repository and keep its working tree at a ref.

use super::hostkit;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, FileKind, FileMeta, FilePerms};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::instrument;

/// Clones a git repository and keeps its working tree at a branch, tag or commit.
///
/// ```yaml
/// - git.repository:
///     repo: https://github.com/geerlingguy/rpi-clone.git
///     path: /opt/rpi-clone
///     ref: master           # branch, tag or full commit id; default: the remote's default branch
///     owner: pi             # run git as this user and own the tree
///     group: pi
///     depth: 1              # shallow clone/fetch
///     recursive: true       # also init and update submodules
///     update: true          # false: clone once, never move HEAD afterwards
///   tags: [network]
/// ```
///
/// The plan compares the checkout's `HEAD` with what `git ls-remote` reports
/// for `ref` on `repo`, so planning contacts the remote (a full 40 or 64
/// character commit id is compared without it). A branch is checked out as a
/// local branch tracking `origin/<ref>`; a tag or commit is checked out
/// detached. When `ref` names both a branch and a tag, the branch wins.
///
/// A working tree with uncommitted changes to tracked files is never reset:
/// a plan that would move `HEAD` is blocked instead. A non-empty `path` that
/// is not a git repository is blocked too. `owner` runs every git command as
/// that user (with `runuser`, so their SSH keys and git config apply) and
/// `owner`/`group` are applied recursively after each change.
///
/// Removal deletes the clone only if Buckle created it, it has no local
/// changes (tracked or untracked), and `HEAD` is still the commit Buckle last
/// checked out. Otherwise it is left in place.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The repository to clone: an https or ssh URL, or a path on the host.
    pub repo: String,
    /// Absolute path of the working tree (created if missing).
    pub path: PathBuf,
    /// A branch, tag or full commit id. Defaults to the remote's default branch.
    #[serde(default, rename = "ref")]
    pub reference: Option<String>,
    /// Run git as this user, and own the working tree.
    #[serde(default)]
    pub owner: Option<String>,
    /// Group to own the working tree.
    #[serde(default)]
    pub group: Option<String>,
    /// Fetch only this many commits of history.
    #[serde(default)]
    pub depth: Option<u32>,
    /// Initialise and update submodules, recursively.
    #[serde(default)]
    pub recursive: bool,
    /// Move `HEAD` when the remote ref changes. `false` clones once and then leaves it alone.
    #[serde(default = "yes")]
    pub update: bool,
}

fn yes() -> bool {
    true
}

pub struct GitRepository;
buckle_core::register_task!(GitRepository);

/// What `ref` resolved to on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Branch(String),
    Tag(String),
    Commit,
}

#[derive(Debug, Clone)]
struct Wanted {
    target: Target,
    sha: String,
}

impl Wanted {
    fn describe(&self) -> String {
        let short = short(&self.sha);
        match &self.target {
            Target::Branch(b) => format!("branch {b} ({short})"),
            Target::Tag(t) => format!("tag {t} ({short})"),
            Target::Commit => format!("commit {short}"),
        }
    }
}

/// The checkout as it is now.
#[derive(Debug, Default)]
struct Local {
    head: Option<String>,
    branch: Option<String>,
    origin: Option<String>,
    /// Tracked files with uncommitted changes.
    dirty: Vec<String>,
}

/// What is at `path`.
enum Found {
    /// Nothing, or an empty directory: clone.
    Clone,
    Repository,
    Blocked(String),
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

fn is_commit_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn validate(spec: &Spec) -> Result<()> {
    hostkit::require_absolute(&spec.path, "path")?;
    if spec.repo.trim().is_empty() || spec.repo.starts_with('-') || spec.repo.contains('\n') {
        return Err(error::user(
            format!("'{}' is not a usable git repository URL.", spec.repo),
            &["Use an https or ssh URL such as `https://github.com/org/repo.git`."],
        ));
    }
    if let Some(r) = &spec.reference
        && (r.trim().is_empty()
            || r.starts_with('-')
            || r.contains(|c: char| c.is_whitespace() || matches!(c, '~' | '^' | ':' | '\\'))
            || r.contains(".."))
    {
        return Err(error::user(
            format!("'{r}' is not a usable git ref."),
            &["Use a branch name, a tag name or a full commit id."],
        ));
    }
    if spec.depth == Some(0) {
        return Err(error::user(
            "`depth` must be at least 1.",
            &["Leave `depth` out for a full clone."],
        ));
    }
    for name in [&spec.owner, &spec.group].into_iter().flatten() {
        super::user_group::valid_name(name)?;
    }
    Ok(())
}

/// A git invocation, run as `as_user` when one is given.
fn git(as_user: Option<&str>, dir: Option<&Path>) -> Command {
    let cmd = match as_user {
        Some(user) => Command::new("runuser").args(["-u", user, "--", "git"]),
        None => Command::new("git"),
    };
    let cmd = cmd.env("GIT_TERMINAL_PROMPT", "0");
    match dir {
        Some(d) => cmd.arg("-C").arg(d.display().to_string()),
        None => cmd,
    }
}

impl Spec {
    fn git(&self) -> Command {
        git(self.owner.as_deref(), Some(&self.path))
    }

    fn perms(&self) -> FilePerms {
        FilePerms {
            mode: None,
            owner: self.owner.clone(),
            group: self.group.clone(),
        }
    }

    fn depth_args(&self) -> Vec<String> {
        self.depth
            .map(|d| vec!["--depth".into(), d.to_string()])
            .unwrap_or_default()
    }
}

/// The repository URL with any secrets it carries scrubbed, for summaries.
fn shown(cx: &Cx, repo: &str) -> String {
    hostkit::scrub(cx, repo)
}

/// The blocked plan (or `None`) when git or runuser is missing.
async fn missing_tools(cx: &Cx, spec: &Spec) -> Result<Option<Planned>> {
    if let Some(blocked) = hostkit::missing_tool(cx, "git", "install git").await? {
        return Ok(Some(blocked));
    }
    if spec.owner.is_some() {
        return hostkit::missing_tool(cx, "runuser", "it is needed to run git as `owner`").await;
    }
    Ok(None)
}

async fn find(cx: &Cx, path: &Path) -> Result<Found> {
    let Some(meta) = cx.system.stat(path).await? else {
        return Ok(Found::Clone);
    };
    if meta.kind != FileKind::Directory {
        return Ok(Found::Blocked(format!(
            "{} exists and is not a directory",
            path.display()
        )));
    }
    if cx.system.stat(&path.join(".git")).await?.is_some() {
        return Ok(Found::Repository);
    }
    Ok(match cx.system.list_dir(path).await? {
        Some(entries) if !entries.is_empty() => Found::Blocked(format!(
            "{} is not empty and is not a git repository; Buckle will not clone over it",
            path.display()
        )),
        _ => Found::Clone,
    })
}

async fn stdout_if_ok(cx: &Cx, command: Command) -> Result<Option<String>> {
    let out = cx.system.run(command).await?;
    Ok(out
        .success()
        .then(|| out.stdout_str().trim().to_owned())
        .filter(|s| !s.is_empty()))
}

async fn local(cx: &Cx, spec: &Spec) -> Result<Local> {
    let head = stdout_if_ok(
        cx,
        spec.git()
            .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
    )
    .await?;
    let branch = stdout_if_ok(
        cx,
        spec.git()
            .args(["symbolic-ref", "--quiet", "--short", "HEAD"]),
    )
    .await?;
    let origin = stdout_if_ok(cx, spec.git().args(["remote", "get-url", "origin"])).await?;
    let status = hostkit::run_checked(
        cx,
        spec.git()
            .args(["status", "--porcelain", "--untracked-files=no"]),
        "Checking the working tree for local changes",
    )
    .await?;
    let dirty = status
        .stdout_str()
        .lines()
        .filter_map(|l| l.get(3..).map(str::to_owned))
        .collect();
    Ok(Local {
        head,
        branch,
        origin,
        dirty,
    })
}

/// Resolves `ref` against the remote with `git ls-remote`.
async fn resolve(cx: &Cx, spec: &Spec) -> Result<Wanted> {
    if let Some(r) = &spec.reference
        && is_commit_id(r)
    {
        return Ok(Wanted {
            target: Target::Commit,
            sha: r.to_ascii_lowercase(),
        });
    }
    let patterns = match &spec.reference {
        None => vec!["HEAD".to_owned()],
        Some(r) => vec![
            format!("refs/heads/{r}"),
            format!("refs/tags/{r}"),
            format!("refs/tags/{r}^{{}}"),
        ],
    };
    let out = hostkit::run_checked(
        cx,
        git(spec.owner.as_deref(), None)
            .args(["ls-remote", "--symref"])
            .arg(&spec.repo)
            .args(patterns),
        "Listing the remote's refs",
    )
    .await?;
    let mut refs = std::collections::BTreeMap::new();
    let mut head_branch = None;
    for line in out.stdout_str().lines() {
        let Some((left, name)) = line.split_once('\t') else {
            continue;
        };
        match left.strip_prefix("ref: ") {
            Some(target) if name == "HEAD" => {
                head_branch = target.strip_prefix("refs/heads/").map(str::to_owned);
            }
            Some(_) => {}
            None => {
                refs.insert(name.to_owned(), left.trim().to_owned());
            }
        }
    }
    let found = match &spec.reference {
        None => refs.get("HEAD").map(|sha| Wanted {
            target: head_branch.map_or(Target::Commit, Target::Branch),
            sha: sha.clone(),
        }),
        Some(r) => refs
            .get(&format!("refs/heads/{r}"))
            .map(|sha| (Target::Branch(r.clone()), sha))
            .or_else(|| {
                refs.get(&format!("refs/tags/{r}^{{}}"))
                    .or_else(|| refs.get(&format!("refs/tags/{r}")))
                    .map(|sha| (Target::Tag(r.clone()), sha))
            })
            .map(|(target, sha)| Wanted {
                target,
                sha: sha.clone(),
            }),
    };
    found.ok_or_else(|| {
        error::user(
            format!(
                "{} has no {}.",
                shown(cx, &spec.repo),
                match &spec.reference {
                    Some(r) => format!("branch or tag named '{r}'"),
                    None => "default branch (HEAD)".to_owned(),
                }
            ),
            &[
                "Check the spelling of `ref`, or give a full 40-character commit id.",
                "Abbreviated commit ids cannot be fetched; use the full id.",
            ],
        )
    })
}

fn at_wanted(local: &Local, wanted: &Wanted) -> bool {
    local.head.as_deref() == Some(wanted.sha.as_str())
        && match &wanted.target {
            Target::Branch(b) => local.branch.as_deref() == Some(b.as_str()),
            Target::Tag(_) | Target::Commit => true,
        }
}

/// Submodules that are missing or at another commit than the superproject records.
async fn stale_submodules(cx: &Cx, spec: &Spec) -> Result<usize> {
    if !spec.recursive {
        return Ok(0);
    }
    let out = hostkit::run_checked(
        cx,
        spec.git().args(["submodule", "status", "--recursive"]),
        "Checking submodules",
    )
    .await?;
    Ok(out
        .stdout_str()
        .lines()
        .filter(|l| l.starts_with(['-', '+', 'U']))
        .count())
}

async fn ownership_changes(cx: &Cx, spec: &Spec) -> Result<Vec<String>> {
    Ok(match cx.system.stat(&spec.path).await? {
        Some(meta) => hostkit::perm_changes(&meta, &spec.perms()),
        None => vec![],
    })
}

fn dirty_reason(spec: &Spec, local: &Local) -> String {
    let mut files = local.dirty.iter().take(3).cloned().collect::<Vec<_>>();
    if local.dirty.len() > 3 {
        files.push(format!("and {} more", local.dirty.len() - 3));
    }
    format!(
        "{} has uncommitted changes ({}); commit, stash or discard them before Buckle moves HEAD",
        spec.path.display(),
        files.join(", ")
    )
}

#[async_trait]
impl Task for GitRepository {
    type Spec = Spec;
    const KIND: &'static str = "git.repository";
    const PLATFORMS: Platforms = Platforms::UNIX;
    const DESCRIPTION: &'static str = "Clone a git repository and keep it at a branch, tag or commit, never resetting local changes.";

    fn key(spec: &Spec) -> String {
        spec.path.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        format!(
            "git {}{} at {}",
            spec.repo,
            spec.reference
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default(),
            spec.path.display()
        )
    }

    #[instrument(name = "task.git.repository.plan", skip_all, fields(path = %spec.path.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        validate(spec)?;
        if let Some(blocked) = missing_tools(cx, spec).await? {
            return Ok(blocked);
        }
        match find(cx, &spec.path).await? {
            Found::Blocked(reason) => return Ok(Planned::blocked(reason)),
            Found::Clone => {
                let wanted = resolve(cx, spec).await?;
                return Ok(Planned::change(Diff::Create {
                    summary: format!(
                        "clone {} at {} into {}",
                        shown(cx, &spec.repo),
                        wanted.describe(),
                        spec.path.display()
                    ),
                    preview: None,
                }));
            }
            Found::Repository => {}
        }
        let local = local(cx, spec).await?;
        let mut changes = Vec::new();
        let mut moves_head = false;
        if spec.update {
            if local.origin.as_deref() != Some(spec.repo.as_str()) {
                changes.push(format!("set origin to {}", shown(cx, &spec.repo)));
            }
            let wanted = resolve(cx, spec).await?;
            if !at_wanted(&local, &wanted) {
                moves_head = true;
                changes.push(format!(
                    "move HEAD from {} to {}",
                    local.head.as_deref().map(short).unwrap_or("nothing"),
                    wanted.describe()
                ));
            }
            match stale_submodules(cx, spec).await? {
                0 => {}
                n => changes.push(format!("update {n} submodule(s)")),
            }
        }
        if moves_head && !local.dirty.is_empty() {
            return Ok(Planned::blocked(dirty_reason(spec, &local)));
        }
        let perms = ownership_changes(cx, spec).await?;
        if !perms.is_empty() {
            changes.push(perms.join(", "));
        }
        Ok(if changes.is_empty() {
            Planned::NoOp
        } else {
            Planned::change(Diff::Update {
                summary: format!("{}: {}", spec.path.display(), changes.join("; ")),
                unified: None,
            })
        })
    }

    #[instrument(name = "task.git.repository.apply", skip_all, fields(path = %spec.path.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        validate(spec)?;
        hostkit::require_tool(cx, "git", "install git").await?;
        if spec.owner.is_some() {
            hostkit::require_tool(cx, "runuser", "it is needed to run git as `owner`").await?;
        }
        let mut did = Vec::new();
        let created = match find(cx, &spec.path).await? {
            Found::Blocked(reason) => {
                return Err(error::user(
                    format!("Cannot clone into {}: {reason}.", spec.path.display()),
                    &["Remove or move what is there, or choose another `path`."],
                ));
            }
            Found::Clone => {
                cx.system.mkdir_all(&spec.path, &spec.perms()).await?;
                hostkit::run_checked(
                    cx,
                    spec.git().args(["init", "--quiet"]),
                    "Creating the repository",
                )
                .await?;
                hostkit::run_checked(
                    cx,
                    spec.git().args(["remote", "add", "origin"]).arg(&spec.repo),
                    "Adding the origin remote",
                )
                .await?;
                did.push(format!("cloned {}", shown(cx, &spec.repo)));
                true
            }
            Found::Repository => false,
        };

        let local = local(cx, spec).await?;
        let mut moved = false;
        if spec.update || created {
            if !created && local.origin.as_deref() != Some(spec.repo.as_str()) {
                let verb = if local.origin.is_some() {
                    "set-url"
                } else {
                    "add"
                };
                hostkit::run_checked(
                    cx,
                    spec.git().args(["remote", verb, "origin"]).arg(&spec.repo),
                    "Setting the origin remote",
                )
                .await?;
                did.push(format!("set origin to {}", shown(cx, &spec.repo)));
            }
            let wanted = resolve(cx, spec).await?;
            if !at_wanted(&local, &wanted) {
                if !local.dirty.is_empty() {
                    return Err(error::user(
                        dirty_reason(spec, &local),
                        &[
                            "Buckle never resets local changes; resolve them on the host and apply again.",
                        ],
                    ));
                }
                checkout(cx, spec, &local, &wanted).await?;
                moved = true;
                did.push(format!("checked out {}", wanted.describe()));
            }
            if spec.recursive && (moved || stale_submodules(cx, spec).await? > 0) {
                hostkit::run_checked(
                    cx,
                    spec.git()
                        .args(["submodule", "update", "--init", "--recursive"])
                        .args(spec.depth_args()),
                    "Updating submodules",
                )
                .await?;
                if !moved {
                    did.push("updated submodules".to_owned());
                }
            }
        }

        let perms = ownership_changes(cx, spec).await?;
        if (spec.owner.is_some() || spec.group.is_some()) && (created || moved || !perms.is_empty())
        {
            let who = match (&spec.owner, &spec.group) {
                (Some(o), Some(g)) => format!("{o}:{g}"),
                (Some(o), None) => o.clone(),
                (None, Some(g)) => format!(":{g}"),
                (None, None) => unreachable!(),
            };
            hostkit::run_checked(
                cx,
                Command::new("chown")
                    .args(["-R", &who])
                    .arg(spec.path.display().to_string()),
                "Setting the working tree's ownership",
            )
            .await?;
            if !perms.is_empty() && !created {
                did.push(perms.join(", "));
            }
        }

        let head = stdout_if_ok(
            cx,
            spec.git()
                .args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
        )
        .await?
        .unwrap_or_default();
        let mut applied = if did.is_empty() {
            Applied::unchanged(format!("{} is at {}", spec.path.display(), short(&head)))
        } else {
            Applied::changed(format!("{}: {}", spec.path.display(), did.join(", ")))
        };
        applied.content_digest = Some(Digest::of(head.as_bytes()));
        if created {
            applied = applied.with_artefact(Artefact::Directory {
                path: spec.path.clone(),
            });
        }
        Ok(hostkit::ownership::carry_forward_if(cx, applied, |a| {
            matches!(a, Artefact::Directory { .. })
        }))
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!(
            "delete the clone at {} if Buckle created it and it has no local changes",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some(path) = record.artefacts.iter().find_map(|a| match a {
            Artefact::Directory { path } => Some(path.clone()),
            _ => None,
        }) else {
            return Ok(Applied::skipped(format!(
                "{} existed before Buckle managed it, left in place",
                record.key.key
            )));
        };
        let Some(meta) = cx.system.stat(&path).await? else {
            return Ok(Applied::unchanged(format!(
                "{} was already absent",
                path.display()
            )));
        };
        let Some(as_user) = removal_user(cx, &meta).await? else {
            return Ok(Applied::skipped(format!(
                "git (or runuser) is not installed, so {} cannot be checked for local changes; left in place",
                path.display()
            )));
        };
        let git = || git(as_user.as_deref(), Some(&path));
        let status = cx
            .system
            .run(git().args(["status", "--porcelain", "--untracked-files=normal"]))
            .await?;
        if !status.success() {
            return Ok(Applied::skipped(format!(
                "{} is no longer a readable git repository, left in place",
                path.display()
            )));
        }
        if !status.stdout_str().trim().is_empty() {
            return Ok(Applied::skipped(format!(
                "{} has local changes, left in place",
                path.display()
            )));
        }
        let head = stdout_if_ok(
            cx,
            git().args(["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
        )
        .await?
        .unwrap_or_default();
        if record.content_digest.as_ref() != Some(&Digest::of(head.as_bytes())) {
            return Ok(Applied::skipped(format!(
                "HEAD of {} moved since Buckle checked it out, left in place",
                path.display()
            )));
        }
        cx.system.remove(&path, true).await?;
        Ok(Applied::changed(format!("deleted {}", path.display())))
    }
}

/// Who to run git as when checking a clone before removal: its owner (unless
/// root), so that git never runs as root inside another user's repository.
/// `None` when the tools for that are missing.
async fn removal_user(cx: &Cx, meta: &FileMeta) -> Result<Option<Option<String>>> {
    if cx.system.which("git").await?.is_none() {
        return Ok(None);
    }
    let owner = meta.owner.as_deref().filter(|o| *o != "root");
    if owner.is_some() && cx.system.which("runuser").await?.is_none() {
        return Ok(None);
    }
    Ok(Some(owner.map(str::to_owned)))
}

async fn checkout(cx: &Cx, spec: &Spec, local: &Local, wanted: &Wanted) -> Result<()> {
    let fetch = |refspec: String| {
        spec.git()
            .args(["fetch", "--quiet", "--no-tags"])
            .args(spec.depth_args())
            .args(["origin".to_owned(), refspec])
    };
    let checkout = match &wanted.target {
        Target::Branch(b) => {
            // Checked against the tracking ref as last fetched, before fetching:
            // after a shallow fetch the old HEAD is no longer connected to the new tip.
            if local.branch.as_deref() == Some(b.as_str()) && local.head.is_some() {
                let ff = cx
                    .system
                    .run(
                        spec.git()
                            .args(["merge-base", "--is-ancestor", "HEAD"])
                            .arg(format!("refs/remotes/origin/{b}")),
                    )
                    .await?;
                if !ff.success() {
                    return Err(error::user(
                        format!(
                            "{} has commits on {b} that origin/{b} did not have; Buckle only fast-forwards.",
                            spec.path.display()
                        ),
                        &["Push or drop the local commits on the host, then apply again."],
                    ));
                }
            }
            hostkit::run_checked(
                cx,
                fetch(format!("+refs/heads/{b}:refs/remotes/origin/{b}")),
                "Fetching the branch",
            )
            .await?;
            spec.git()
                .args(["checkout", "--quiet", "--track", "-B"])
                .arg(b)
                .arg(format!("origin/{b}"))
        }
        Target::Tag(t) => {
            hostkit::run_checked(
                cx,
                fetch(format!("+refs/tags/{t}:refs/tags/{t}")),
                "Fetching the tag",
            )
            .await?;
            spec.git()
                .args(["checkout", "--quiet", "--detach"])
                .arg(format!("refs/tags/{t}"))
        }
        Target::Commit => {
            let present = cx
                .system
                .run(
                    spec.git()
                        .args(["cat-file", "-e"])
                        .arg(format!("{}^{{commit}}", wanted.sha)),
                )
                .await?
                .success();
            if !present {
                hostkit::run_checked(cx, fetch(wanted.sha.clone()), "Fetching the commit").await?;
            }
            spec.git()
                .args(["checkout", "--quiet", "--detach"])
                .arg(&wanted.sha)
        }
    };
    hostkit::run_checked(cx, checkout, "Checking out the ref").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::Output;
    use buckle_core::testing::{CxBuilder, FakeSystem, ok_output};
    use rstest::rstest;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    const URL: &str = "https://git.example/dotfiles.git";
    const PATH: &str = "/srv/dotfiles";

    fn sha(n: u8) -> String {
        format!("{n:02x}").repeat(20)
    }

    #[derive(Default)]
    struct Clone {
        head: Option<String>,
        branch: Option<String>,
        origin: Option<String>,
        dirty: Vec<String>,
        untracked: bool,
        objects: BTreeSet<String>,
        tracking: BTreeMap<String, String>,
    }

    /// A remote with a linear `main` history and a `v1` tag, and the clones made from it.
    #[derive(Default)]
    struct World {
        refs: BTreeMap<String, String>,
        history: Vec<String>,
        clones: BTreeMap<String, Clone>,
        stale_submodules: usize,
        users: Vec<String>,
    }

    fn fail() -> Output {
        Output {
            code: Some(1),
            stdout: vec![],
            stderr: b"fatal".to_vec(),
            timed_out: false,
        }
    }

    fn ok() -> Output {
        ok_output("")
    }

    fn run_git(w: &mut World, sys: &FakeSystem, args: &[String]) -> Output {
        let (dir, args) = match args {
            [c, d, rest @ ..] if c == "-C" => (d.clone(), rest),
            rest => (String::new(), rest),
        };
        let a: Vec<&str> = args.iter().map(String::as_str).collect();
        if a[0] == "ls-remote" {
            let mut out = String::new();
            for pattern in &a[3..] {
                if *pattern == "HEAD" {
                    out += "ref: refs/heads/main\tHEAD\n";
                    out += &format!("{}\tHEAD\n", w.refs["refs/heads/main"]);
                } else if let Some(sha) = w.refs.get(*pattern) {
                    out += &format!("{sha}\t{pattern}\n");
                }
            }
            return ok_output(&out);
        }
        if a[0] == "init" {
            let _ = sys.clone().with_file(
                format!("{dir}/.git/HEAD"),
                "ref: refs/heads/master\n",
                FilePerms::default(),
            );
            w.clones.insert(dir, Clone::default());
            return ok();
        }
        let tip = |w: &World, r: &str| w.refs.get(&format!("{r}^{{}}")).or(w.refs.get(r)).cloned();
        let Some(c) = w.clones.get_mut(&dir) else {
            return fail();
        };
        match a.as_slice() {
            ["remote", "add" | "set-url", "origin", url] => c.origin = Some((*url).into()),
            ["remote", "get-url", "origin"] => {
                return c.origin.as_deref().map_or_else(fail, ok_output);
            }
            ["rev-parse", ..] => return c.head.as_deref().map_or_else(fail, ok_output),
            ["symbolic-ref", ..] => return c.branch.as_deref().map_or_else(fail, ok_output),
            ["status", "--porcelain", untracked] => {
                let mut out: String = c.dirty.iter().map(|f| format!(" M {f}\n")).collect();
                if c.untracked && *untracked == "--untracked-files=normal" {
                    out += "?? notes.txt\n";
                }
                return ok_output(&out);
            }
            ["fetch", .., "origin", refspec] => {
                let (src, dst) = refspec
                    .trim_start_matches('+')
                    .split_once(':')
                    .unwrap_or((refspec, ""));
                let Some(target) = tip(w, src)
                    .or_else(|| w.history.contains(&src.to_owned()).then(|| src.to_owned()))
                else {
                    return fail();
                };
                let c = w.clones.get_mut(&dir).unwrap();
                c.objects.insert(target.clone());
                if !dst.is_empty() {
                    c.tracking.insert(dst.into(), target);
                }
            }
            ["cat-file", "-e", obj] => {
                return if c.objects.contains(obj.trim_end_matches("^{commit}")) {
                    ok()
                } else {
                    fail()
                };
            }
            ["merge-base", "--is-ancestor", "HEAD", upstream] => {
                let Some(tracked) = c.tracking.get(*upstream).cloned() else {
                    return fail();
                };
                let pos = |s: &str| w.history.iter().position(|h| h == s);
                let head = c.head.clone().unwrap();
                return match (pos(&head), pos(&tracked)) {
                    _ if head == tracked => ok(),
                    (Some(a), Some(b)) if a <= b => ok(),
                    _ => fail(),
                };
            }
            ["checkout", "--quiet", "--track", "-B", branch, upstream] => {
                c.head = Some(c.tracking[&format!("refs/remotes/{upstream}")].clone());
                c.branch = Some((*branch).into());
            }
            ["checkout", "--quiet", "--detach", target] => {
                let target = c
                    .tracking
                    .get(*target)
                    .cloned()
                    .unwrap_or_else(|| (*target).into());
                c.head = Some(target);
                c.branch = None;
            }
            ["submodule", "status", ..] => {
                return ok_output(&"-0000 lib/sub\n".repeat(w.stale_submodules));
            }
            ["submodule", "update", ..] => w.stale_submodules = 0,
            other => panic!("unexpected git command {other:?}"),
        }
        ok()
    }

    /// A host with git (and runuser) whose remote's `main` is at sha(2)
    /// (history 1, 2) and whose tag `v1` peels to sha(1).
    fn host() -> (FakeSystem, Arc<Mutex<World>>) {
        let (sys, world) = git_only();
        let (w, h) = (world.clone(), sys.clone());
        let sys = sys.on_command("runuser", &[], move |cmd| {
            let mut w = w.lock().unwrap();
            w.users.push(cmd.args[1].clone());
            run_git(&mut w, &h, &cmd.args[4..])
        });
        (sys, world)
    }

    /// [`host`] without runuser.
    fn git_only() -> (FakeSystem, Arc<Mutex<World>>) {
        let world = Arc::new(Mutex::new(World {
            refs: BTreeMap::from([
                ("refs/heads/main".into(), sha(2)),
                ("refs/tags/v1".into(), sha(9)),
                ("refs/tags/v1^{}".into(), sha(1)),
            ]),
            history: vec![sha(1), sha(2)],
            ..World::default()
        }));
        let sys = FakeSystem::new().on_command_ok("chown", &[], "");
        let (w, h) = (world.clone(), sys.clone());
        let sys = sys.on_command("git", &[], move |cmd| {
            run_git(&mut w.lock().unwrap(), &h, &cmd.args)
        });
        (sys, world)
    }

    /// A host with an existing clone of `main` at `head`.
    fn cloned(head: u8, tweak: impl FnOnce(&mut Clone)) -> (FakeSystem, Arc<Mutex<World>>) {
        let (sys, world) = host();
        let sys = sys.with_file(
            format!("{PATH}/.git/HEAD"),
            "ref: refs/heads/main\n",
            FilePerms::default(),
        );
        let mut c = Clone {
            head: Some(sha(head)),
            branch: Some("main".into()),
            origin: Some(URL.into()),
            ..Clone::default()
        };
        c.objects.insert(sha(head));
        c.tracking
            .insert("refs/remotes/origin/main".into(), sha(head));
        tweak(&mut c);
        world.lock().unwrap().clones.insert(PATH.into(), c);
        (sys, world)
    }

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"repo": URL, "path": PATH});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    fn head(world: &Arc<Mutex<World>>) -> (Option<String>, Option<String>) {
        let w = world.lock().unwrap();
        let c = &w.clones[PATH];
        (c.head.clone(), c.branch.clone())
    }

    fn ls_remotes(sys: &FakeSystem) -> usize {
        sys.command_lines()
            .iter()
            .filter(|l| l.contains("ls-remote"))
            .count()
    }

    #[rstest]
    #[case::missing(host().0, json!({}), Class::Change)]
    #[case::empty_dir(host().0.with_dir(PATH, FilePerms::default()), json!({}), Class::Change)]
    #[case::current(cloned(2, |_| {}).0, json!({}), Class::NoOp)]
    #[case::behind(cloned(1, |_| {}).0, json!({}), Class::Change)]
    #[case::behind_but_pinned(cloned(1, |_| {}).0, json!({"update": false}), Class::NoOp)]
    #[case::dirty_and_current(cloned(2, |c| c.dirty = vec!["a".into()]).0, json!({}), Class::NoOp)]
    #[case::dirty_and_behind(cloned(1, |c| c.dirty = vec!["a".into()]).0, json!({}), Class::Blocked)]
    #[case::other_origin(cloned(2, |c| c.origin = Some("https://old.example/x.git".into())).0, json!({}), Class::Change)]
    #[case::detached_on_branch_commit(cloned(2, |c| c.branch = None).0, json!({}), Class::Change)]
    #[case::tag_current(cloned(1, |c| c.branch = None).0, json!({"ref": "v1"}), Class::NoOp)]
    #[case::commit_current(cloned(1, |_| {}).0, json!({"ref": sha(1)}), Class::NoOp)]
    #[case::stale_submodules(cloned(2, |_| {}).0.on_command_ok("git", &["-C", PATH, "submodule", "status"], "-abc lib\n"), json!({"recursive": true}), Class::Change)]
    #[case::wrong_owner(cloned(2, |_| {}).0, json!({"owner": "pi"}), Class::Change)]
    #[case::not_a_repo(host().0.with_file(format!("{PATH}/x"), "x", FilePerms::default()), json!({}), Class::Blocked)]
    #[case::a_file(host().0.with_file(PATH, "x", FilePerms::default()), json!({}), Class::Blocked)]
    #[case::no_git(FakeSystem::new(), json!({}), Class::Blocked)]
    #[case::owner_without_runuser(git_only().0, json!({"owner": "pi"}), Class::Blocked)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        assert_eq!(
            plan_class(&GitRepository, &cx(&sys), &s(extra)).await,
            expected
        );
    }

    #[tokio::test]
    async fn a_full_commit_id_plans_without_contacting_the_remote() {
        let (sys, _) = cloned(1, |_| {});
        GitRepository
            .plan(&cx(&sys), &s(json!({"ref": sha(1)})))
            .await
            .unwrap();
        assert_eq!(ls_remotes(&sys), 0);
    }

    #[tokio::test]
    async fn clones_the_default_branch_then_fast_forwards() {
        let (sys, world) = host();
        let c = cx(&sys);
        let spec = s(json!({}));
        let applied = apply(&GitRepository, &c, &spec).await;
        assert!(applied.changed);
        assert!(
            applied
                .artefacts
                .contains(&Artefact::Directory { path: PATH.into() })
        );
        assert_eq!(head(&world), (Some(sha(2)), Some("main".into())));
        assert_eq!(plan_class(&GitRepository, &c, &spec).await, Class::NoOp);

        {
            let mut w = world.lock().unwrap();
            w.history.push(sha(3));
            w.refs.insert("refs/heads/main".into(), sha(3));
        }
        let again = CxBuilder::new(sys.clone())
            .previous(record("git.repository", PATH, &applied))
            .build();
        let moved = apply(&GitRepository, &again, &spec).await;
        assert!(moved.changed, "{}", moved.summary);
        assert_eq!(head(&world).0, Some(sha(3)));
        assert!(
            moved
                .artefacts
                .contains(&Artefact::Directory { path: PATH.into() }),
            "a clone Buckle created stays Buckle's"
        );
    }

    #[rstest]
    #[case::tag(json!({"ref": "v1"}), sha(1), None)]
    #[case::commit(json!({"ref": sha(1)}), sha(1), None)]
    #[case::branch(json!({"ref": "main"}), sha(2), Some("main"))]
    #[tokio::test]
    async fn checks_out_the_resolved_ref(
        #[case] extra: serde_json::Value,
        #[case] expected: String,
        #[case] branch: Option<&str>,
    ) {
        let (sys, world) = host();
        apply(&GitRepository, &cx(&sys), &s(extra)).await;
        assert_eq!(head(&world), (Some(expected), branch.map(str::to_owned)));
    }

    #[tokio::test]
    async fn an_unknown_ref_is_a_user_error() {
        let (sys, _) = host();
        let err = GitRepository
            .plan(&cx(&sys), &s(json!({"ref": "nope"})))
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
        assert!(err.description().contains("nope"));
    }

    #[tokio::test]
    async fn an_owner_runs_git_as_that_user_and_owns_the_tree() {
        let (sys, world) = host();
        apply(
            &GitRepository,
            &cx(&sys),
            &s(json!({"owner": "pi", "group": "users"})),
        )
        .await;
        let lines = sys.command_lines();
        assert!(
            lines
                .iter()
                .filter(|l| l.contains(" git "))
                .all(|l| l.starts_with("runuser -u pi -- git")),
            "{lines:?}"
        );
        assert!(world.lock().unwrap().users.iter().all(|u| u == "pi"));
        assert!(
            lines
                .iter()
                .any(|l| l == &format!("chown -R pi:users {PATH}")),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn local_commits_are_never_discarded() {
        let (sys, world) = cloned(7, |c| {
            c.tracking.insert("refs/remotes/origin/main".into(), sha(1));
        });
        let err = GitRepository
            .apply(&cx(&sys), &s(json!({})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(
            err.description().contains("fast-forward"),
            "{}",
            err.description()
        );
        assert_eq!(head(&world).0, Some(sha(7)));
    }

    #[tokio::test]
    async fn a_dirty_tree_is_not_moved_on_apply() {
        let (sys, world) = cloned(1, |c| c.dirty = vec!["a".into()]);
        let err = GitRepository
            .apply(&cx(&sys), &s(json!({})), &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("uncommitted"));
        assert_eq!(head(&world).0, Some(sha(1)));
    }

    #[tokio::test]
    async fn recursive_updates_submodules_after_checkout() {
        let (sys, world) = host();
        world.lock().unwrap().stale_submodules = 1;
        apply(
            &GitRepository,
            &cx(&sys),
            &s(json!({"recursive": true, "depth": 1})),
        )
        .await;
        assert_eq!(world.lock().unwrap().stale_submodules, 0);
        assert!(
            sys.command_lines()
                .iter()
                .any(|l| l.contains("submodule update --init --recursive --depth 1"))
        );
    }

    #[rstest]
    #[case::clean(|_: &mut Clone| {}, true)]
    #[case::untracked(|c: &mut Clone| c.untracked = true, false)]
    #[case::modified(|c: &mut Clone| c.dirty = vec!["a".into()], false)]
    #[case::local_commit(|c: &mut Clone| c.head = Some(sha(7)), false)]
    #[tokio::test]
    async fn removal_deletes_only_a_clean_clone(
        #[case] tweak: fn(&mut Clone),
        #[case] deleted: bool,
    ) {
        let (sys, world) = host();
        let c = cx(&sys);
        let rec = record(
            "git.repository",
            PATH,
            &apply(&GitRepository, &c, &s(json!({}))).await,
        );
        tweak(world.lock().unwrap().clones.get_mut(PATH).unwrap());
        let out = GitRepository.remove(&c, &rec).await.unwrap();
        assert_eq!(out.changed, deleted, "{}", out.summary);
        assert_eq!(sys.exists(PATH), !deleted);
    }

    #[tokio::test]
    async fn removal_leaves_a_clone_buckle_did_not_create() {
        let (sys, _) = cloned(2, |_| {});
        let c = cx(&sys);
        let rec = record(
            "git.repository",
            PATH,
            &GitRepository
                .apply(&c, &s(json!({})), &Planned::NoOp)
                .await
                .unwrap(),
        );
        let out = GitRepository.remove(&c, &rec).await.unwrap();
        assert!(
            !out.changed && out.summary.contains("before Buckle"),
            "{}",
            out.summary
        );
        assert!(sys.exists(PATH));
    }

    #[rstest]
    #[case::relative_path(json!({"path": "srv/x"}))]
    #[case::option_repo(json!({"repo": "--upload-pack=evil"}))]
    #[case::option_ref(json!({"ref": "-f"}))]
    #[case::range_ref(json!({"ref": "a..b"}))]
    #[case::zero_depth(json!({"depth": 0}))]
    #[tokio::test]
    async fn invalid_specs_are_user_errors(#[case] extra: serde_json::Value) {
        let (sys, _) = host();
        let err = GitRepository.plan(&cx(&sys), &s(extra)).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }
}
