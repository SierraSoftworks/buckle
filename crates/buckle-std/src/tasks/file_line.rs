//! `file.line`: ensure a single line is present in (or absent from) a text file.

use super::hostkit::{self, Ensure};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::FilePerms;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result, error};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Ensures a line is present in (or absent from) a text file, like Ansible's `lineinfile`.
///
/// ```yaml
/// - file.line:
///     path: /etc/ssh/sshd_config
///     line: "PasswordAuthentication no"
///     regex: '^#?PasswordAuthentication\b'   # replace the last matching line
///
/// - file.line:                      # without `regex`: append when missing
///     path: /etc/hosts
///     line: "10.0.0.5 vault.internal"
///
/// - file.line:                      # remove every matching line
///     path: /etc/fstab
///     state: absent
///     regex: '\sswap\s'
/// ```
///
/// With `state: present` (the default): when `regex` matches, the **last**
/// matching line is replaced by `line`; when nothing matches, `line` is
/// inserted unless it is already in the file. Insertion goes after the last
/// line matching `insert_after` (or `EOF`), before the last line matching
/// `insert_before` (or `BOF`), and otherwise at the end of the file.
///
/// With `state: absent`, every line matching `regex` (or, without `regex`,
/// equal to `line`) is deleted.
///
/// The file must exist unless `create: true`. Mode and ownership are
/// preserved. `backup: true` keeps a timestamped copy before each edit.
///
/// `validate` is a command (argv) that checks the new content before it
/// replaces the file (and while planning); `{path}` stands for a private
/// temporary copy under `<state_dir>/tmp`, deleted afterwards. A non-zero
/// exit fails the task and leaves the file untouched. Without `validate`,
/// `/etc/sudoers`, `/etc/sudoers.d/*` and `sshd_config` files are checked with
/// `visudo -cf {path}` and `sshd -t -f {path}` (when those tools are
/// installed); `validate: []` turns that off.
///
/// Removal reverts the edit: a line Buckle added is deleted again (one
/// occurrence only, so an identical line another task owns stays), a line it
/// replaced is restored to its original text, and a file it created is deleted
/// when it ends up empty. Edits made by `state: absent` are not restored.
///
/// The resource is identified by `path` and `regex` when a `regex` is given
/// (else by `path` and `line`), so changing the `line` of a regex task updates
/// the same line in place and removal still restores the text from before
/// Buckle's first edit. Edits of one file are serialised across modules.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Absolute path of the file to edit.
    pub path: PathBuf,
    /// The line to ensure (required for `state: present`; must be a single line).
    #[serde(default)]
    pub line: Option<String>,
    /// Pattern selecting the line to replace (present) or delete (absent).
    #[serde(default)]
    pub regex: Option<String>,
    #[serde(default)]
    pub state: Ensure,
    /// Insert after the last line matching this pattern, or `EOF`.
    #[serde(default)]
    pub insert_after: Option<String>,
    /// Insert before the last line matching this pattern, or `BOF`.
    #[serde(default)]
    pub insert_before: Option<String>,
    /// Create the file when it does not exist.
    #[serde(default)]
    pub create: bool,
    /// Keep a timestamped `.bak` copy before editing.
    #[serde(default)]
    pub backup: bool,
    /// A command that checks the new content; `{path}` stands for the file to check.
    /// Default: automatic for sudoers and sshd_config; `[]` disables.
    #[serde(default)]
    pub validate: Option<Vec<String>>,
}

pub struct FileLine;
buckle_core::register_task!(FileLine);

/// What an edit did, recorded so that removal can revert it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Marker {
    /// `line` was appended or inserted.
    Added { line: String, created: bool },
    /// `original` was replaced by `line`.
    Replaced {
        line: String,
        /// The replaced text; empty when it was stashed instead (sensitive files).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        original: String,
        /// For sensitive files (`/etc/ssh/*`, `/etc/environment`, ...): the
        /// digest of the replaced text, which is kept in a 0600 file under
        /// `<state_dir>/stash/file.line/` rather than in state (which is
        /// reported to the server).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stashed: Option<String>,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Edit {
    Unchanged,
    Changed {
        lines: Vec<String>,
        marker: Option<Marker>,
        summary: String,
    },
}

fn compile(spec: &Spec) -> Result<Option<Regex>> {
    spec.regex
        .as_deref()
        .map(|p| hostkit::compile_regex(p, "regex"))
        .transpose()
}

fn wanted_line(spec: &Spec) -> Result<&str> {
    let line = spec.line.as_deref();
    match (spec.state, line) {
        (Ensure::Present, None) => Err(error::user(
            "file.line with `state: present` needs a `line`.",
            &["Add `line: \"...\"`, or use `state: absent` to delete lines."],
        )),
        (Ensure::Absent, None) if spec.regex.is_none() => Err(error::user(
            "file.line with `state: absent` needs a `regex` or a `line` to match.",
            &["Add `regex: '...'` selecting the lines to delete."],
        )),
        (_, Some(l)) if l.contains('\n') => Err(error::user(
            "The `line` of a file.line task must be a single line.",
            &["Use file.block to manage several lines at once."],
        )),
        (_, l) => Ok(l.unwrap_or("")),
    }
}

/// Computes the edited lines. Pure, so the cases are tested without a host.
pub(crate) fn edit(spec: &Spec, current: &str) -> Result<Edit> {
    let line = wanted_line(spec)?;
    let regex = compile(spec)?;
    let mut lines = hostkit::split_lines(current);
    match spec.state {
        Ensure::Absent => {
            let before = lines.len();
            match &regex {
                Some(re) => lines.retain(|l| !re.is_match(l)),
                None => lines.retain(|l| l != line),
            }
            let removed = before - lines.len();
            Ok(if removed == 0 {
                Edit::Unchanged
            } else {
                Edit::Changed {
                    lines,
                    marker: None,
                    summary: format!("delete {removed} line(s) from {}", spec.path.display()),
                }
            })
        }
        Ensure::Present => {
            let matched = regex
                .as_ref()
                .and_then(|re| lines.iter().rposition(|l| re.is_match(l)));
            if let Some(index) = matched {
                if lines[index] == line {
                    return Ok(Edit::Unchanged);
                }
                let original = std::mem::replace(&mut lines[index], line.to_owned());
                return Ok(Edit::Changed {
                    lines,
                    marker: Some(Marker::Replaced {
                        line: line.to_owned(),
                        original,
                        stashed: None,
                    }),
                    summary: format!("replace line {} in {}", index + 1, spec.path.display()),
                });
            }
            if lines.iter().any(|l| l == line) {
                return Ok(Edit::Unchanged);
            }
            let at = hostkit::insertion_index(
                &lines,
                spec.insert_after.as_deref(),
                spec.insert_before.as_deref(),
            )?;
            lines.insert(at, line.to_owned());
            Ok(Edit::Changed {
                lines,
                marker: Some(Marker::Added {
                    line: line.to_owned(),
                    created: false,
                }),
                summary: format!("insert a line at {} in {}", at + 1, spec.path.display()),
            })
        }
    }
}

#[allow(clippy::large_enum_variant)]
enum Outcome {
    NoOp,
    Blocked(String),
    Change {
        diff: Diff,
        new_text: String,
        marker: Option<Marker>,
        existing: Option<hostkit::TextFile>,
    },
}

impl FileLine {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Outcome> {
        hostkit::require_absolute(&spec.path, "path")?;
        let existing = hostkit::load_text(cx, &spec.path).await?;
        let Some(file) = existing else {
            if spec.state == Ensure::Absent {
                wanted_line(spec)?;
                return Ok(Outcome::NoOp);
            }
            let line = wanted_line(spec)?;
            if !spec.create {
                return Ok(Outcome::Blocked(format!(
                    "{} does not exist; set `create: true` to create it",
                    spec.path.display()
                )));
            }
            let new_text = hostkit::join_lines(&[line.to_owned()]);
            return Ok(Outcome::Change {
                diff: Diff::Create {
                    summary: format!("create {} containing the line", spec.path.display()),
                    preview: Some(buckle_core::secrets::Redacted::plain(new_text.clone())),
                },
                new_text,
                marker: Some(Marker::Added {
                    line: line.to_owned(),
                    created: true,
                }),
                existing: None,
            });
        };
        Ok(match edit(spec, &file.text)? {
            Edit::Unchanged => Outcome::NoOp,
            Edit::Changed {
                lines,
                marker,
                summary,
            } => {
                let new_text = hostkit::join_lines(&lines);
                Outcome::Change {
                    diff: hostkit::text_diff(cx, &spec.path, summary, &file.text, &new_text),
                    new_text,
                    marker,
                    existing: Some(file),
                }
            }
        })
    }
}

#[async_trait]
impl Task for FileLine {
    type Spec = Spec;
    const KIND: &'static str = "file.line";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Ensure a line is present in, replaced in or removed from a text file (lineinfile).";

    fn key(spec: &Spec) -> String {
        format!(
            "{}: {}",
            spec.path.display(),
            spec.regex
                .as_deref()
                .or(spec.line.as_deref())
                .unwrap_or_default()
        )
    }

    fn describe(spec: &Spec) -> String {
        match spec.state {
            Ensure::Present => format!("line in {}", spec.path.display()),
            Ensure::Absent => format!("no matching line in {}", spec.path.display()),
        }
    }

    #[instrument(name = "task.file.line.plan", skip_all, fields(path = %spec.path.display(), state = ?spec.state))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(match self.inspect(cx, spec).await? {
            Outcome::NoOp => Planned::NoOp,
            Outcome::Blocked(reason) => Planned::blocked(reason),
            Outcome::Change { diff, new_text, .. } => {
                match hostkit::validate::check(
                    cx,
                    &spec.path,
                    spec.validate.as_deref(),
                    hostkit::validate::Content::Bytes(new_text.as_bytes()),
                )
                .await?
                {
                    hostkit::validate::Checked::ToolMissing(reason) => Planned::blocked(reason),
                    hostkit::validate::Checked::Passed => Planned::change(diff),
                }
            }
        })
    }

    #[instrument(name = "task.file.line.apply", skip_all, fields(path = %spec.path.display(), state = ?spec.state))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let _guard = hostkit::lock_path(&spec.path).await;
        match self.inspect(cx, spec).await? {
            Outcome::NoOp => Ok(hostkit::ownership::carry_forward(
                cx,
                Applied::unchanged(format!("{} already up to date", spec.path.display())),
            )),
            Outcome::Blocked(reason) => Err(error::user(
                format!("Cannot edit {}: {reason}.", spec.path.display()),
                &["Set `create: true` or create the file in an earlier task."],
            )),
            Outcome::Change {
                diff,
                new_text,
                marker,
                existing,
            } => {
                hostkit::validate::check(
                    cx,
                    &spec.path,
                    spec.validate.as_deref(),
                    hostkit::validate::Content::Bytes(new_text.as_bytes()),
                )
                .await?;
                hostkit::save_text(
                    cx,
                    &spec.path,
                    &new_text,
                    existing.as_ref(),
                    spec.backup,
                    &FilePerms::default(),
                )
                .await?;
                let mut applied = Applied::changed(diff.summary().to_owned());
                let mut marker = marker.map(|m| merge_marker(previous_marker(cx, &spec.path), m));
                if hostkit::is_sensitive_path(&spec.path)
                    && let Some(m) = marker.take()
                {
                    marker = Some(stash_original(cx, m).await?);
                }
                if let Some(marker) = marker {
                    applied = applied.with_artefact(Artefact::Line {
                        path: spec.path.clone(),
                        marker: serde_json::to_string(&marker).map_err(|e| {
                            error::wrap_system(
                                e,
                                "Could not record the line edit.",
                                &["This is a bug in Buckle; please report it."],
                            )
                        })?,
                    });
                }
                Ok(applied)
            }
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        match line_artefact(record) {
            Some((path, Marker::Added { line, .. })) => {
                format!("delete the line `{line}` from {}", path.display())
            }
            Some((
                path,
                Marker::Replaced {
                    stashed: Some(_), ..
                },
            )) => {
                format!("restore the original line in {}", path.display())
            }
            Some((path, Marker::Replaced { original, .. })) => {
                format!("restore the line `{original}` in {}", path.display())
            }
            None => format!("nothing to revert for {}", record.key.key),
        }
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        if let Some((path, _)) = line_artefact(record) {
            let _guard = hostkit::lock_path(&path).await;
            return self.revert(cx, record).await;
        }
        self.revert(cx, record).await
    }
}

impl FileLine {
    async fn revert(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some((path, marker)) = line_artefact(record) else {
            return Ok(Applied::skipped(
                "no edit was recorded to revert (removed lines are not restored)",
            ));
        };
        let Some(file) = hostkit::load_text(cx, &path).await? else {
            return Ok(Applied::unchanged(format!(
                "{} no longer exists",
                path.display()
            )));
        };
        let mut lines = hostkit::split_lines(&file.text);
        match &marker {
            Marker::Added { line, created } => {
                // One occurrence only: an identical line may belong to another task.
                let Some(index) = lines.iter().rposition(|l| l == line) else {
                    return Ok(Applied::skipped(format!(
                        "the line is no longer in {}, nothing to revert",
                        path.display()
                    )));
                };
                lines.remove(index);
                if *created && lines.iter().all(|l| l.trim().is_empty()) {
                    cx.system.remove(&path, false).await?;
                    return Ok(Applied::changed(format!(
                        "deleted {}, which Buckle created",
                        path.display()
                    )));
                }
            }
            Marker::Replaced {
                line,
                original,
                stashed,
            } => {
                let Some(index) = lines.iter().position(|l| l == line) else {
                    return Ok(Applied::skipped(format!(
                        "the line was changed outside Buckle in {}, left in place",
                        path.display()
                    )));
                };
                lines[index] = match stashed {
                    None => original.clone(),
                    Some(digest) => match unstash(cx, digest).await? {
                        Some(text) => text,
                        None => {
                            return Ok(Applied::skipped(format!(
                                "the original line of {} was not found in Buckle's stash, left as it is",
                                path.display()
                            )));
                        }
                    },
                };
            }
        }
        hostkit::save_text(
            cx,
            &path,
            &hostkit::join_lines(&lines),
            Some(&file),
            false,
            &FilePerms::default(),
        )
        .await?;
        if let Marker::Replaced {
            stashed: Some(digest),
            ..
        } = &marker
        {
            let _ = cx.system.remove(&stash_path(cx, digest), false).await;
        }
        Ok(Applied::changed(format!(
            "reverted the edit in {}",
            path.display()
        )))
    }
}

/// The edit an earlier apply of this resource recorded for `path`.
fn previous_marker(cx: &Cx, path: &std::path::Path) -> Option<Marker> {
    hostkit::ownership::previous_artefacts(cx)
        .iter()
        .find_map(|a| match a {
            Artefact::Line { path: p, marker } if p == path => serde_json::from_str(marker).ok(),
            _ => None,
        })
}

/// Combines this apply's edit with the one recorded before, so that removal
/// reverts to the state before Buckle's *first* edit: a line Buckle added and
/// now rewrites is still an added line; a line it replaced and now rewrites
/// again still restores the original text.
fn merge_marker(previous: Option<Marker>, now: Marker) -> Marker {
    match (previous, now) {
        (Some(Marker::Added { line: old, created }), Marker::Replaced { line, original, .. })
            if original == old =>
        {
            Marker::Added { line, created }
        }
        (
            Some(Marker::Replaced {
                line: old,
                original: first,
                stashed,
            }),
            Marker::Replaced { line, original, .. },
        ) if original == old => Marker::Replaced {
            line,
            original: first,
            stashed,
        },
        (_, now) => now,
    }
}

fn stash_path(cx: &Cx, digest: &str) -> PathBuf {
    crate::tasks::sys_exec::state_dir(cx)
        .join("stash")
        .join("file.line")
        .join(digest)
}

/// Keeps the replaced text of a sensitive file out of state: it goes to a
/// root-only file on the host, and the marker records only its digest.
async fn stash_original(cx: &Cx, marker: Marker) -> Result<Marker> {
    let Marker::Replaced {
        line,
        original,
        stashed: None,
    } = marker
    else {
        return Ok(marker);
    };
    let digest = buckle_core::Digest::of(original.as_bytes());
    let path = stash_path(cx, digest.as_str());
    if let Some(dir) = path.parent() {
        cx.system.mkdir_all(dir, &FilePerms::mode(0o700)).await?;
    }
    cx.system
        .write_atomic(&path, original.as_bytes(), &FilePerms::mode(0o600))
        .await?;
    Ok(Marker::Replaced {
        line,
        original: String::new(),
        stashed: Some(digest.as_str().to_owned()),
    })
}

async fn unstash(cx: &Cx, digest: &str) -> Result<Option<String>> {
    Ok(cx
        .system
        .read(&stash_path(cx, digest))
        .await?
        .filter(|b| buckle_core::Digest::of(b).as_str() == digest)
        .and_then(|b| String::from_utf8(b).ok()))
}

fn line_artefact(record: &ResourceRecord) -> Option<(PathBuf, Marker)> {
    record.artefacts.iter().find_map(|a| match a {
        Artefact::Line { path, marker } => {
            serde_json::from_str(marker).ok().map(|m| (path.clone(), m))
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"path": "/etc/conf"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    #[rstest]
    #[case::append_when_missing(json!({"line": "c=3"}), "a=1\nb=2\n", "a=1\nb=2\nc=3\n")]
    #[case::present_is_unchanged(json!({"line": "b=2"}), "a=1\nb=2\n", "a=1\nb=2\n")]
    #[case::regex_replaces(json!({"line": "b=9", "regex": "^b="}), "a=1\nb=2\n", "a=1\nb=9\n")]
    #[case::regex_replaces_last_match(json!({"line": "x=9", "regex": "^x="}), "x=1\nmid\nx=2\n", "x=1\nmid\nx=9\n")]
    #[case::regex_already_correct(json!({"line": "b=2", "regex": "^b="}), "a=1\nb=2\n", "a=1\nb=2\n")]
    #[case::regex_without_match_appends(json!({"line": "z=1", "regex": "^z="}), "a=1\n", "a=1\nz=1\n")]
    #[case::regex_without_match_but_line_present(json!({"line": "z=1", "regex": "^nomatch"}), "z=1\n", "z=1\n")]
    #[case::insert_after(json!({"line": "new", "insert_after": "^\\[main\\]"}), "[main]\na=1\n[other]\n", "[main]\nnew\na=1\n[other]\n")]
    #[case::insert_after_missing_pattern_appends(json!({"line": "new", "insert_after": "^nope"}), "a\n", "a\nnew\n")]
    #[case::insert_before(json!({"line": "new", "insert_before": "^\\[other\\]"}), "[main]\na=1\n[other]\n", "[main]\na=1\nnew\n[other]\n")]
    #[case::insert_before_bof(json!({"line": "first", "insert_before": "BOF"}), "a\n", "first\na\n")]
    #[case::insert_after_eof(json!({"line": "last", "insert_after": "EOF"}), "a\nb\n", "a\nb\nlast\n")]
    #[case::unterminated_last_line_gets_terminated(json!({"line": "b"}), "a", "a\nb\n")]
    #[case::empty_file(json!({"line": "b"}), "", "b\n")]
    #[case::absent_by_line(json!({"line": "a=1", "state": "absent"}), "a=1\nb=2\na=1\n", "b=2\n")]
    #[case::absent_by_regex(json!({"regex": "^#", "state": "absent"}), "# c\na=1\n#d\n", "a=1\n")]
    #[case::absent_nothing_to_remove(json!({"line": "zzz", "state": "absent"}), "a=1\n", "a=1\n")]
    fn edit_cases(#[case] extra: serde_json::Value, #[case] before: &str, #[case] after: &str) {
        let spec = s(extra);
        match edit(&spec, before).unwrap() {
            Edit::Unchanged => {
                assert_eq!(before.trim_end_matches('\n'), after.trim_end_matches('\n'))
            }
            Edit::Changed { lines, .. } => assert_eq!(hostkit::join_lines(&lines), after),
        }
    }

    #[rstest]
    #[case::present_needs_a_line(json!({}))]
    #[case::multi_line_is_rejected(json!({"line": "a\nb"}))]
    #[case::absent_needs_something(json!({"state": "absent"}))]
    #[case::bad_regex(json!({"line": "a", "regex": "("}))]
    fn misconfiguration_is_a_user_error(#[case] extra: serde_json::Value) {
        let err = edit(&s(extra), "x\n").unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }

    #[tokio::test]
    async fn missing_files_need_create() {
        let sys = FakeSystem::new();
        let c = cx(&sys);
        let spec = s(json!({"line": "a=1"}));
        assert_eq!(plan_class(&FileLine, &c, &spec).await, Class::Blocked);
        assert!(FileLine.apply(&c, &spec, &Planned::NoOp).await.is_err());

        let spec = s(json!({"line": "a=1", "create": true}));
        let applied = apply(&FileLine, &c, &spec).await;
        assert!(applied.changed);
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "a=1\n");
        assert_eq!(plan_class(&FileLine, &c, &spec).await, Class::NoOp);

        // Absent on a missing file is simply done.
        let sys = FakeSystem::new();
        let absent = s(json!({"line": "a=1", "state": "absent"}));
        assert_eq!(plan_class(&FileLine, &cx(&sys), &absent).await, Class::NoOp);
    }

    #[tokio::test]
    async fn apply_edits_in_place_keeps_perms_and_is_idempotent() {
        let sys = FakeSystem::new().with_file(
            "/etc/conf",
            "a=1\nb=2\n",
            FilePerms {
                mode: Some(0o600),
                owner: Some("app".into()),
                group: None,
            },
        );
        let c = cx(&sys);
        let spec = s(json!({"line": "b=9", "regex": "^b="}));
        let planned = FileLine.plan(&c, &spec).await.unwrap();
        let Diff::Update {
            unified: Some(u), ..
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert!(u.get().contains("-b=2") && u.get().contains("+b=9"));
        FileLine.apply(&c, &spec, &planned).await.unwrap();
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "a=1\nb=9\n");
        let meta = buckle_core::system::System::stat(&sys, std::path::Path::new("/etc/conf"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (meta.mode, meta.owner.as_deref()),
            (Some(0o600), Some("app"))
        );
        assert_eq!(plan_class(&FileLine, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn backup_keeps_the_previous_content() {
        let sys = FakeSystem::new().with_file("/etc/conf", "a=1\n", FilePerms::default());
        let spec = s(json!({"line": "b=2", "backup": true}));
        apply(&FileLine, &cx(&sys), &spec).await;
        assert!(
            sys.paths()
                .iter()
                .any(|p| p.to_string_lossy().ends_with(".bak"))
        );
    }

    #[tokio::test]
    async fn removal_deletes_added_lines_and_restores_replaced_ones() {
        let sys = FakeSystem::new().with_file("/etc/conf", "a=1\nb=2\n", FilePerms::default());
        let c = cx(&sys);

        let added = s(json!({"line": "c=3"}));
        let rec_added = record("file.line", "k1", &apply(&FileLine, &c, &added).await);
        let replaced = s(json!({"line": "b=9", "regex": "^b="}));
        let rec_replaced = record("file.line", "k2", &apply(&FileLine, &c, &replaced).await);
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "a=1\nb=9\nc=3\n");
        assert!(
            FileLine
                .describe_removal(&rec_replaced)
                .contains("restore the line `b=2`")
        );

        let out = FileLine.remove(&c, &rec_replaced).await.unwrap();
        assert!(out.changed);
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "a=1\nb=2\nc=3\n");
        assert!(FileLine.remove(&c, &rec_added).await.unwrap().changed);
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "a=1\nb=2\n");
        // Reverting twice is harmless.
        assert!(!FileLine.remove(&c, &rec_added).await.unwrap().changed);
    }

    #[tokio::test]
    async fn removal_leaves_lines_that_were_edited_by_hand() {
        let sys = FakeSystem::new().with_file("/etc/conf", "b=2\n", FilePerms::default());
        let c = cx(&sys);
        let spec = s(json!({"line": "b=9", "regex": "^b="}));
        let rec = record("file.line", "k", &apply(&FileLine, &c, &spec).await);
        put(&sys, "/etc/conf", "b=hand-edit\n").await;
        let out = FileLine.remove(&c, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("left in place"));
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "b=hand-edit\n");
    }

    #[tokio::test]
    async fn removal_deletes_files_it_created_once_empty() {
        let sys = FakeSystem::new();
        let c = cx(&sys);
        let spec = s(json!({"line": "a=1", "create": true}));
        let rec = record("file.line", "k", &apply(&FileLine, &c, &spec).await);
        assert!(FileLine.remove(&c, &rec).await.unwrap().changed);
        assert!(!sys.exists("/etc/conf"));
    }

    #[tokio::test]
    async fn absent_records_nothing_to_restore() {
        let sys = FakeSystem::new().with_file("/etc/conf", "a=1\nb=2\n", FilePerms::default());
        let c = cx(&sys);
        let spec = s(json!({"line": "a=1", "state": "absent"}));
        let applied = apply(&FileLine, &c, &spec).await;
        assert!(applied.artefacts.is_empty());
        assert_eq!(plan_class(&FileLine, &c, &spec).await, Class::NoOp);
        let out = FileLine
            .remove(&c, &record("file.line", "k", &applied))
            .await
            .unwrap();
        assert!(!out.changed);
    }

    #[test]
    fn keys_are_unique_per_line() {
        assert_ne!(
            FileLine::key(&s(json!({"line": "a"}))),
            FileLine::key(&s(json!({"line": "b"})))
        );
    }

    #[tokio::test]
    async fn sshd_config_edits_are_validated_before_they_are_written() {
        let sys = FakeSystem::new()
            .with_file(
                "/etc/ssh/sshd_config",
                "PermitRootLogin yes\n",
                FilePerms::mode(0o644),
            )
            .on_command_fail(
                "sshd",
                &["-t"],
                255,
                "Bad configuration option: PermitRootLogn",
            );
        let c = cx(&sys);
        let s: Spec = spec(
            json!({"path": "/etc/ssh/sshd_config", "line": "PermitRootLogn no", "regex": "^PermitRootLogin"}),
        );
        assert!(FileLine.plan(&c, &s).await.is_err());
        assert!(FileLine.apply(&c, &s, &Planned::NoOp).await.is_err());
        assert_eq!(
            sys.file_string("/etc/ssh/sshd_config").unwrap(),
            "PermitRootLogin yes\n"
        );

        let sys = sys.on_command_ok("sshd", &["-t"], "");
        let c = cx(&sys);
        apply(&FileLine, &c, &s).await;
        assert_eq!(
            sys.file_string("/etc/ssh/sshd_config").unwrap(),
            "PermitRootLogn no\n"
        );
    }

    #[tokio::test]
    async fn a_value_change_updates_in_place_and_removal_restores_the_first_original() {
        let sys =
            FakeSystem::new().with_file("/etc/conf", "#Port 22\nother\n", FilePerms::default());
        let first: Spec = s(json!({"line": "Port 2222", "regex": "^#?Port "}));
        let second: Spec = s(json!({"line": "Port 2200", "regex": "^#?Port "}));
        assert_eq!(
            FileLine::key(&first),
            FileLine::key(&second),
            "keyed on the regex"
        );
        let a = apply(&FileLine, &cx(&sys), &first).await;
        let c = buckle_core::testing::CxBuilder::new(sys.clone())
            .previous(record("file.line", "k", &a))
            .build();
        let b = apply(&FileLine, &c, &second).await;
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "Port 2200\nother\n");
        FileLine
            .remove(&cx(&sys), &record("file.line", "k", &b))
            .await
            .unwrap();
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "#Port 22\nother\n");
    }

    #[tokio::test]
    async fn removing_an_added_line_deletes_one_occurrence_only() {
        let sys = FakeSystem::new().with_file("/etc/conf", "a\n", FilePerms::default());
        let added = apply(&FileLine, &cx(&sys), &s(json!({"line": "x"}))).await;
        // Another module's identical line appears later.
        put(&sys, "/etc/conf", "a\nx\nb\nx\n").await;
        FileLine
            .remove(&cx(&sys), &record("file.line", "k", &added))
            .await
            .unwrap();
        assert_eq!(sys.file_string("/etc/conf").unwrap(), "a\nx\nb\n");
    }

    #[tokio::test]
    async fn concurrent_edits_of_one_file_do_not_lose_lines() {
        let sys = FakeSystem::new().with_file("/etc/conf", "", FilePerms::default());
        let mut handles = Vec::new();
        for i in 0..8 {
            let c = cx(&sys);
            handles.push(tokio::spawn(async move {
                let spec: Spec = spec(json!({"path": "/etc/conf", "line": format!("line{i}")}));
                FileLine.apply(&c, &spec, &Planned::NoOp).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(sys.file_string("/etc/conf").unwrap().lines().count(), 8);
    }

    #[tokio::test]
    async fn sensitive_files_get_context_free_diffs_and_no_replaced_text_in_state() {
        let sys = FakeSystem::new().with_file(
            "/etc/environment",
            "PATH=/usr/bin\nAPI_TOKEN=hand-placed-token\nLANG=C\n",
            FilePerms::mode(0o644),
        );
        let s: Spec =
            spec(json!({"path": "/etc/environment", "line": "API_TOKEN=", "regex": "^API_TOKEN="}));
        let planned = FileLine.plan(&cx(&sys), &s).await.unwrap();
        let Diff::Update {
            unified: Some(u), ..
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert!(
            !u.get().contains("PATH=") && !u.get().contains("LANG="),
            "no context: {}",
            u.get()
        );
        let applied = apply(&FileLine, &cx(&sys), &s).await;
        let state = serde_json::to_string(&applied).unwrap();
        assert!(!state.contains("hand-placed-token"), "{state}");
        // Removal still restores it, from the 0600 stash outside state.
        FileLine
            .remove(&cx(&sys), &record("file.line", "k", &applied))
            .await
            .unwrap();
        assert_eq!(
            sys.file_string("/etc/environment").unwrap(),
            "PATH=/usr/bin\nAPI_TOKEN=hand-placed-token\nLANG=C\n"
        );
        assert!(
            sys.paths()
                .iter()
                .all(|p| !p.starts_with("/var/lib/buckle/stash/file.line/")
                    || p == std::path::Path::new("/var/lib/buckle/stash/file.line"))
        );
    }

    #[tokio::test]
    async fn diffs_are_scrubbed_of_known_secret_values() {
        let sys =
            FakeSystem::new().with_file("/etc/conf", "pw=s3cr3t-value\n", FilePerms::default());
        let c = cx_secrets(
            &sys,
            buckle_core::testing::MemorySource::new(),
            secrets("pw", "s3cr3t-value"),
        );
        let planned = FileLine
            .plan(&c, &s(json!({"line": "pw=other", "regex": "^pw="})))
            .await
            .unwrap();
        let json = serde_json::to_string(&planned).unwrap();
        assert!(
            !json.contains("s3cr3t-value") && json.contains("secret:pw@"),
            "{json}"
        );
    }
}
