//! `file.tokens`: ensure space-separated tokens on a single line (a kernel command line).

use super::hostkit;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::FilePerms;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::instrument;

/// Ensures tokens are present in (or absent from) a space-separated list on one line,
/// such as a kernel command line.
///
/// ```yaml
/// - file.tokens:
///     path: /boot/firmware/cmdline.txt
///     present: [cgroup_enable=cpuset, cgroup_enable=memory, cgroup_memory=1]
///     absent: [console=serial0]          # also matches console=serial0,115200
///
/// - file.tokens:                         # the quoted value of one assignment
///     path: /etc/default/grub
///     key: GRUB_CMDLINE_LINUX_DEFAULT
///     present: [apparmor=1, security=apparmor]
/// ```
///
/// Without `key`, the whole file is the list and must hold a single line (a
/// trailing newline is kept). With `key`, the list is the value of the last
/// `KEY=...` line (optionally `export`ed); its quotes are kept, and added when
/// the value grows past one token.
///
/// `present` tokens missing from the list are appended in order; tokens that
/// are already there stay where they are. `absent` deletes every token equal
/// to an entry, or starting with it followed by `=` or `,` (so `quiet` matches
/// `quiet`, `console` matches `console=tty1` and `console=serial0` matches
/// `console=serial0,115200`), except tokens listed in `present`.
///
/// Buckle records what it changed. Dropping a token from the task, or removing
/// the task, reverts it: appended tokens are deleted and deleted tokens are put
/// back where they were. A missing file (or `key` line) plans as blocked, so
/// the task can run on every host and changes only those that have the file.
/// Edits of one file are serialised across modules.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Absolute path of the file to edit.
    pub path: PathBuf,
    /// Edit the value of this `KEY="..."` assignment rather than the whole (single-line) file.
    #[serde(default)]
    pub key: Option<String>,
    /// Tokens to ensure, appended when missing.
    #[serde(default)]
    pub present: Vec<String>,
    /// Tokens to delete: an entry matches a token equal to it or followed by `=` or `,`.
    #[serde(default)]
    pub absent: Vec<String>,
    /// Keep a timestamped `.bak` copy before editing.
    #[serde(default)]
    pub backup: bool,
}

pub struct FileTokens;
buckle_core::register_task!(FileTokens);

/// What an apply changed, recorded so that the next plan and removal can revert it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Owned {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    /// Tokens Buckle appended.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    added: Vec<String>,
    /// Tokens `absent` deleted, with their index at the time (ascending).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed: Vec<Removed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Removed {
    index: usize,
    token: String,
}

impl Owned {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty()
    }
}

impl Spec {
    fn validate(&self) -> Result<()> {
        hostkit::require_absolute(&self.path, "path")?;
        if let Some(key) = &self.key
            && (key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            return Err(error::user(
                format!("`{key}` is not a variable name."),
                &["Give the name the file assigns, for example `key: GRUB_CMDLINE_LINUX`."],
            ));
        }
        for (field, tokens) in [("present", &self.present), ("absent", &self.absent)] {
            if let Some(bad) = tokens.iter().find(|t| {
                t.is_empty() || t.contains(char::is_whitespace) || t.contains(['"', '\''])
            }) {
                return Err(error::user(
                    format!("`{bad}` in `{field}` is not a single token."),
                    &["List each token on its own, without spaces or quotes."],
                ));
            }
        }
        Ok(())
    }
}

fn matches_entry(token: &str, entry: &str) -> bool {
    token == entry
        || token
            .strip_prefix(entry)
            .is_some_and(|rest| rest.starts_with(['=', ',']))
}

/// Applies `spec` to `tokens`. Pure, so the cases are tested without a host.
fn edit(mut tokens: Vec<String>, spec: &Spec) -> (Vec<String>, Owned) {
    let mut owned = Owned {
        key: spec.key.clone(),
        ..Owned::default()
    };
    for t in &spec.present {
        if !tokens.contains(t) {
            tokens.push(t.clone());
            owned.added.push(t.clone());
        }
    }
    let mut kept = Vec::with_capacity(tokens.len());
    for (index, token) in tokens.into_iter().enumerate() {
        if !spec.present.contains(&token) && spec.absent.iter().any(|e| matches_entry(&token, e)) {
            owned.removed.push(Removed { index, token });
        } else {
            kept.push(token);
        }
    }
    (kept, owned)
}

/// Undoes `owned`: the deleted tokens go back, then the appended ones come off.
fn revert(mut tokens: Vec<String>, owned: &Owned) -> Vec<String> {
    for r in &owned.removed {
        // Already back (put back by hand, or reverted before): not twice.
        if tokens.contains(&r.token) {
            continue;
        }
        let at = r.index.min(tokens.len());
        tokens.insert(at, r.token.clone());
    }
    for added in owned.added.iter().rev() {
        if let Some(i) = tokens.iter().rposition(|t| t == added) {
            tokens.remove(i);
        }
    }
    tokens
}

/// The token list inside a file and how to write it back.
#[derive(Debug, PartialEq, Eq)]
struct Located {
    tokens: Vec<String>,
    /// Text before the tokens on their line, and the quote around them.
    before: String,
    quote: Option<char>,
    after: String,
    /// The other lines, with the token line's index.
    lines: Vec<String>,
    index: usize,
    trailing_newline: bool,
}

impl Located {
    fn render(&self, tokens: &[String]) -> String {
        let joined = tokens.join(" ");
        let quote = self
            .quote
            .or_else(|| (self.before.ends_with('=') && tokens.len() > 1).then_some('"'));
        let line = match quote {
            Some(q) => format!("{}{q}{joined}{q}{}", self.before, self.after),
            None => format!("{}{joined}{}", self.before, self.after),
        };
        let mut lines = self.lines.clone();
        if self.index < lines.len() {
            lines[self.index] = line;
        } else {
            lines.push(line);
        }
        let mut text = lines.join("\n");
        if self.trailing_newline {
            text.push('\n');
        }
        text
    }
}

/// Finds the token list in `text`, or the reason it cannot be edited.
fn locate(text: &str, key: Option<&str>, path: &Path) -> std::result::Result<Located, String> {
    let lines: Vec<String> = hostkit::split_lines(text);
    let trailing_newline = text.is_empty() || text.ends_with('\n');
    let Some(key) = key else {
        let used: Vec<usize> = (0..lines.len())
            .filter(|&i| !lines[i].trim().is_empty())
            .collect();
        if used.len() > 1 {
            return Err(format!(
                "{} holds {} lines; without `key` the file must be a single line",
                path.display(),
                used.len()
            ));
        }
        let index = used.first().copied().unwrap_or(0);
        let line = lines.get(index).map(String::as_str).unwrap_or("");
        let start = line.len() - line.trim_start().len();
        let end = line.trim_end().len();
        return Ok(Located {
            tokens: line.split_whitespace().map(str::to_owned).collect(),
            before: line[..start].to_owned(),
            quote: None,
            after: line[end.max(start)..].to_owned(),
            lines,
            index,
            trailing_newline,
        });
    };
    let found = lines.iter().enumerate().rev().find_map(|(i, l)| {
        let t = l.trim_start();
        let t = t.strip_prefix("export ").map(str::trim_start).unwrap_or(t);
        t.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|value| (i, l.len() - value.len()))
    });
    let Some((index, value_at)) = found else {
        return Err(format!("{} does not set {key}", path.display()));
    };
    let line = &lines[index];
    let value = &line[value_at..];
    let (quote, inner, after) = match value.chars().next() {
        Some(q @ ('"' | '\'')) => match value[1..].find(q) {
            Some(close) => (Some(q), &value[1..1 + close], &value[2 + close..]),
            None => {
                return Err(format!(
                    "the value of {key} in {} has no closing quote",
                    path.display()
                ));
            }
        },
        _ => {
            let end = value.find(char::is_whitespace).unwrap_or(value.len());
            (None, &value[..end], &value[end..])
        }
    };
    Ok(Located {
        tokens: inner.split_whitespace().map(str::to_owned).collect(),
        before: line[..value_at].to_owned(),
        quote,
        after: after.to_owned(),
        lines: lines.clone(),
        index,
        trailing_newline,
    })
}

fn previous_owned(cx: &Cx, path: &Path) -> Option<Owned> {
    hostkit::ownership::previous_artefacts(cx)
        .iter()
        .find_map(|a| match a {
            Artefact::Line { path: p, marker } if p == path => serde_json::from_str(marker).ok(),
            _ => None,
        })
}

fn owned_artefact(record: &ResourceRecord) -> Option<(PathBuf, Owned)> {
    record.artefacts.iter().find_map(|a| match a {
        Artefact::Line { path, marker } => {
            serde_json::from_str(marker).ok().map(|o| (path.clone(), o))
        }
        _ => None,
    })
}

fn where_(key: Option<&str>, path: &Path) -> String {
    match key {
        Some(k) => format!("{k} in {}", path.display()),
        None => path.display().to_string(),
    }
}

enum Outcome {
    NoOp(Owned),
    Blocked(String),
    Change {
        diff: Diff,
        new_text: String,
        owned: Owned,
        existing: hostkit::TextFile,
    },
}

impl FileTokens {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Outcome> {
        spec.validate()?;
        let Some(file) = hostkit::load_text(cx, &spec.path).await? else {
            return Ok(Outcome::Blocked(format!(
                "{} does not exist",
                spec.path.display()
            )));
        };
        let located = match locate(&file.text, spec.key.as_deref(), &spec.path) {
            Ok(l) => l,
            Err(reason) => return Ok(Outcome::Blocked(reason)),
        };
        let baseline = match previous_owned(cx, &spec.path).filter(|o| o.key == spec.key) {
            Some(owned) => revert(located.tokens.clone(), &owned),
            None => located.tokens.clone(),
        };
        let (tokens, owned) = edit(baseline, spec);
        if tokens == located.tokens {
            return Ok(Outcome::NoOp(owned));
        }
        let new_text = located.render(&tokens);
        let mut changes = Vec::new();
        let added: Vec<&String> = tokens
            .iter()
            .filter(|t| !located.tokens.contains(t))
            .collect();
        let dropped: Vec<&String> = located
            .tokens
            .iter()
            .filter(|t| !tokens.contains(t))
            .collect();
        if !added.is_empty() {
            changes.push(format!("add {}", join(&added)));
        }
        if !dropped.is_empty() {
            changes.push(format!("remove {}", join(&dropped)));
        }
        if changes.is_empty() {
            changes.push("reorder tokens".to_owned());
        }
        let summary = format!(
            "{} in {}",
            changes.join(", "),
            where_(spec.key.as_deref(), &spec.path)
        );
        Ok(Outcome::Change {
            diff: hostkit::text_diff(cx, &spec.path, summary, &file.text, &new_text),
            new_text,
            owned,
            existing: file,
        })
    }

    fn applied(spec: &Spec, applied: Applied, owned: &Owned) -> Result<Applied> {
        if owned.is_empty() {
            return Ok(applied);
        }
        let marker = serde_json::to_string(owned).map_err(|e| {
            error::wrap_system(
                e,
                "Could not record the file.tokens edit.",
                &["This is a bug in Buckle; please report it."],
            )
        })?;
        Ok(applied.with_artefact(Artefact::Line {
            path: spec.path.clone(),
            marker,
        }))
    }
}

fn join(tokens: &[&String]) -> String {
    tokens
        .iter()
        .map(|t| format!("`{t}`"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait]
impl Task for FileTokens {
    type Spec = Spec;
    const KIND: &'static str = "file.tokens";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Ensure tokens in a space-separated list on one line (cmdline.txt, GRUB_CMDLINE_LINUX).";

    fn key(spec: &Spec) -> String {
        match &spec.key {
            Some(k) => format!("{}: {k}", spec.path.display()),
            None => spec.path.display().to_string(),
        }
    }

    fn describe(spec: &Spec) -> String {
        let mut parts = Vec::new();
        if !spec.present.is_empty() {
            parts.push(format!("{} present", spec.present.len()));
        }
        if !spec.absent.is_empty() {
            parts.push(format!("{} absent", spec.absent.len()));
        }
        format!(
            "tokens in {} ({})",
            where_(spec.key.as_deref(), &spec.path),
            parts.join(", ")
        )
    }

    #[instrument(name = "task.file.tokens.plan", skip_all, fields(path = %spec.path.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(match self.inspect(cx, spec).await? {
            Outcome::NoOp(_) => Planned::NoOp,
            Outcome::Blocked(reason) => Planned::blocked(reason),
            Outcome::Change { diff, .. } => Planned::change(diff),
        })
    }

    #[instrument(name = "task.file.tokens.apply", skip_all, fields(path = %spec.path.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let _guard = hostkit::lock_path(&spec.path).await;
        match self.inspect(cx, spec).await? {
            Outcome::NoOp(owned) => Self::applied(
                spec,
                Applied::unchanged(format!(
                    "{} already up to date",
                    where_(spec.key.as_deref(), &spec.path)
                )),
                &owned,
            ),
            Outcome::Blocked(reason) => Err(error::user(
                format!("Cannot edit {}: {reason}.", spec.path.display()),
                &["Create the file in an earlier task, or limit the task with `when`."],
            )),
            Outcome::Change {
                diff,
                new_text,
                owned,
                existing,
            } => {
                hostkit::save_text(
                    cx,
                    &spec.path,
                    &new_text,
                    Some(&existing),
                    spec.backup,
                    &FilePerms::default(),
                )
                .await?;
                Self::applied(spec, Applied::changed(diff.summary().to_owned()), &owned)
            }
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        match owned_artefact(record) {
            Some((path, owned)) => {
                let mut parts = Vec::new();
                if !owned.added.is_empty() {
                    parts.push(format!(
                        "remove {}",
                        join(&owned.added.iter().collect::<Vec<_>>())
                    ));
                }
                if !owned.removed.is_empty() {
                    let back: Vec<&String> = owned.removed.iter().map(|r| &r.token).collect();
                    parts.push(format!("put back {}", join(&back)));
                }
                format!(
                    "{} in {}",
                    parts.join(", "),
                    where_(owned.key.as_deref(), &path)
                )
            }
            None => format!("nothing to revert for {}", record.key.key),
        }
    }

    #[instrument(name = "task.file.tokens.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some((path, owned)) = owned_artefact(record) else {
            return Ok(Applied::skipped("no edit was recorded to revert"));
        };
        let _guard = hostkit::lock_path(&path).await;
        let Some(file) = hostkit::load_text(cx, &path).await? else {
            return Ok(Applied::unchanged(format!(
                "{} no longer exists",
                path.display()
            )));
        };
        let located = match locate(&file.text, owned.key.as_deref(), &path) {
            Ok(l) => l,
            Err(reason) => return Ok(Applied::skipped(format!("{reason}, left as it is"))),
        };
        let tokens = revert(located.tokens.clone(), &owned);
        if tokens == located.tokens {
            return Ok(Applied::unchanged(format!(
                "nothing of Buckle's edit is left in {}",
                where_(owned.key.as_deref(), &path)
            )));
        }
        hostkit::save_text(
            cx,
            &path,
            &located.render(&tokens),
            Some(&file),
            false,
            &FilePerms::default(),
        )
        .await?;
        Ok(Applied::changed(format!(
            "reverted the edit of {}",
            where_(owned.key.as_deref(), &path)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::{Class, apply, cx, plan_class, put, record, spec};
    use buckle_core::testing::{CxBuilder, FakeSystem};
    use rstest::rstest;
    use serde_json::json;

    const CMDLINE: &str = "/boot/firmware/cmdline.txt";
    const PI: &str = "console=serial0,115200 console=tty1 root=PARTUUID=1234-02 rootfstype=ext4 fsck.repair=yes rootwait quiet\n";

    fn s(extra: serde_json::Value) -> Spec {
        let mut v = json!({"path": CMDLINE});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(v)
    }

    fn run(extra: serde_json::Value, before: &str) -> String {
        let spec = s(extra);
        let located = locate(before, spec.key.as_deref(), &spec.path).unwrap();
        let (tokens, _) = edit(located.tokens.clone(), &spec);
        located.render(&tokens)
    }

    #[rstest]
    #[case::append_missing(json!({"present": ["b", "c"]}), "a b\n", "a b c\n")]
    #[case::present_is_unchanged(json!({"present": ["a"]}), "a b\n", "a b\n")]
    #[case::absent_by_prefix(json!({"absent": ["console=serial0", "quiet"]}), "console=serial0,115200 console=tty1 quiet\n", "console=tty1\n")]
    #[case::absent_by_key(json!({"absent": ["console"]}), "console=serial0,115200 console=tty1 rootwait\n", "rootwait\n")]
    #[case::absent_is_not_a_bare_prefix(json!({"absent": ["root"]}), "root=/dev/sda rootwait\n", "rootwait\n")]
    #[case::present_beats_absent(json!({"present": ["cgroup_memory=1"], "absent": ["cgroup_memory"]}), "cgroup_memory=0 x\n", "x cgroup_memory=1\n")]
    #[case::no_trailing_newline_kept(json!({"present": ["b"]}), "a", "a b")]
    #[case::empty_file(json!({"present": ["a"]}), "", "a\n")]
    #[case::grub_quoted(json!({"key": "GRUB_CMDLINE_LINUX_DEFAULT", "present": ["apparmor=1"]}), "GRUB_DEFAULT=0\nGRUB_CMDLINE_LINUX_DEFAULT=\"quiet splash\"\nGRUB_CMDLINE_LINUX=\"\"\n", "GRUB_DEFAULT=0\nGRUB_CMDLINE_LINUX_DEFAULT=\"quiet splash apparmor=1\"\nGRUB_CMDLINE_LINUX=\"\"\n")]
    #[case::grub_single_quotes(json!({"key": "K", "absent": ["quiet"]}), "K='quiet splash' # note\n", "K='splash' # note\n")]
    #[case::unquoted_value_gains_quotes(json!({"key": "K", "present": ["b"]}), "export K=a\n", "export K=\"a b\"\n")]
    #[case::last_assignment_wins(json!({"key": "K", "present": ["z"]}), "K=\"a\"\nK=\"b\"\n", "K=\"a\"\nK=\"b z\"\n")]
    fn edit_cases(#[case] extra: serde_json::Value, #[case] before: &str, #[case] after: &str) {
        assert_eq!(run(extra, before), after);
    }

    #[rstest]
    #[case::relative_path(json!({"path": "cmdline.txt"}))]
    #[case::token_with_space(json!({"present": ["a b"]}))]
    #[case::empty_token(json!({"absent": [""]}))]
    #[case::quoted_token(json!({"present": ["\"a\""]}))]
    #[case::bad_key(json!({"key": "GRUB CMDLINE"}))]
    fn misconfiguration_is_a_user_error(#[case] extra: serde_json::Value) {
        assert!(s(extra).validate().is_err());
    }

    fn cx_after(sys: &FakeSystem, applied: &Applied) -> Cx {
        CxBuilder::new(sys.clone())
            .previous(record("file.tokens", "k", applied))
            .build()
    }

    #[tokio::test]
    async fn applies_idempotently_and_reverts_on_removal() {
        let sys = FakeSystem::new().with_file(CMDLINE, PI, FilePerms::mode(0o755));
        let spec = s(json!({
            "present": ["cgroup_enable=cpuset", "cgroup_enable=memory", "cgroup_memory=1"],
            "absent": ["console=serial0"],
        }));
        let planned = FileTokens.plan(&cx(&sys), &spec).await.unwrap();
        let summary = crate::tasks::hostkit::testkit::diff(&planned)
            .summary()
            .to_owned();
        assert!(
            summary.contains("`cgroup_memory=1`")
                && summary.contains("remove `console=serial0,115200`"),
            "{summary}"
        );
        let applied = apply(&FileTokens, &cx(&sys), &spec).await;
        assert_eq!(
            sys.file_string(CMDLINE).unwrap(),
            "console=tty1 root=PARTUUID=1234-02 rootfstype=ext4 fsck.repair=yes rootwait quiet cgroup_enable=cpuset cgroup_enable=memory cgroup_memory=1\n"
        );
        assert_eq!(
            plan_class(&FileTokens, &cx_after(&sys, &applied), &spec).await,
            Class::NoOp
        );

        let rec = record("file.tokens", "k", &applied);
        assert!(
            FileTokens
                .describe_removal(&rec)
                .contains("put back `console=serial0,115200`")
        );
        assert!(FileTokens.remove(&cx(&sys), &rec).await.unwrap().changed);
        assert_eq!(sys.file_string(CMDLINE).unwrap(), PI);
        assert!(!FileTokens.remove(&cx(&sys), &rec).await.unwrap().changed);
    }

    #[tokio::test]
    async fn dropping_a_token_from_the_task_reverts_it() {
        let sys = FakeSystem::new().with_file(CMDLINE, "a quiet\n", FilePerms::default());
        let first = s(json!({"present": ["b", "c"], "absent": ["quiet"]}));
        let applied = apply(&FileTokens, &cx(&sys), &first).await;
        assert_eq!(sys.file_string(CMDLINE).unwrap(), "a b c\n");

        let second = s(json!({"present": ["c"]}));
        let applied = apply(&FileTokens, &cx_after(&sys, &applied), &second).await;
        assert_eq!(sys.file_string(CMDLINE).unwrap(), "a quiet c\n");
        assert_eq!(
            plan_class(&FileTokens, &cx_after(&sys, &applied), &second).await,
            Class::NoOp
        );
    }

    #[tokio::test]
    async fn tokens_already_there_are_not_owned() {
        let sys = FakeSystem::new().with_file(CMDLINE, "a rootwait\n", FilePerms::default());
        let applied = apply(
            &FileTokens,
            &cx(&sys),
            &s(json!({"present": ["rootwait", "b"]})),
        )
        .await;
        FileTokens
            .remove(&cx(&sys), &record("file.tokens", "k", &applied))
            .await
            .unwrap();
        assert_eq!(sys.file_string(CMDLINE).unwrap(), "a rootwait\n");
    }

    #[rstest]
    #[case::missing_file(None)]
    #[case::several_lines(Some("a\nb\n"))]
    #[tokio::test]
    async fn unusable_files_block(#[case] content: Option<&str>) {
        let sys = FakeSystem::new();
        if let Some(c) = content {
            put(&sys, CMDLINE, c).await;
        }
        let spec = s(json!({"present": ["x"]}));
        assert_eq!(
            plan_class(&FileTokens, &cx(&sys), &spec).await,
            Class::Blocked
        );
        assert!(
            FileTokens
                .apply(&cx(&sys), &spec, &Planned::NoOp)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_missing_key_blocks() {
        let sys = FakeSystem::new().with_file(
            "/etc/default/grub",
            "GRUB_DEFAULT=0\n",
            FilePerms::default(),
        );
        let spec =
            s(json!({"path": "/etc/default/grub", "key": "GRUB_CMDLINE_LINUX", "present": ["x"]}));
        assert_eq!(
            plan_class(&FileTokens, &cx(&sys), &spec).await,
            Class::Blocked
        );
    }
}
