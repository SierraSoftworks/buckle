//! `file.block`: manage a marked block of lines inside a text file.

use super::hostkit::{self, Ensure};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::FilePerms;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::instrument;

/// Manages a block of lines between marker comments, like Ansible's `blockinfile`.
///
/// ```yaml
/// - file.block:
///     path: /etc/hosts
///     marker: "# {mark} nomad cluster"      # {mark} becomes BEGIN / END
///     block: |
///       10.0.0.1 nomad-1
///       10.0.0.2 nomad-2
///     insert_after: '^127\.0\.0\.1'          # optional; default is the end of the file
/// ```
///
/// The block, including its `BEGIN` and `END` marker lines, is inserted when
/// missing and rewritten in place when its content differs. `insert_after` and
/// `insert_before` take a pattern (the last match wins) or `EOF` / `BOF` and
/// only matter when the block is first inserted. With `state: absent` the
/// block and its markers are deleted. The file must exist unless
/// `create: true`; mode and ownership are preserved; `backup: true` keeps a
/// timestamped copy before each edit.
///
/// The marker lines delimit what Buckle owns, so removal deletes everything
/// between them, and a file Buckle created is deleted if that leaves it empty.
/// Two blocks in one file need different `marker`s. Edits of one file are
/// serialised across modules.
///
/// `validate` is a command (argv) that checks the new content before it
/// replaces the file (and while planning); `{path}` stands for a private
/// temporary copy under `<state_dir>/tmp`, deleted afterwards. A non-zero
/// exit fails the task and leaves the file untouched. Without `validate`,
/// `/etc/sudoers`, `/etc/sudoers.d/*` and `sshd_config` files are checked with
/// `visudo -cf {path}` and `sshd -t -f {path}` (when those tools are
/// installed); `validate: []` turns that off.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Absolute path of the file to edit.
    pub path: PathBuf,
    /// The lines of the block (may be empty, which leaves just the markers).
    #[serde(default)]
    pub block: String,
    /// Marker line template containing `{mark}`.
    #[serde(default = "default_marker")]
    pub marker: String,
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

fn default_marker() -> String {
    "# {mark} BUCKLE MANAGED BLOCK".to_owned()
}

pub struct FileBlock;
buckle_core::register_task!(FileBlock);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    marker: String,
    created: bool,
}

fn markers(marker: &str) -> Result<(String, String)> {
    if !marker.contains("{mark}") {
        return Err(error::user(
            format!("The marker '{marker}' does not contain the `{{mark}}` placeholder."),
            &["Use a marker such as \"# {mark} MANAGED BLOCK\"; {mark} becomes BEGIN and END."],
        ));
    }
    Ok((
        marker.replace("{mark}", "BEGIN"),
        marker.replace("{mark}", "END"),
    ))
}

/// Finds the inclusive line range of the block.
fn find_block(lines: &[String], begin: &str, end: &str) -> Result<Option<(usize, usize)>> {
    let Some(b) = lines.iter().position(|l| l == begin) else {
        return Ok(None);
    };
    let Some(e) = lines[b + 1..].iter().position(|l| l == end) else {
        return Err(error::user(
            format!("Found the block marker '{begin}' without a matching '{end}'."),
            &[
                "Fix or remove the dangling marker by hand; Buckle will not guess where the block ends.",
            ],
        ));
    };
    Ok(Some((b, b + 1 + e)))
}

#[derive(Debug)]
enum Edit {
    Unchanged,
    Changed { lines: Vec<String>, summary: String },
}

fn edit(spec: &Spec, current: &str) -> Result<Edit> {
    let (begin, end) = markers(&spec.marker)?;
    let mut lines = hostkit::split_lines(current);
    let found = find_block(&lines, &begin, &end)?;
    match spec.state {
        Ensure::Absent => Ok(match found {
            None => Edit::Unchanged,
            Some((b, e)) => {
                lines.drain(b..=e);
                Edit::Changed {
                    lines,
                    summary: format!("remove the block from {}", spec.path.display()),
                }
            }
        }),
        Ensure::Present => {
            let mut wanted = vec![begin];
            wanted.extend(hostkit::split_lines(&spec.block));
            wanted.push(end);
            match found {
                Some((b, e)) if lines[b..=e] == wanted[..] => Ok(Edit::Unchanged),
                Some((b, e)) => {
                    lines.splice(b..=e, wanted);
                    Ok(Edit::Changed {
                        lines,
                        summary: format!("update the block in {}", spec.path.display()),
                    })
                }
                None => {
                    let at = hostkit::insertion_index(
                        &lines,
                        spec.insert_after.as_deref(),
                        spec.insert_before.as_deref(),
                    )?;
                    lines.splice(at..at, wanted);
                    Ok(Edit::Changed {
                        lines,
                        summary: format!("insert the block into {}", spec.path.display()),
                    })
                }
            }
        }
    }
}

enum Outcome {
    NoOp,
    Blocked(String),
    Change {
        diff: Diff,
        new_text: String,
        created: bool,
        existing: Option<hostkit::TextFile>,
    },
}

impl FileBlock {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Outcome> {
        hostkit::require_absolute(&spec.path, "path")?;
        let existing = hostkit::load_text(cx, &spec.path).await?;
        let current = existing.as_ref().map(|f| f.text.as_str());
        if current.is_none() {
            if spec.state == Ensure::Absent {
                markers(&spec.marker)?;
                return Ok(Outcome::NoOp);
            }
            if !spec.create {
                return Ok(Outcome::Blocked(format!(
                    "{} does not exist; set `create: true` to create it",
                    spec.path.display()
                )));
            }
        }
        Ok(match edit(spec, current.unwrap_or(""))? {
            Edit::Unchanged => Outcome::NoOp,
            Edit::Changed { lines, summary } => {
                let new_text = hostkit::join_lines(&lines);
                let diff = match &existing {
                    Some(file) => {
                        hostkit::text_diff(cx, &spec.path, summary, &file.text, &new_text)
                    }
                    None => Diff::Create {
                        summary: format!("create {} with the block", spec.path.display()),
                        preview: Some(buckle_core::secrets::Redacted::plain(new_text.clone())),
                    },
                };
                Outcome::Change {
                    diff,
                    new_text,
                    created: existing.is_none(),
                    existing,
                }
            }
        })
    }
}

#[async_trait]
impl Task for FileBlock {
    type Spec = Spec;
    const KIND: &'static str = "file.block";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Manage a marked block of lines inside a text file (blockinfile).";

    fn key(spec: &Spec) -> String {
        format!(
            "{}#{}",
            spec.path.display(),
            spec.marker.replace("{mark}", "BEGIN").trim()
        )
    }

    fn describe(spec: &Spec) -> String {
        match spec.state {
            Ensure::Present => format!("managed block in {}", spec.path.display()),
            Ensure::Absent => format!("no managed block in {}", spec.path.display()),
        }
    }

    #[instrument(name = "task.file.block.plan", skip_all, fields(path = %spec.path.display(), state = ?spec.state))]
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

    #[instrument(name = "task.file.block.apply", skip_all, fields(path = %spec.path.display(), state = ?spec.state))]
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
                created,
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
                if spec.state == Ensure::Present {
                    // A file Buckle created on an earlier apply stays Buckle's.
                    let created_before =
                        hostkit::ownership::previous_artefacts(cx).iter().any(|a| {
                            matches!(a, Artefact::Line { path, marker }
                            if *path == spec.path
                                && serde_json::from_str::<Marker>(marker).is_ok_and(|m| m.created))
                        });
                    let marker = serde_json::to_string(&Marker {
                        marker: spec.marker.clone(),
                        created: created || created_before,
                    })
                    .map_err(|e| {
                        error::wrap_system(
                            e,
                            "Could not record the block edit.",
                            &["This is a bug in Buckle; please report it."],
                        )
                    })?;
                    applied = applied.with_artefact(Artefact::Line {
                        path: spec.path.clone(),
                        marker,
                    });
                }
                Ok(applied)
            }
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        match block_artefact(record) {
            Some((path, _)) => format!("delete the managed block from {}", path.display()),
            None => format!("nothing to revert for {}", record.key.key),
        }
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some((path, marker)) = block_artefact(record) else {
            return Ok(Applied::skipped("no block was recorded to remove"));
        };
        let _guard = hostkit::lock_path(&path).await;
        let Some(file) = hostkit::load_text(cx, &path).await? else {
            return Ok(Applied::unchanged(format!(
                "{} no longer exists",
                path.display()
            )));
        };
        let (begin, end) = markers(&marker.marker)?;
        let mut lines = hostkit::split_lines(&file.text);
        let Some((b, e)) = find_block(&lines, &begin, &end)? else {
            return Ok(Applied::skipped(format!(
                "the block is no longer in {}, nothing to revert",
                path.display()
            )));
        };
        lines.drain(b..=e);
        if marker.created && lines.iter().all(|l| l.trim().is_empty()) {
            cx.system.remove(&path, false).await?;
            return Ok(Applied::changed(format!(
                "deleted {}, which Buckle created",
                path.display()
            )));
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
        Ok(Applied::changed(format!(
            "removed the block from {}",
            path.display()
        )))
    }
}

fn block_artefact(record: &ResourceRecord) -> Option<(PathBuf, Marker)> {
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
        let mut raw = json!({"path": "/etc/hosts", "block": "10.0.0.1 a\n10.0.0.2 b\n"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    const BLOCK: &str =
        "# BEGIN BUCKLE MANAGED BLOCK\n10.0.0.1 a\n10.0.0.2 b\n# END BUCKLE MANAGED BLOCK\n";

    #[rstest]
    #[case::appended(json!({}), "127.0.0.1 localhost\n", &format!("127.0.0.1 localhost\n{BLOCK}"))]
    #[case::unchanged(json!({}), &format!("x\n{BLOCK}y\n"), &format!("x\n{BLOCK}y\n"))]
    #[case::updated_in_place(json!({}), "x\n# BEGIN BUCKLE MANAGED BLOCK\nold\n# END BUCKLE MANAGED BLOCK\ny\n", &format!("x\n{BLOCK}y\n"))]
    #[case::insert_after(json!({"insert_after": "^127"}), "127.0.0.1 localhost\n::1 ip6\n", &format!("127.0.0.1 localhost\n{BLOCK}::1 ip6\n"))]
    #[case::insert_before(json!({"insert_before": "^::1"}), "127.0.0.1 localhost\n::1 ip6\n", &format!("127.0.0.1 localhost\n{BLOCK}::1 ip6\n"))]
    #[case::custom_marker(json!({"marker": "## {mark} nomad"}), "", "## BEGIN nomad\n10.0.0.1 a\n10.0.0.2 b\n## END nomad\n")]
    #[case::absent_removes(json!({"state": "absent"}), &format!("x\n{BLOCK}y\n"), "x\ny\n")]
    #[case::absent_when_missing(json!({"state": "absent"}), "x\n", "x\n")]
    fn edit_cases(#[case] extra: serde_json::Value, #[case] before: &str, #[case] after: &str) {
        match edit(&s(extra), before).unwrap() {
            Edit::Unchanged => assert_eq!(before, after),
            Edit::Changed { lines, .. } => assert_eq!(hostkit::join_lines(&lines), after),
        }
    }

    #[rstest]
    #[case::marker_without_placeholder(json!({"marker": "# fixed"}), "x\n")]
    #[case::dangling_begin(json!({}), "# BEGIN BUCKLE MANAGED BLOCK\nno end\n")]
    fn misconfiguration_is_a_user_error(#[case] extra: serde_json::Value, #[case] text: &str) {
        assert!(
            edit(&s(extra), text)
                .unwrap_err()
                .is(buckle_core::error::Kind::User)
        );
    }

    #[tokio::test]
    async fn create_update_and_remove_a_block_on_the_host() {
        let sys = FakeSystem::new();
        let c = cx(&sys);

        // Missing file: blocked without `create`, created with it.
        assert_eq!(
            plan_class(&FileBlock, &c, &s(json!({}))).await,
            Class::Blocked
        );
        let create = s(json!({"create": true}));
        let rec = record("file.block", "k", &apply(&FileBlock, &c, &create).await);
        assert_eq!(sys.file_string("/etc/hosts").unwrap(), BLOCK);
        assert_eq!(plan_class(&FileBlock, &c, &create).await, Class::NoOp);

        // Update: new content replaces the old block in place.
        put(&sys, "/etc/hosts", &format!("top\n{BLOCK}bottom\n")).await;
        let update = s(json!({"block": "10.0.0.9 z"}));
        let planned = FileBlock.plan(&c, &update).await.unwrap();
        let Diff::Update {
            unified: Some(u), ..
        } = diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert!(u.get().contains("-10.0.0.1 a") && u.get().contains("+10.0.0.9 z"));
        FileBlock.apply(&c, &update, &planned).await.unwrap();
        assert_eq!(
            sys.file_string("/etc/hosts").unwrap(),
            "top\n# BEGIN BUCKLE MANAGED BLOCK\n10.0.0.9 z\n# END BUCKLE MANAGED BLOCK\nbottom\n"
        );
        assert_eq!(plan_class(&FileBlock, &c, &update).await, Class::NoOp);

        // Removal deletes the block (and the file we created, once it is empty).
        put(&sys, "/etc/hosts", BLOCK).await;
        assert!(FileBlock.describe_removal(&rec).contains("/etc/hosts"));
        assert!(FileBlock.remove(&c, &rec).await.unwrap().changed);
        assert!(
            !sys.exists("/etc/hosts"),
            "the file Buckle created is deleted when emptied"
        );
    }

    #[tokio::test]
    async fn removal_keeps_the_rest_of_a_pre_existing_file() {
        let sys =
            FakeSystem::new().with_file("/etc/hosts", "top\nbottom\n", FilePerms::mode(0o640));
        let c = cx(&sys);
        let block = s(json!({}));
        let rec = record("file.block", "k", &apply(&FileBlock, &c, &block).await);
        assert!(FileBlock.remove(&c, &rec).await.unwrap().changed);
        assert_eq!(sys.file_string("/etc/hosts").unwrap(), "top\nbottom\n");
        // Already gone: harmless.
        assert!(!FileBlock.remove(&c, &rec).await.unwrap().changed);
    }

    #[tokio::test]
    async fn absent_state_removes_the_block_and_records_nothing() {
        let sys =
            FakeSystem::new().with_file("/etc/hosts", format!("a\n{BLOCK}"), FilePerms::default());
        let c = cx(&sys);
        let absent = s(json!({"state": "absent"}));
        let applied = apply(&FileBlock, &c, &absent).await;
        assert!(applied.artefacts.is_empty());
        assert_eq!(sys.file_string("/etc/hosts").unwrap(), "a\n");
        assert_eq!(plan_class(&FileBlock, &c, &absent).await, Class::NoOp);
    }

    #[test]
    fn different_markers_are_different_resources() {
        assert_ne!(
            FileBlock::key(&s(json!({"marker": "# {mark} one"}))),
            FileBlock::key(&s(json!({"marker": "# {mark} two"})))
        );
    }

    #[tokio::test]
    async fn an_explicit_validator_checks_the_edited_file() {
        let sys = FakeSystem::new()
            .with_file("/etc/app.conf", "a\n", FilePerms::default())
            .on_command_fail("appcheck", &[], 1, "bad block");
        let c = cx(&sys);
        let s: Spec = spec(
            json!({"path": "/etc/app.conf", "block": "b", "validate": ["appcheck", "{path}"]}),
        );
        assert!(
            FileBlock
                .plan(&c, &s)
                .await
                .unwrap_err()
                .description()
                .contains("bad block")
        );
        assert_eq!(sys.file_string("/etc/app.conf").unwrap(), "a\n");
    }
}
