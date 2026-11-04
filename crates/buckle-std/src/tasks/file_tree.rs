//! `file.tree`: copy a directory of files from the module to the host.

use super::archive_extract::glob_match;
use super::hostkit::{self, describe_perms, diffable_text, digest_change};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::redact::Redactor;
use buckle_core::secrets::sinks::FileSink;
use buckle_core::secrets::{Redacted, Rendered};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FileKind, FileMeta, FilePerms};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use tracing::instrument;

/// Copies every file under a directory of the module's `files/` to `dest`.
///
/// ```yaml
/// - file.tree:
///     src: compose                  # a directory under the module's files/
///     dest: /opt/compose
///     mode: "0640"                  # files; owner and group apply to directories too
///     dir_mode: "0750"
///     owner: root
///     group: docker
///     templates: true               # render `*.j2` files (secret-aware), dropping the suffix
///     exclude: ["*.md", ".git"]     # globs over paths relative to `src` / `dest`
///     purge: false                  # also delete files under dest that are not in the tree
/// ```
///
/// The tree is one resource keyed by `dest`, and its plan shows a diff per
/// file. Buckle owns the files it writes: a file dropped from the module is
/// deleted on the next apply (or left in place, and reported, if it was
/// edited by hand since), and a hand-edited file that is still in the module
/// is reported as drift and overwritten. Directories Buckle created are
/// removed once empty. A file already on the host with the same content is
/// adopted rather than owned, so neither of those ever deletes it.
///
/// With `templates: true`, files ending in `.j2` are rendered like
/// `file.template` (secrets never reach the plan or state) and written without
/// the suffix; other files are copied byte for byte. With `purge: true`,
/// anything else under `dest` (that `exclude` does not match) is deleted as
/// well, so `dest` holds exactly the tree.
///
/// Removal deletes the files Buckle wrote that are unchanged since, then the
/// directories it created if they are empty.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Source directory, relative to the module's `files/` directory (`.` for all of it).
    pub src: String,
    /// Absolute destination directory on the host (created if missing).
    pub dest: PathBuf,
    /// `mode` applies to files; `owner` and `group` to files and the directories in the tree.
    #[serde(flatten)]
    pub perms: FilePerms,
    /// Octal mode for `dest` and the directories in the tree, such as `"0755"`.
    #[serde(default, with = "buckle_core::system::mode_serde")]
    #[schemars(with = "Option<String>")]
    pub dir_mode: Option<u32>,
    /// Render files ending in `.j2` as templates and write them without the suffix.
    #[serde(default)]
    pub templates: bool,
    /// Skip paths matching these globs (`*` and `?`), matched against the
    /// relative path, its parent directories and its file name.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Also delete everything under `dest` that is not in the tree (and not excluded).
    #[serde(default)]
    pub purge: bool,
}

pub struct FileTree;
buckle_core::register_task!(FileTree);

/// The content one file should have.
enum Body {
    Bytes(Vec<u8>),
    Rendered {
        rendered: Rendered,
        redacted: Redacted<String>,
    },
}

impl Body {
    fn digest(&self) -> Digest {
        match self {
            Body::Bytes(b) => Digest::of(b),
            Body::Rendered { rendered, .. } => rendered.digest(),
        }
    }

    fn sensitive(&self) -> bool {
        matches!(self, Body::Rendered { rendered, .. } if rendered.is_sensitive())
    }

    /// The displayable text, when it is small UTF-8.
    fn text(&self) -> Option<&str> {
        match self {
            Body::Bytes(b) => diffable_text(b),
            Body::Rendered { redacted, .. } => {
                Some(redacted.get().as_str()).filter(|t| t.len() <= hostkit::SHADOW_LIMIT)
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Body::Bytes(b) => b.len(),
            Body::Rendered { redacted, .. } => redacted.get().len(),
        }
    }
}

/// One file of the tree.
struct Want {
    /// Path relative to `dest`, `/`-separated.
    rel: String,
    target: PathBuf,
    body: Body,
}

/// What Buckle wrote, kept as the resource's shadow: each owned file's digest,
/// and the redacted render of those that hold secrets (the old side of their
/// next diff, since such a file is never read back into a plan).
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    files: BTreeMap<String, Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    digest: Digest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shadow: Option<String>,
}

impl Manifest {
    fn of(record: Option<&ResourceRecord>) -> Self {
        record
            .and_then(|r| r.shadow.as_ref())
            .and_then(|s| serde_json::from_str(s.get()).ok())
            .unwrap_or_default()
    }

    fn digest(&self) -> Digest {
        let lines: Vec<String> = self
            .files
            .iter()
            .map(|(k, e)| format!("{k}\0{}", e.digest.as_str()))
            .collect();
        Digest::of(lines.join("\n").as_bytes())
    }
}

/// Why a path is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// Buckle wrote it and the module no longer has it.
    Dropped,
    /// `purge`: it is not in the tree.
    Purged,
}

enum Op {
    MkDir(PathBuf),
    DirPerms(PathBuf),
    /// Write the content of `wants[idx]`.
    Write {
        idx: usize,
        existing: Option<FileMeta>,
    },
    /// Only set permissions of `wants[idx]`.
    Perms {
        idx: usize,
        owned: bool,
    },
    /// `wants[idx]` already matches.
    Keep {
        idx: usize,
        owned: bool,
    },
    Delete {
        path: PathBuf,
        dir: bool,
        reason: Reason,
    },
    /// A file Buckle wrote, dropped from the module but edited by hand since: left in place.
    Forget,
}

impl Op {
    fn is_change(&self) -> bool {
        !matches!(self, Op::Keep { .. })
    }
}

struct Inspection {
    planned: Planned,
    wants: Vec<Want>,
    ops: Vec<Op>,
    needed_dirs: BTreeSet<PathBuf>,
}

fn rel_string(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The source directory as a relative path (`.` and `""` are the whole `files/`).
fn src_dir(src: &str) -> PathBuf {
    Path::new(src.trim())
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect()
}

/// Whether `rel` (or one of its parent directories, or its file name) matches an `exclude` glob.
fn excluded(patterns: &[String], rel: &str) -> bool {
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
        let mut prefix = String::new();
        rel.split('/').any(|segment| {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            glob_match(pattern, &prefix) || glob_match(pattern, segment)
        })
    })
}

fn dir_perms(spec: &Spec) -> FilePerms {
    FilePerms {
        mode: spec.dir_mode,
        owner: spec.perms.owner.clone(),
        group: spec.perms.group.clone(),
    }
}

/// One file's part of the plan's unified diff, in `---`/`+++` form so it
/// renders like a multi-file `diff -u`.
struct Sections(String);

impl Sections {
    fn header(&mut self, old: &str, new: &str) {
        self.0.push_str(&format!("--- {old}\n+++ {new}\n"));
    }

    fn note(&mut self, old: &str, new: &str, note: &str) {
        self.header(old, new);
        self.0.push_str(&format!("\\ {note}\n"));
    }

    fn text(&mut self, old_name: &str, new_name: &str, old: &str, new: &str, context: usize) {
        let text = similar::TextDiff::from_lines(old, new)
            .unified_diff()
            .context_radius(context)
            .header(old_name, new_name)
            .to_string();
        self.0.push_str(&text);
        if !text.ends_with('\n') {
            self.0.push('\n');
        }
    }
}

impl FileTree {
    async fn wants(&self, cx: &Cx, spec: &Spec) -> Result<Vec<Want>> {
        let src = src_dir(&spec.src);
        let listed = cx.source.list(&src).await?;
        if listed.is_empty() {
            let is_file = !src.as_os_str().is_empty() && cx.source.read(&src).await?.is_some();
            return Err(error::user(
                if is_file {
                    format!("The module path '{}' is a file, not a directory.", spec.src)
                } else {
                    format!("The module directory '{}' has no files.", spec.src)
                },
                &[
                    "Point `src` at a directory under the module's files/ directory.",
                    "Use file.copy or file.template for a single file.",
                ],
            ));
        }
        let mut wants: BTreeMap<String, Want> = BTreeMap::new();
        for rel in listed {
            let src_rel = rel_string(&rel);
            if excluded(&spec.exclude, &src_rel) {
                continue;
            }
            let module_path = rel_string(&src.join(&rel));
            let (dest_rel, body) = match src_rel.strip_suffix(".j2") {
                Some(stripped) if spec.templates && !stripped.ends_with('/') => {
                    let text = hostkit::read_module_text(cx, &module_path).await?;
                    let rendered = cx.templater.render_sensitive(&text, &cx.host)?;
                    let redacted = cx.templater.render_redacted(&text, &cx.host)?;
                    (stripped.to_owned(), Body::Rendered { rendered, redacted })
                }
                _ => (
                    src_rel.clone(),
                    Body::Bytes(hostkit::read_module_file(cx, &module_path).await?),
                ),
            };
            if dest_rel.is_empty() || excluded(&spec.exclude, &dest_rel) {
                continue;
            }
            if wants.contains_key(&dest_rel) {
                return Err(error::user(
                    format!(
                        "Both '{dest_rel}' and '{dest_rel}.j2' in '{}' would be written to {}.",
                        spec.src,
                        spec.dest.join(&dest_rel).display()
                    ),
                    &["Keep only one of them, or exclude the other."],
                ));
            }
            wants.insert(
                dest_rel.clone(),
                Want {
                    target: spec.dest.join(&dest_rel),
                    rel: dest_rel,
                    body,
                },
            );
        }
        if wants.is_empty() {
            return Err(error::user(
                format!(
                    "Every file in the module directory '{}' is excluded.",
                    spec.src
                ),
                &["Narrow the `exclude` patterns."],
            ));
        }
        Ok(wants.into_values().collect())
    }

    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Inspection> {
        hostkit::require_absolute(&spec.dest, "dest")?;
        let depth = spec
            .dest
            .components()
            .filter(|c| matches!(c, Component::Normal(_)))
            .count();
        if spec.purge && depth < 2 {
            return Err(error::user(
                format!(
                    "`purge` would delete everything else under {}.",
                    spec.dest.display()
                ),
                &["Purge a dedicated directory such as /opt/app/config, not a top-level one."],
            ));
        }
        let dest = &spec.dest;
        let wants = self.wants(cx, spec).await?;
        let prev = Manifest::of(cx.previous.as_deref());
        let owned: BTreeSet<PathBuf> = hostkit::ownership::previous_artefacts(cx)
            .iter()
            .filter_map(|a| match a {
                Artefact::File { path } => Some(path.clone()),
                _ => None,
            })
            .collect();

        let mut needed_dirs: BTreeSet<PathBuf> = BTreeSet::from([dest.clone()]);
        for want in &wants {
            needed_dirs.extend(
                want.target
                    .ancestors()
                    .skip(1)
                    .take_while(|a| a.starts_with(dest))
                    .map(Path::to_path_buf),
            );
        }

        let mut ops = Vec::new();
        let mut out = Sections(String::new());
        let (mut new, mut changed, mut perms_only, mut deleted, mut left) = (0, 0, 0, 0, 0);
        let dir_perms = dir_perms(spec);
        let dest_exists = match cx.system.stat(dest).await? {
            Some(meta) if meta.kind == FileKind::File => {
                return Err(error::user(
                    format!(
                        "{} is a file, so file.tree cannot write a directory there.",
                        dest.display()
                    ),
                    &["Point `dest` at a directory."],
                ));
            }
            other => other.is_some(),
        };
        for dir in &needed_dirs {
            let rel = rel_string(dir.strip_prefix(dest).unwrap_or(dir));
            let shown = if rel.is_empty() {
                rel
            } else {
                format!("{rel}/")
            };
            match cx.system.stat(dir).await? {
                None => ops.push(Op::MkDir(dir.clone())),
                Some(meta) if meta.kind == FileKind::Directory => {
                    let changes = hostkit::perm_changes(&meta, &dir_perms);
                    if !changes.is_empty() {
                        out.note(
                            &format!("a/{shown}"),
                            &format!("b/{shown}"),
                            &changes.join(", "),
                        );
                        perms_only += 1;
                        ops.push(Op::DirPerms(dir.clone()));
                    }
                }
                Some(meta) if meta.kind == FileKind::File => {
                    return Err(error::user(
                        format!(
                            "{} is a file, but the tree needs a directory there.",
                            dir.display()
                        ),
                        &["Delete or move the file, or exclude the directory from the tree."],
                    ));
                }
                // A symlink to a directory is followed.
                Some(_) => {}
            }
        }

        for (idx, want) in wants.iter().enumerate() {
            let (a, b) = (format!("a/{}", want.rel), format!("b/{}", want.rel));
            let Some(meta) = cx.system.stat(&want.target).await? else {
                new += 1;
                match want.body.text() {
                    Some(text) => out.text("/dev/null", &b, "", text, 3),
                    None => out.note(
                        "/dev/null",
                        &b,
                        &format!("binary or large file ({} bytes)", want.body.len()),
                    ),
                }
                ops.push(Op::Write {
                    idx,
                    existing: None,
                });
                continue;
            };
            if meta.kind == FileKind::Directory {
                return Err(error::user(
                    format!(
                        "{} is a directory, but the tree has a file there.",
                        want.target.display()
                    ),
                    &["Delete or move the directory, or exclude the file from the tree."],
                ));
            }
            let current = cx.system.read(&want.target).await?.unwrap_or_default();
            let current_digest = Digest::of(&current);
            let new_digest = want.body.digest();
            let is_owned = owned.contains(&want.target);
            let perms = hostkit::perm_changes(&meta, &spec.perms);
            if current_digest == new_digest {
                if perms.is_empty() {
                    ops.push(Op::Keep {
                        idx,
                        owned: is_owned,
                    });
                } else {
                    perms_only += 1;
                    out.note(&a, &b, &perms.join(", "));
                    ops.push(Op::Perms {
                        idx,
                        owned: is_owned,
                    });
                }
                continue;
            }
            changed += 1;
            let previous = prev.files.get(&want.rel);
            let unchanged_since_written =
                is_owned && previous.is_some_and(|e| e.digest == current_digest);
            let mut notes = Vec::new();
            if is_owned && !unchanged_since_written {
                notes.push("changed outside Buckle since it was written; overwriting".to_owned());
            }
            if !perms.is_empty() {
                notes.push(perms.join(", "));
            }
            let held_secrets = previous.is_some_and(|e| e.shadow.is_some());
            let sensitive = want.body.sensitive() || held_secrets;
            let context = if sensitive || hostkit::is_sensitive_path(&want.target) {
                0
            } else {
                3
            };
            let old_text = if sensitive {
                previous
                    .and_then(|e| e.shadow.as_deref())
                    .filter(|_| unchanged_since_written)
            } else {
                diffable_text(&current)
            };
            match (old_text, want.body.text()) {
                (Some(old), Some(new)) => out.text(&a, &b, old, new, context),
                _ => {
                    let why = if sensitive {
                        "; the current content is not shown because the file holds secrets"
                    } else {
                        ""
                    };
                    out.header(&a, &b);
                    notes.push(format!(
                        "content changed ({}){why}",
                        digest_change(&current_digest, &new_digest)
                    ));
                }
            }
            for note in notes {
                out.0.push_str(&format!("\\ {note}\n"));
            }
            ops.push(Op::Write {
                idx,
                existing: Some(meta),
            });
        }

        let wanted: BTreeSet<&Path> = wants.iter().map(|w| w.target.as_path()).collect();
        let mut handled: BTreeSet<PathBuf> = BTreeSet::new();
        for path in owned.iter().filter(|p| !wanted.contains(p.as_path())) {
            let Ok(rel) = path.strip_prefix(dest) else {
                continue;
            };
            let rel = rel_string(rel);
            let Some(bytes) = cx.system.read(path).await? else {
                continue;
            };
            handled.insert(path.clone());
            let a = format!("a/{rel}");
            let intact = prev
                .files
                .get(&rel)
                .is_some_and(|e| e.digest == Digest::of(&bytes));
            if intact || spec.purge {
                deleted += 1;
                out.note(&a, "/dev/null", "deleted: no longer in the module");
                ops.push(Op::Delete {
                    path: path.clone(),
                    dir: false,
                    reason: Reason::Dropped,
                });
            } else {
                left += 1;
                out.note(
                    &a,
                    "/dev/null",
                    "no longer in the module, but changed outside Buckle since it was written: left in place",
                );
                ops.push(Op::Forget);
            }
        }

        if spec.purge && dest_exists {
            let mut stack = vec![dest.clone()];
            while let Some(dir) = stack.pop() {
                for path in cx.system.list_dir(&dir).await?.unwrap_or_default() {
                    let rel = rel_string(path.strip_prefix(dest).unwrap_or(&path));
                    if wanted.contains(path.as_path())
                        || handled.contains(&path)
                        || excluded(&spec.exclude, &rel)
                    {
                        continue;
                    }
                    let is_dir = cx
                        .system
                        .stat(&path)
                        .await?
                        .is_some_and(|m| m.kind == FileKind::Directory);
                    if is_dir && needed_dirs.contains(&path) {
                        stack.push(path);
                        continue;
                    }
                    deleted += 1;
                    let shown = if is_dir {
                        format!("a/{rel}/")
                    } else {
                        format!("a/{rel}")
                    };
                    out.note(&shown, "/dev/null", "deleted: not in the tree (purge)");
                    ops.push(Op::Delete {
                        path,
                        dir: is_dir,
                        reason: Reason::Purged,
                    });
                }
            }
        }

        let planned = if !ops.iter().any(Op::is_change) {
            Planned::NoOp
        } else if !dest_exists {
            let mut preview = String::new();
            for want in &wants {
                preview.push_str(&format!("==> {} <==\n", want.rel));
                match want.body.text() {
                    Some(text) => {
                        preview.push_str(text);
                        if !text.ends_with('\n') {
                            preview.push('\n');
                        }
                    }
                    None => preview.push_str(&format!(
                        "(binary or large file, {} bytes)\n",
                        want.body.len()
                    )),
                }
            }
            Planned::change(Diff::Create {
                summary: format!(
                    "create {} with {} files from {}",
                    dest.display(),
                    wants.len(),
                    spec.src
                ),
                // Module files are plain; renders are already redacted.
                preview: Some(hostkit::truncate_redacted(Redacted::plain(preview))),
            })
        } else {
            let parts: Vec<String> = [
                (new, "new"),
                (changed, "changed"),
                (perms_only, "permissions"),
                (deleted, "deleted"),
                (left, "left in place"),
            ]
            .into_iter()
            .filter(|(n, _)| *n > 0)
            .map(|(n, what)| format!("{n} {what}"))
            .collect();
            let summary = if parts.is_empty() {
                format!("update {}", dest.display())
            } else {
                format!("update {} ({})", dest.display(), parts.join(", "))
            };
            // The host's content goes through the same scrubbing as other editors.
            let unified = Redactor::new(&cx.secrets).redact(&out.0);
            Planned::change(Diff::Update {
                summary,
                unified: Some(hostkit::truncate_redacted(unified)),
            })
        };
        Ok(Inspection {
            planned,
            wants,
            ops,
            needed_dirs,
        })
    }
}

#[async_trait]
impl Task for FileTree {
    type Spec = Spec;
    const KIND: &'static str = "file.tree";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "Copy a directory of files (optionally templated) from the module's files/ directory to the host.";

    fn key(spec: &Spec) -> String {
        spec.dest.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        let perms = describe_perms(&spec.perms);
        let mut out = format!("{} into {}", spec.src, spec.dest.display());
        if !perms.is_empty() {
            out.push_str(&format!(" ({perms})"));
        }
        if spec.purge {
            out.push_str(", purged");
        }
        out
    }

    #[instrument(name = "task.file.tree.plan", skip_all, fields(src = %spec.src, dest = %spec.dest.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(self.inspect(cx, spec).await?.planned)
    }

    #[instrument(name = "task.file.tree.apply", skip_all, fields(src = %spec.src, dest = %spec.dest.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let Inspection {
            planned,
            wants,
            ops,
            needed_dirs,
        } = self.inspect(cx, spec).await?;
        let dest = &spec.dest;
        if !planned.is_change() {
            return Ok(Applied::unchanged(format!(
                "{} already up to date",
                dest.display()
            )));
        }
        let previous = hostkit::ownership::previous_artefacts(cx);
        let mut owned_dirs: BTreeSet<PathBuf> = previous
            .iter()
            .filter_map(|a| match a {
                Artefact::Directory { path } => Some(path.clone()),
                _ => None,
            })
            .collect();
        let dir_perms = dir_perms(spec);
        let mut manifest = Manifest::default();
        let mut files = Vec::new();
        let mut adopted = Vec::new();
        let (mut wrote, mut removed) = (0usize, 0usize);
        let own = |manifest: &mut Manifest, files: &mut Vec<Artefact>, want: &Want| {
            let shadow = (want.body.sensitive())
                .then(|| want.body.text().map(str::to_owned))
                .flatten();
            manifest.files.insert(
                want.rel.clone(),
                Entry {
                    digest: want.body.digest(),
                    shadow,
                },
            );
            files.push(hostkit::file_artefact(&want.target));
        };
        for op in &ops {
            match op {
                Op::MkDir(dir) => {
                    cx.system.mkdir_all(dir, &dir_perms).await?;
                    owned_dirs.insert(dir.clone());
                }
                Op::DirPerms(dir) => cx.system.set_perms(dir, &dir_perms).await?,
                Op::Write { idx, existing } => {
                    let want = &wants[*idx];
                    let perms = hostkit::effective_perms_sensitive(
                        &spec.perms,
                        existing.as_ref(),
                        want.body.sensitive(),
                    );
                    match &want.body {
                        Body::Bytes(bytes) => {
                            cx.system.write_atomic(&want.target, bytes, &perms).await?
                        }
                        Body::Rendered { rendered, .. } => {
                            FileSink
                                .write_rendered(&*cx.system, &want.target, rendered, &perms)
                                .await?
                        }
                    }
                    wrote += 1;
                    own(&mut manifest, &mut files, want);
                }
                Op::Perms { idx, owned } | Op::Keep { idx, owned } => {
                    let want = &wants[*idx];
                    if matches!(op, Op::Perms { .. }) {
                        cx.system.set_perms(&want.target, &spec.perms).await?;
                    }
                    if *owned {
                        own(&mut manifest, &mut files, want);
                    } else {
                        adopted.push(hostkit::ownership::adopted(&want.target));
                    }
                }
                Op::Delete { path, dir, reason } => {
                    cx.system.remove(path, *dir).await?;
                    removed += 1;
                    if *reason == Reason::Purged && *dir {
                        owned_dirs.retain(|d| !d.starts_with(path));
                    }
                }
                Op::Forget => {}
            }
        }
        // Directories Buckle created that the tree no longer needs go once empty.
        let mut stale: Vec<PathBuf> = owned_dirs
            .iter()
            .filter(|d| !needed_dirs.contains(*d))
            .cloned()
            .collect();
        stale.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for dir in stale {
            match cx.system.list_dir(&dir).await? {
                Some(entries) if entries.is_empty() => {
                    cx.system.remove(&dir, false).await?;
                    owned_dirs.remove(&dir);
                }
                Some(_) => {}
                None => {
                    owned_dirs.remove(&dir);
                }
            }
        }
        let mut summary = format!("wrote {wrote} files into {}", dest.display());
        if removed > 0 {
            summary.push_str(&format!(", deleted {removed}"));
        }
        let mut applied = Applied::changed(summary);
        applied.artefacts = owned_dirs
            .into_iter()
            .map(|path| Artefact::Directory { path })
            .chain(files)
            .chain(adopted)
            .collect();
        applied.content_digest = Some(manifest.digest());
        let json = serde_json::to_string(&manifest).expect("a manifest serialises");
        // Digests and redacted renders only.
        applied.shadow = Some(Redacted::plain(json));
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        let files = record
            .artefacts
            .iter()
            .filter(|a| matches!(a, Artefact::File { .. }))
            .count();
        format!(
            "delete the {files} files Buckle wrote under {} that are unchanged since",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let dest = PathBuf::from(&record.key.key);
        let manifest = Manifest::of(Some(record));
        let (mut deleted, mut kept) = (0usize, Vec::new());
        let mut dirs = Vec::new();
        for artefact in &record.artefacts {
            match artefact {
                Artefact::File { path } => {
                    let Some(bytes) = cx.system.read(path).await? else {
                        continue;
                    };
                    let rel = rel_string(path.strip_prefix(&dest).unwrap_or(path));
                    if manifest
                        .files
                        .get(&rel)
                        .is_some_and(|e| e.digest == Digest::of(&bytes))
                    {
                        cx.system.remove(path, false).await?;
                        deleted += 1;
                    } else {
                        kept.push(rel);
                    }
                }
                Artefact::Directory { path } => dirs.push(path.clone()),
                _ => {}
            }
        }
        // Deepest directories first; a directory with other content stays.
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for dir in &dirs {
            if cx.system.list_dir(dir).await?.is_some_and(|e| e.is_empty()) {
                cx.system.remove(dir, false).await?;
            }
        }
        let mut summary = format!("deleted {deleted} files under {}", dest.display());
        if !kept.is_empty() {
            summary.push_str(&format!(
                "; left {} changed outside Buckle in place ({})",
                kept.len(),
                kept.join(", ")
            ));
        }
        Ok(if deleted > 0 {
            Applied::changed(summary)
        } else {
            Applied::skipped(summary)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::secrets::SecretStore;
    use buckle_core::testing::{CxBuilder, FakeSystem, MemorySource};
    use rstest::rstest;
    use serde_json::json;

    fn source() -> MemorySource {
        MemorySource::new()
            .with("compose/docker-compose.yml", "services: {}\n")
            .with("compose/env/app.env", "PORT=80\n")
            .with("compose/README.md", "notes\n")
            .with("other/x", "x\n")
    }

    fn tree(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"src": "compose", "dest": "/opt/compose"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    /// A host plus the state record the engine keeps between applies.
    struct Host {
        sys: FakeSystem,
        record: Option<ResourceRecord>,
        secrets: SecretStore,
    }

    impl Host {
        fn new(sys: FakeSystem) -> Self {
            Self {
                sys,
                record: None,
                secrets: SecretStore::new(),
            }
        }

        fn cx(&self, source: &MemorySource) -> Cx {
            let mut b = CxBuilder::new(self.sys.clone())
                .source(source.clone())
                .secrets(self.secrets.clone());
            if let Some(r) = &self.record {
                b = b.previous(r.clone());
            }
            b.build()
        }

        async fn plan(&self, source: &MemorySource, s: &Spec) -> Planned {
            FileTree
                .plan(&self.cx(source), s)
                .await
                .expect("plan succeeds")
        }

        /// Plans and applies, recording the result as the executor does.
        async fn apply(&mut self, source: &MemorySource, s: &Spec) -> (Planned, Applied) {
            let cx = self.cx(source);
            let planned = FileTree.plan(&cx, s).await.expect("plan succeeds");
            let applied = FileTree
                .apply(&cx, s, &planned)
                .await
                .expect("apply succeeds");
            let mut next = record("file.tree", &s.dest.display().to_string(), &applied);
            if let Some(prev) = &self.record {
                if !(applied.changed && !applied.artefacts.is_empty()) {
                    next.artefacts = prev.artefacts.clone();
                }
                next.shadow = next.shadow.or(prev.shadow.clone());
                next.content_digest = next.content_digest.or(prev.content_digest.clone());
            }
            self.record = Some(next);
            (planned, applied)
        }

        async fn remove(&self) -> Applied {
            let cx = self.cx(&MemorySource::new());
            FileTree
                .remove(&cx, self.record.as_ref().expect("applied before"))
                .await
                .expect("remove succeeds")
        }
    }

    fn unified(planned: &Planned) -> String {
        match diff(planned) {
            Diff::Update {
                unified: Some(u), ..
            } => u.get().clone(),
            other => panic!("expected an update with a diff, got {other:?}"),
        }
    }

    fn mode_of(sys: &FakeSystem, path: &str) -> Option<u32> {
        match sys.node(path) {
            Some(buckle_core::testing::Node::File { perms, .. })
            | Some(buckle_core::testing::Node::Dir { perms }) => perms.mode,
            _ => None,
        }
    }

    #[tokio::test]
    async fn apply_writes_the_tree_with_perms_and_is_then_a_no_op() {
        let mut host = Host::new(FakeSystem::new());
        let s = tree(json!({"mode": "0640", "dir_mode": "0750", "group": "docker"}));
        let (planned, applied) = host.apply(&source(), &s).await;
        let Diff::Create {
            preview: Some(p), ..
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert!(p.get().contains("env/app.env") && p.get().contains("PORT=80"));
        assert!(applied.changed);
        assert_eq!(
            host.sys.file_string("/opt/compose/env/app.env").unwrap(),
            "PORT=80\n"
        );
        assert!(!host.sys.exists("/opt/compose/x"), "only src is copied");
        assert_eq!(
            mode_of(&host.sys, "/opt/compose/docker-compose.yml"),
            Some(0o640)
        );
        assert_eq!(mode_of(&host.sys, "/opt/compose/env"), Some(0o750));
        assert_eq!(class(&host.plan(&source(), &s).await), Class::NoOp);
    }

    #[rstest]
    #[case::content(json!({}), "PORT=8080\n", 0o644, "-PORT=8080")]
    #[case::mode(json!({"mode": "0600"}), "PORT=80\n", 0o644, "mode 0644 → 0600")]
    #[tokio::test]
    async fn an_existing_tree_diffs_per_file(
        #[case] extra: serde_json::Value,
        #[case] current: &str,
        #[case] mode: u32,
        #[case] expected: &str,
    ) {
        let sys = FakeSystem::new()
            .with_file(
                "/opt/compose/docker-compose.yml",
                "services: {}\n",
                FilePerms::mode(mode),
            )
            .with_file("/opt/compose/README.md", "notes\n", FilePerms::mode(mode))
            .with_file("/opt/compose/env/app.env", current, FilePerms::mode(mode));
        let host = Host::new(sys);
        let planned = host.plan(&source(), &tree(extra)).await;
        let text = unified(&planned);
        assert!(text.contains("b/env/app.env"), "{text}");
        assert!(text.contains(expected), "{text}");
        assert!(!text.contains("b/docker-compose.yml") || expected.starts_with("mode"));
    }

    #[tokio::test]
    async fn exclude_skips_paths_by_name_or_directory() {
        let mut host = Host::new(FakeSystem::new());
        host.apply(&source(), &tree(json!({"exclude": ["*.md", "env"]})))
            .await;
        assert!(host.sys.exists("/opt/compose/docker-compose.yml"));
        assert!(!host.sys.exists("/opt/compose/README.md"));
        assert!(!host.sys.exists("/opt/compose/env/app.env"));
    }

    #[tokio::test]
    async fn templates_render_without_the_suffix_and_keep_secrets_out_of_plans_and_state() {
        const SECRET: &str = "hunter2-very-secret";
        let src = MemorySource::new()
            .with("app/app.env.j2", "TOKEN={{ secrets.token }}\n")
            .with("app/raw.j2.txt", "{{ untouched }}\n");
        let mut host = Host::new(FakeSystem::new());
        host.secrets = secrets("token", SECRET);
        let s: Spec = spec(json!({"src": "app", "dest": "/opt/app", "templates": true}));
        let (planned, applied) = host.apply(&src, &s).await;
        assert_eq!(
            host.sys.file_string("/opt/app/app.env").unwrap(),
            format!("TOKEN={SECRET}\n")
        );
        assert_eq!(mode_of(&host.sys, "/opt/app/app.env"), Some(0o600));
        assert_eq!(
            host.sys.file_string("/opt/app/raw.j2.txt").unwrap(),
            "{{ untouched }}\n"
        );
        assert!(!serialised(&planned, &applied).contains(SECRET));

        // A rotated secret diffs against the redacted shadow, never the file.
        host.secrets = secrets("token", "rotated-value-xyz");
        let (planned, applied) = host.apply(&src, &s).await;
        let text = unified(&planned);
        assert!(text.contains("secret:token@"), "{text}");
        let everything = serialised(&planned, &applied);
        assert!(!everything.contains(SECRET) && !everything.contains("rotated-value-xyz"));
    }

    #[tokio::test]
    async fn a_file_dropped_from_the_module_is_deleted_unless_edited_by_hand() {
        let mut host = Host::new(FakeSystem::new());
        let s = tree(json!({}));
        host.apply(&source(), &s).await;

        // Hand edits: one file that stays in the module, one that leaves it.
        put(&host.sys, "/opt/compose/docker-compose.yml", "edited\n").await;
        put(&host.sys, "/opt/compose/README.md", "mine now\n").await;
        let fewer = MemorySource::new().with("compose/docker-compose.yml", "services: {}\n");
        let (planned, _) = host.apply(&fewer, &s).await;
        let text = unified(&planned);
        assert!(text.contains("changed outside Buckle"), "{text}");
        assert!(text.contains("left in place"), "{text}");
        assert_eq!(
            host.sys
                .file_string("/opt/compose/docker-compose.yml")
                .unwrap(),
            "services: {}\n",
            "drift is overwritten"
        );
        assert!(
            host.sys.exists("/opt/compose/README.md"),
            "hand-edited, left"
        );
        assert!(
            !host.sys.exists("/opt/compose/env/app.env"),
            "dropped, deleted"
        );
        assert!(
            !host.sys.exists("/opt/compose/env"),
            "emptied directory removed"
        );
        assert_eq!(class(&host.plan(&fewer, &s).await), Class::NoOp);
    }

    #[tokio::test]
    async fn purge_deletes_everything_else_except_excluded_paths() {
        let sys = FakeSystem::new()
            .with_file("/opt/compose/old.yml", "x\n", FilePerms::default())
            .with_file("/opt/compose/stale/a", "x\n", FilePerms::default())
            .with_file("/opt/compose/data/db", "keep\n", FilePerms::default());
        let mut host = Host::new(sys);
        let s = tree(json!({"purge": true, "exclude": ["data"]}));
        let (planned, _) = host.apply(&source(), &s).await;
        assert!(unified(&planned).contains("purge"));
        assert!(!host.sys.exists("/opt/compose/old.yml"));
        assert!(!host.sys.exists("/opt/compose/stale"));
        assert!(host.sys.exists("/opt/compose/data/db"));
        assert!(host.sys.exists("/opt/compose/env/app.env"));
        assert_eq!(class(&host.plan(&source(), &s).await), Class::NoOp);
    }

    #[tokio::test]
    async fn removal_deletes_unchanged_files_and_leaves_edited_and_adopted_ones() {
        let sys =
            FakeSystem::new().with_file("/opt/compose/README.md", "notes\n", FilePerms::default());
        let mut host = Host::new(sys);
        host.apply(&source(), &tree(json!({}))).await;
        put(&host.sys, "/opt/compose/env/app.env", "edited\n").await;
        let out = host.remove().await;
        assert!(out.changed);
        assert!(out.summary.contains("env/app.env"), "{}", out.summary);
        assert!(!host.sys.exists("/opt/compose/docker-compose.yml"));
        assert!(host.sys.exists("/opt/compose/env/app.env"));
        assert!(
            host.sys.exists("/opt/compose/README.md"),
            "existed with the same content before Buckle"
        );
    }

    #[tokio::test]
    async fn removal_clears_the_directories_it_created() {
        let mut host = Host::new(FakeSystem::new().with_dir("/opt", FilePerms::default()));
        host.apply(&source(), &tree(json!({}))).await;
        host.remove().await;
        assert!(!host.sys.exists("/opt/compose"));
        assert!(host.sys.exists("/opt"));
    }

    #[rstest]
    #[case::missing_src(MemorySource::new(), json!({}))]
    #[case::src_is_a_file(MemorySource::new().with("compose", "x"), json!({}))]
    #[case::all_excluded(source(), json!({"exclude": ["*"]}))]
    #[case::template_collides(
        MemorySource::new().with("compose/a", "x").with("compose/a.j2", "y"),
        json!({"templates": true})
    )]
    #[case::relative_dest(source(), json!({"dest": "opt/compose"}))]
    #[case::purge_top_level(source(), json!({"dest": "/opt", "purge": true}))]
    #[tokio::test]
    async fn invalid_trees_are_user_errors(
        #[case] src: MemorySource,
        #[case] extra: serde_json::Value,
    ) {
        let host = Host::new(FakeSystem::new());
        let err = FileTree
            .plan(&host.cx(&src), &tree(extra))
            .await
            .unwrap_err();
        assert!(
            err.is(buckle_core::error::Kind::User),
            "{}",
            err.description()
        );
    }

    #[tokio::test]
    async fn a_file_where_the_tree_needs_a_directory_is_an_error() {
        let host =
            Host::new(FakeSystem::new().with_file("/opt/compose/env", "x", FilePerms::default()));
        assert!(
            FileTree
                .plan(&host.cx(&source()), &tree(json!({})))
                .await
                .is_err()
        );
    }

    #[rstest]
    #[case("*.md", "docs/README.md", true)]
    #[case("env", "env/app.env", true)]
    #[case(".git", "sub/.git/config", true)]
    #[case("docs/*", "docs/a/b", true)]
    #[case("*.md", "docker-compose.yml", false)]
    #[case("env", "environment", false)]
    fn exclude_patterns(#[case] pattern: &str, #[case] rel: &str, #[case] expected: bool) {
        assert_eq!(excluded(&[pattern.to_owned()], rel), expected);
    }
}
