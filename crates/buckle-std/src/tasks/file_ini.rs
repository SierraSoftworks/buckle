//! `file.ini`: own settings and lines in one section of an INI-style file.

use super::hostkit;
use super::sys_exec::Scalar;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::Redacted;
use buckle_core::state::ResourceRecord;
use buckle_core::system::FilePerms;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use tracing::instrument;

/// Owns settings and lines in one `[section]` of an INI-style file, leaving the rest of it alone.
///
/// ```yaml
/// - file.ini:
///     path: /boot/firmware/config.txt
///     section: all
///     settings:                        # one value per key: replaced in place
///       enable_uart: 1
///       dtparam=poe_fan_temp0: 50000   # a key may itself hold `=`
///     lines:                           # repeatable keys, kept as written
///       - dtoverlay=rpi-poe-plus
///       - dtoverlay=pps-gpio,gpiopin=18
///     absent: [dtoverlay=vc4-kms-v3d]
/// ```
///
/// `section` names the `[section]` header to edit (exactly as written, without
/// brackets); without it the task edits the lines before the first header.
/// A missing section is appended to the end of the file.
///
/// Each `settings` entry replaces the **last** line in the section that starts
/// with `key=` (whitespace around the separator is ignored), or adds
/// `key=value` when there is none. Each of `lines` is added unless an
/// identical line is already in the section. Lines that are already right are
/// adopted as they are. New lines are added together, settings first, after
/// the section's last line; later edits keep them where they are.
///
/// `absent` deletes lines from the section: an entry matches a line equal to
/// it, or one that starts with it followed by `=` or `,` (so `enable_uart`
/// matches `enable_uart=1` and `dtoverlay=vc4-kms-v3d` matches
/// `dtoverlay=vc4-kms-v3d,cma-512`). Lines the task itself wants are kept.
/// Comments (`#`, `;`) are never matched.
///
/// Buckle records what it changed. Dropping an entry from the task, or
/// removing the task, reverts it: added lines (and a section header Buckle
/// added) are deleted, replaced lines get their original text back and lines
/// deleted by `absent` are put back. Lines changed by hand since are left
/// alone.
///
/// The file must exist unless `create: true`, so a task for a file that only
/// some hosts have plans as blocked on the others. Mode and ownership are
/// preserved, and edits of one file are serialised across modules.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Absolute path of the file to edit.
    pub path: PathBuf,
    /// The section to edit (`all` for `[all]`); omit for the lines before the first header.
    #[serde(default)]
    pub section: Option<String>,
    /// Keys with a single value: `key: value` replaces the line starting with `key=` or adds one.
    #[serde(default)]
    pub settings: BTreeMap<String, Scalar>,
    /// Lines to ensure as written, for keys that may repeat (`dtoverlay=rpi-poe-plus`).
    #[serde(default)]
    pub lines: Vec<String>,
    /// Lines to delete: an entry matches a line equal to it or followed by `=` or `,`.
    #[serde(default)]
    pub absent: Vec<String>,
    /// What to write between a setting's key and value; `=` by default, ` = ` for spaced files.
    #[serde(default = "default_separator")]
    pub separator: String,
    /// Create the file when it does not exist.
    #[serde(default)]
    pub create: bool,
    /// Keep a timestamped `.bak` copy before editing.
    #[serde(default)]
    pub backup: bool,
}

fn default_separator() -> String {
    "=".to_owned()
}

pub struct FileIni;
buckle_core::register_task!(FileIni);

/// What an apply changed, recorded so that the next plan and removal can revert it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Owned {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    section: Option<String>,
    /// The separator the lines were matched with.
    sep: String,
    /// Buckle created the file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    created: bool,
    /// Buckle added the section header.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    header: bool,
    /// Lines Buckle added, in order (one contiguous group).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    added: Vec<String>,
    /// Lines Buckle rewrote in place, with the text they had before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    replaced: Vec<Replaced>,
    /// Lines `absent` deleted, with their index at the time (ascending).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    removed: Vec<Removed>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Replaced {
    now: String,
    original: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Removed {
    index: usize,
    text: String,
}

impl Owned {
    fn is_empty(&self) -> bool {
        !self.created
            && !self.header
            && self.added.is_empty()
            && self.replaced.is_empty()
            && self.removed.is_empty()
    }
}

impl Spec {
    fn section(&self) -> Option<&str> {
        self.section.as_deref().map(section_name)
    }

    /// The separator used for matching: the given one without its padding.
    fn sep(&self) -> Result<&str> {
        let sep = self.separator.trim();
        if sep.is_empty() {
            return Err(error::user(
                "The `separator` of a file.ini task is blank.",
                &["Use `=` (the default), `:` or a padded form such as ` = `."],
            ));
        }
        Ok(sep)
    }

    fn rendered_settings(&self) -> Vec<(&str, String)> {
        self.settings
            .iter()
            .map(|(k, v)| (k.as_str(), format!("{k}{}{}", self.separator, v.text())))
            .collect()
    }

    fn validate(&self) -> Result<()> {
        hostkit::require_absolute(&self.path, "path")?;
        if hostkit::is_sensitive_path(&self.path) {
            return Err(error::user(
                format!(
                    "file.ini does not edit {}, which may hold credentials.",
                    self.path.display()
                ),
                &["Use file.line, which keeps replaced lines out of the state it reports."],
            ));
        }
        let sep = self.sep()?;
        let single = |field: &str, text: &str| -> Result<()> {
            if text.contains('\n') || text.contains('\r') {
                return Err(error::user(
                    format!("An entry of `{field}` in a file.ini task spans several lines."),
                    &["Give each line as its own entry."],
                ));
            }
            if header(text).is_some() {
                return Err(error::user(
                    format!("The `{field}` entry `{text}` is a section header."),
                    &["Use `section` to choose the section, and one task per section."],
                ));
            }
            Ok(())
        };
        if let Some(section) = self.section()
            && (section.is_empty() || section.contains(['[', ']', '\n']))
        {
            return Err(error::user(
                format!("`{section}` is not a usable section name."),
                &["Give the name without brackets, for example `section: all`."],
            ));
        }
        for (key, line) in self.rendered_settings() {
            if key.trim().is_empty() {
                return Err(error::user(
                    "A file.ini setting has an empty key.",
                    &["Name the key, for example `enable_uart: 1`."],
                ));
            }
            if norm(key, sep).starts_with(sep) {
                return Err(error::user(
                    format!("The setting `{key}` starts with the separator `{sep}`."),
                    &["Name the key before the separator, for example `enable_uart: 1`."],
                ));
            }
            single("settings", &line)?;
        }
        for line in &self.lines {
            if line.trim().is_empty() {
                return Err(error::user(
                    "A file.ini task lists an empty line.",
                    &["Remove the empty entry from `lines`."],
                ));
            }
            single("lines", line)?;
        }
        for entry in &self.absent {
            if entry.trim().is_empty() {
                return Err(error::user(
                    "A file.ini task lists an empty `absent` entry, which would match nothing.",
                    &["Remove the empty entry, or name the key to delete."],
                ));
            }
            single("absent", entry)?;
        }
        Ok(())
    }
}

/// `all` for `[all]` or `all`.
fn section_name(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(s)
        .trim()
}

/// The name of a `[section]` header line.
fn header(line: &str) -> Option<&str> {
    let t = line.trim();
    t.strip_prefix('[')?.strip_suffix(']').map(str::trim)
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with(';')
}

/// A line with the padding around its first separator (and its ends) removed.
fn norm(line: &str, sep: &str) -> String {
    let line = line.trim();
    match line.split_once(sep) {
        Some((k, v)) => format!("{}{sep}{}", k.trim(), v.trim()),
        None => line.to_owned(),
    }
}

/// Whether `line` (normalised) is `entry`, or starts with it and then `=` or `,`.
fn matches_entry(line: &str, entry: &str) -> bool {
    line == entry
        || line
            .strip_prefix(entry)
            .is_some_and(|rest| rest.starts_with(['=', ',']))
}

/// For each line, whether it is a non-header line of `section` (`None`: before the first header).
fn members(lines: &[String], section: Option<&str>) -> Vec<bool> {
    let mut current: Option<String> = None;
    lines
        .iter()
        .map(|l| match header(l) {
            Some(h) => {
                current = Some(h.to_owned());
                false
            }
            None => current.as_deref() == section,
        })
        .collect()
}

/// Index of the last `[section]` header.
fn header_index(lines: &[String], section: &str) -> Option<usize> {
    lines.iter().rposition(|l| header(l) == Some(section))
}

/// Where new lines go: after the section's last non-blank line, else after its header.
fn append_index(lines: &[String], member: &[bool], section: Option<&str>) -> Option<usize> {
    if let Some(i) = (0..lines.len())
        .rev()
        .find(|&i| member[i] && !lines[i].trim().is_empty())
    {
        return Some(i + 1);
    }
    match section {
        None => Some(0),
        Some(s) => header_index(lines, s).map(|h| h + 1),
    }
}

/// Undoes `owned` on `lines`: the inverse of [`edit`], step by step. Returns
/// where the added group (or the header Buckle added) stood, so that a re-edit
/// puts it back in the same place.
fn revert(mut lines: Vec<String>, owned: &Owned) -> (Vec<String>, Option<usize>) {
    let sep = owned.sep.as_str();
    let section = owned.section.as_deref();
    for r in &owned.removed {
        // Already back (put back by hand, or reverted before): not twice.
        let member = members(&lines, section);
        let text = norm(&r.text, sep);
        if (0..lines.len()).any(|i| member[i] && norm(&lines[i], sep) == text) {
            continue;
        }
        let at = r.index.min(lines.len());
        lines.insert(at, r.text.clone());
    }

    let member = members(&lines, section);
    let mut drop = BTreeSet::new();
    for added in &owned.added {
        let want = norm(added, sep);
        if let Some(i) = (0..lines.len())
            .rev()
            .find(|&i| member[i] && !drop.contains(&i) && norm(&lines[i], sep) == want)
        {
            drop.insert(i);
        }
    }
    let mut anchor = drop.first().copied();
    for i in drop.into_iter().rev() {
        lines.remove(i);
    }

    if let Some(s) = section.filter(|_| owned.header)
        && let Some(h) = header_index(&lines, s)
    {
        let member = members(&lines, section);
        let empty = (h + 1..lines.len())
            .take_while(|&i| header(&lines[i]).is_none())
            .all(|i| !member[i] || lines[i].trim().is_empty());
        if empty {
            lines.remove(h);
            anchor = Some(h);
        }
    }

    let member = members(&lines, section);
    for r in &owned.replaced {
        let now = norm(&r.now, sep);
        if let Some(i) = (0..lines.len())
            .rev()
            .find(|&i| member[i] && norm(&lines[i], sep) == now)
        {
            lines[i] = r.original.clone();
        }
    }
    (lines, anchor)
}

/// Applies `spec` to `lines`. Pure, so the cases are tested without a host.
fn edit(
    mut lines: Vec<String>,
    spec: &Spec,
    anchor: Option<usize>,
) -> Result<(Vec<String>, Owned)> {
    let sep = spec.sep()?;
    let section = spec.section();
    let mut owned = Owned {
        section: section.map(str::to_owned),
        sep: sep.to_owned(),
        ..Owned::default()
    };
    let member = members(&lines, section);
    let content = |i: usize, lines: &[String]| member[i] && !is_comment(&lines[i]);

    let mut group: Vec<String> = Vec::new();
    for (key, want) in spec.rendered_settings() {
        let key = norm(key, sep);
        let prefix = format!("{key}{sep}");
        let found = (0..lines.len()).rev().find(|&i| {
            content(i, &lines) && {
                let l = norm(&lines[i], sep);
                l == key || l.starts_with(&prefix)
            }
        });
        match found {
            Some(i) if norm(&lines[i], sep) == norm(&want, sep) => {}
            Some(i) => {
                let original = std::mem::replace(&mut lines[i], want.clone());
                owned.replaced.push(Replaced {
                    now: want,
                    original,
                });
            }
            None => group.push(want),
        }
    }
    for line in &spec.lines {
        let want = norm(line, sep);
        let present = (0..lines.len()).any(|i| content(i, &lines) && norm(&lines[i], sep) == want)
            || group.iter().any(|g| norm(g, sep) == want);
        if !present {
            group.push(line.trim().to_owned());
        }
    }

    if !group.is_empty() {
        // The anchor is used only while it still sits inside the section.
        let anchored = anchor.filter(|&a| {
            a <= lines.len()
                && match a.checked_sub(1) {
                    None => section.is_none(),
                    Some(prev) => {
                        member[prev] || section.is_some_and(|s| header(&lines[prev]) == Some(s))
                    }
                }
        });
        let missing_header = section.is_some_and(|s| header_index(&lines, s).is_none());
        let at = if missing_header {
            anchor.unwrap_or(lines.len()).min(lines.len())
        } else {
            anchored
                .or_else(|| append_index(&lines, &member, section))
                .unwrap_or(lines.len())
        };
        let mut insert = group.clone();
        if missing_header && let Some(s) = section {
            insert.insert(0, format!("[{s}]"));
            owned.header = true;
        }
        lines.splice(at..at, insert);
        owned.added = group;
    }

    if !spec.absent.is_empty() {
        let wanted: BTreeSet<String> = spec
            .rendered_settings()
            .into_iter()
            .map(|(_, l)| norm(&l, sep))
            .chain(spec.lines.iter().map(|l| norm(l, sep)))
            .collect();
        let entries: Vec<String> = spec.absent.iter().map(|e| norm(e, sep)).collect();
        let member = members(&lines, section);
        let mut kept = Vec::with_capacity(lines.len());
        for (index, text) in lines.into_iter().enumerate() {
            let l = norm(&text, sep);
            if member[index]
                && !is_comment(&text)
                && !wanted.contains(&l)
                && entries.iter().any(|e| matches_entry(&l, e))
            {
                owned.removed.push(Removed { index, text });
            } else {
                kept.push(text);
            }
        }
        lines = kept;
    }
    Ok((lines, owned))
}

/// The edit an earlier apply of this resource recorded for `path`.
fn previous_owned(cx: &Cx, path: &std::path::Path) -> Option<Owned> {
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

fn where_(section: Option<&str>, path: &std::path::Path) -> String {
    match section {
        Some(s) => format!("[{s}] in {}", path.display()),
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
        existing: Option<hostkit::TextFile>,
    },
}

impl FileIni {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Outcome> {
        spec.validate()?;
        let existing = hostkit::load_text(cx, &spec.path).await?;
        if existing.is_none() && !spec.create {
            return Ok(Outcome::Blocked(format!(
                "{} does not exist; set `create: true` to create it",
                spec.path.display()
            )));
        }
        let text = existing.as_ref().map(|f| f.text.as_str()).unwrap_or("");
        let current = hostkit::split_lines(text);
        let previous = previous_owned(cx, &spec.path);
        let (baseline, anchor) = match &previous {
            Some(owned) => revert(current.clone(), owned),
            None => (current.clone(), None),
        };
        let (lines, mut owned) = edit(baseline, spec, anchor)?;
        owned.created = existing.is_none() || previous.as_ref().is_some_and(|p| p.created);
        if existing.is_some() && lines == current {
            return Ok(Outcome::NoOp(owned));
        }
        let new_text = hostkit::join_lines(&lines);
        let summary = format!("edit {}", where_(spec.section(), &spec.path));
        let diff = match &existing {
            Some(_) => hostkit::text_diff(cx, &spec.path, summary, text, &new_text),
            None => Diff::Create {
                summary: format!("create {}", spec.path.display()),
                preview: Some(Redacted::plain(new_text.clone())),
            },
        };
        Ok(Outcome::Change {
            diff,
            new_text,
            owned,
            existing,
        })
    }

    fn applied(spec: &Spec, applied: Applied, owned: &Owned) -> Result<Applied> {
        if owned.is_empty() {
            return Ok(applied);
        }
        let marker = serde_json::to_string(owned).map_err(|e| {
            error::wrap_system(
                e,
                "Could not record the file.ini edit.",
                &["This is a bug in Buckle; please report it."],
            )
        })?;
        Ok(applied.with_artefact(Artefact::Line {
            path: spec.path.clone(),
            marker,
        }))
    }
}

#[async_trait]
impl Task for FileIni {
    type Spec = Spec;
    const KIND: &'static str = "file.ini";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Own settings and lines in one section of an INI-style file (config.txt, php.ini, .conf).";

    fn key(spec: &Spec) -> String {
        match spec.section() {
            Some(s) => format!("{} [{s}]", spec.path.display()),
            None => spec.path.display().to_string(),
        }
    }

    fn describe(spec: &Spec) -> String {
        let n = spec.settings.len() + spec.lines.len();
        let mut text = format!(
            "{n} entr{} in {}",
            if n == 1 { "y" } else { "ies" },
            where_(spec.section(), &spec.path)
        );
        if !spec.absent.is_empty() {
            text.push_str(&format!(", {} absent", spec.absent.len()));
        }
        text
    }

    #[instrument(name = "task.file.ini.plan", skip_all, fields(path = %spec.path.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(match self.inspect(cx, spec).await? {
            Outcome::NoOp(_) => Planned::NoOp,
            Outcome::Blocked(reason) => Planned::blocked(reason),
            Outcome::Change { diff, .. } => Planned::change(diff),
        })
    }

    #[instrument(name = "task.file.ini.apply", skip_all, fields(path = %spec.path.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let _guard = hostkit::lock_path(&spec.path).await;
        match self.inspect(cx, spec).await? {
            Outcome::NoOp(owned) => Self::applied(
                spec,
                Applied::unchanged(format!(
                    "{} already up to date",
                    where_(spec.section(), &spec.path)
                )),
                &owned,
            ),
            Outcome::Blocked(reason) => Err(error::user(
                format!("Cannot edit {}: {reason}.", spec.path.display()),
                &[
                    "Set `create: true`, create the file in an earlier task, or limit the task with `when`.",
                ],
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
                    existing.as_ref(),
                    spec.backup,
                    &FilePerms::mode(0o644),
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
                    parts.push(format!("delete {} added line(s)", owned.added.len()));
                }
                if !owned.replaced.is_empty() {
                    parts.push(format!("restore {} replaced line(s)", owned.replaced.len()));
                }
                if !owned.removed.is_empty() {
                    parts.push(format!("put back {} deleted line(s)", owned.removed.len()));
                }
                if parts.is_empty() {
                    parts.push("revert the edit".to_owned());
                }
                format!(
                    "{} in {}",
                    parts.join(", "),
                    where_(owned.section.as_deref(), &path)
                )
            }
            None => format!("nothing to revert for {}", record.key.key),
        }
    }

    #[instrument(name = "task.file.ini.remove", skip_all, fields(resource = %record.key))]
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
        let current = hostkit::split_lines(&file.text);
        let (lines, _) = revert(current.clone(), &owned);
        if owned.created && lines.iter().all(|l| l.trim().is_empty()) {
            cx.system.remove(&path, false).await?;
            return Ok(Applied::changed(format!(
                "deleted {}, which Buckle created",
                path.display()
            )));
        }
        if lines == current {
            return Ok(Applied::unchanged(format!(
                "nothing of Buckle's edit is left in {}",
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
            "reverted the edit of {}",
            where_(owned.section.as_deref(), &path)
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

    const PATH: &str = "/boot/firmware/config.txt";

    fn s(extra: serde_json::Value) -> Spec {
        let mut v = json!({"path": PATH});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(v)
    }

    fn run(extra: serde_json::Value, before: &str) -> String {
        let (lines, _) = edit(hostkit::split_lines(before), &s(extra), None).unwrap();
        hostkit::join_lines(&lines)
    }

    #[rstest]
    #[case::append_to_section(
        json!({"section": "all", "settings": {"enable_uart": 1}}),
        "[pi4]\narm_boost=1\n\n[all]\narm_64bit=1\n",
        "[pi4]\narm_boost=1\n\n[all]\narm_64bit=1\nenable_uart=1\n"
    )]
    #[case::append_before_the_next_section(
        json!({"section": "pi4", "lines": ["dtoverlay=x"]}),
        "[pi4]\narm_boost=1\n\n[all]\narm_64bit=1\n",
        "[pi4]\narm_boost=1\ndtoverlay=x\n\n[all]\narm_64bit=1\n"
    )]
    #[case::replace_last_match_in_place(
        json!({"section": "all", "settings": {"enable_uart": 1}}),
        "[all]\nenable_uart=0\nfoo=1\nenable_uart = 0\n",
        "[all]\nenable_uart=0\nfoo=1\nenable_uart=1\n"
    )]
    #[case::only_the_named_section(
        json!({"section": "all", "settings": {"enable_uart": 1}}),
        "[pi4]\nenable_uart=0\n[all]\n",
        "[pi4]\nenable_uart=0\n[all]\nenable_uart=1\n"
    )]
    #[case::compound_keys(
        json!({"section": "all", "settings": {"dtparam=poe_fan_temp0": 50000}}),
        "[all]\ndtparam=poe_fan_temp0=60000\ndtparam=poe_fan_temp1=65000\n",
        "[all]\ndtparam=poe_fan_temp0=50000\ndtparam=poe_fan_temp1=65000\n"
    )]
    #[case::identical_lines_are_adopted(
        json!({"section": "all", "lines": ["dtoverlay=rpi-poe-plus", "dtoverlay=pps-gpio"]}),
        "[all]\ndtoverlay = rpi-poe-plus\n",
        "[all]\ndtoverlay = rpi-poe-plus\ndtoverlay=pps-gpio\n"
    )]
    #[case::settings_before_lines(
        json!({"section": "all", "settings": {"enable_uart": 1}, "lines": ["dtoverlay=x"]}),
        "[all]\n",
        "[all]\nenable_uart=1\ndtoverlay=x\n"
    )]
    #[case::missing_section_is_appended(
        json!({"section": "pi5", "settings": {"pciex1_gen": 3}}),
        "[all]\narm_64bit=1\n",
        "[all]\narm_64bit=1\n[pi5]\npciex1_gen=3\n"
    )]
    #[case::global_region_without_headers(
        json!({"settings": {"a": "b"}}),
        "x=1\n",
        "x=1\na=b\n"
    )]
    #[case::global_region_goes_before_the_first_header(
        json!({"settings": {"a": "b"}}),
        "# top\nx=1\n\n[s]\ny=2\n",
        "# top\nx=1\na=b\n\n[s]\ny=2\n"
    )]
    #[case::padded_separator(
        json!({"section": "PHP", "settings": {"memory_limit": "512M"}, "separator": " = "}),
        "[PHP]\nmemory_limit = 128M\n",
        "[PHP]\nmemory_limit = 512M\n"
    )]
    #[case::absent_by_key_and_prefix(
        json!({"section": "all", "absent": ["enable_uart", "dtoverlay=vc4-kms-v3d"]}),
        "[all]\nenable_uart=1\ndtoverlay=vc4-kms-v3d,cma-512\ndtoverlay=vc4-kms-v3d-pi4\n# enable_uart=1\n",
        "[all]\ndtoverlay=vc4-kms-v3d-pi4\n# enable_uart=1\n"
    )]
    #[case::absent_keeps_wanted_lines(
        json!({"section": "all", "lines": ["dtoverlay=a"], "absent": ["dtoverlay"]}),
        "[all]\ndtoverlay=a\ndtoverlay=b\n",
        "[all]\ndtoverlay=a\n"
    )]
    #[case::comments_are_never_matched(
        json!({"section": "all", "settings": {"enable_uart": 1}}),
        "[all]\n#enable_uart=0\n",
        "[all]\n#enable_uart=0\nenable_uart=1\n"
    )]
    fn edit_cases(#[case] extra: serde_json::Value, #[case] before: &str, #[case] after: &str) {
        assert_eq!(run(extra, before), after);
    }

    #[rstest]
    #[case::relative_path(json!({"path": "config.txt"}))]
    #[case::multi_line_setting(json!({"settings": {"a": "1\nb=2"}}))]
    #[case::header_as_line(json!({"lines": ["[all]"]}))]
    #[case::bracket_in_section(json!({"section": "a]b"}))]
    #[case::blank_separator(json!({"separator": " "}))]
    #[case::empty_absent(json!({"absent": [""]}))]
    #[case::sensitive_file(json!({"path": "/etc/ssh/sshd_config"}))]
    fn misconfiguration_is_a_user_error(#[case] extra: serde_json::Value) {
        assert!(s(extra).validate().is_err());
    }

    /// A context whose previous record is `applied`, as the engine would give the next run.
    fn cx_after(sys: &FakeSystem, applied: &Applied) -> Cx {
        CxBuilder::new(sys.clone())
            .previous(record("file.ini", "k", applied))
            .build()
    }

    const PI: &str =
        "# boot config\n[cm4]\notg_mode=1\n\n[all]\ndtoverlay=vc4-kms-v3d\nenable_uart=0\n";

    #[tokio::test]
    async fn applies_idempotently_and_reverts_on_removal() {
        let sys = FakeSystem::new().with_file(PATH, PI, FilePerms::mode(0o755));
        let c = cx(&sys);
        let spec = s(json!({
            "section": "all",
            "settings": {"enable_uart": 1, "dtparam=poe_fan_temp0": 50000},
            "lines": ["dtoverlay=rpi-poe-plus"],
            "absent": ["dtoverlay=vc4-kms-v3d"],
        }));
        let applied = apply(&FileIni, &c, &spec).await;
        let after = sys.file_string(PATH).unwrap();
        assert_eq!(
            after,
            "# boot config\n[cm4]\notg_mode=1\n\n[all]\nenable_uart=1\ndtparam=poe_fan_temp0=50000\ndtoverlay=rpi-poe-plus\n"
        );
        assert_eq!(
            plan_class(&FileIni, &cx_after(&sys, &applied), &spec).await,
            Class::NoOp
        );

        let out = FileIni
            .remove(&c, &record("file.ini", "k", &applied))
            .await
            .unwrap();
        assert!(out.changed, "{out:?}");
        assert_eq!(sys.file_string(PATH).unwrap(), PI);
        let meta = buckle_core::system::System::stat(&sys, std::path::Path::new(PATH))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.mode, Some(0o755), "mode kept");
        // Reverting twice is harmless.
        let again = FileIni
            .remove(&c, &record("file.ini", "k", &applied))
            .await
            .unwrap();
        assert!(!again.changed, "{again:?}");
        assert_eq!(sys.file_string(PATH).unwrap(), PI);
    }

    #[tokio::test]
    async fn dropping_an_entry_reverts_just_that_entry() {
        let sys = FakeSystem::new().with_file(PATH, "[all]\nenable_uart=0\n", FilePerms::default());
        let first = s(
            json!({"section": "all", "settings": {"enable_uart": 1}, "lines": ["dtoverlay=a", "dtoverlay=b"]}),
        );
        let applied = apply(&FileIni, &cx(&sys), &first).await;
        assert_eq!(
            sys.file_string(PATH).unwrap(),
            "[all]\nenable_uart=1\ndtoverlay=a\ndtoverlay=b\n"
        );

        let second = s(json!({"section": "all", "lines": ["dtoverlay=b"]}));
        let c = cx_after(&sys, &applied);
        let applied = apply(&FileIni, &c, &second).await;
        assert_eq!(
            sys.file_string(PATH).unwrap(),
            "[all]\nenable_uart=0\ndtoverlay=b\n"
        );
        assert_eq!(
            plan_class(&FileIni, &cx_after(&sys, &applied), &second).await,
            Class::NoOp
        );
    }

    #[tokio::test]
    async fn changing_a_value_keeps_the_original_for_removal() {
        let sys = FakeSystem::new().with_file(PATH, "[all]\narm_freq=1500\n", FilePerms::default());
        let a = apply(
            &FileIni,
            &cx(&sys),
            &s(json!({"section": "all", "settings": {"arm_freq": 1800}})),
        )
        .await;
        let b = apply(
            &FileIni,
            &cx_after(&sys, &a),
            &s(json!({"section": "all", "settings": {"arm_freq": 2000}})),
        )
        .await;
        assert_eq!(sys.file_string(PATH).unwrap(), "[all]\narm_freq=2000\n");
        FileIni
            .remove(&cx(&sys), &record("file.ini", "k", &b))
            .await
            .unwrap();
        assert_eq!(sys.file_string(PATH).unwrap(), "[all]\narm_freq=1500\n");
    }

    #[tokio::test]
    async fn added_lines_keep_their_place_when_others_follow() {
        let sys = FakeSystem::new().with_file(PATH, "[all]\na=1\n", FilePerms::default());
        let spec = s(json!({"section": "all", "lines": ["dtoverlay=x"]}));
        let applied = apply(&FileIni, &cx(&sys), &spec).await;
        put(&sys, PATH, "[all]\na=1\ndtoverlay=x\nhand=1\n").await;
        assert_eq!(
            plan_class(&FileIni, &cx_after(&sys, &applied), &spec).await,
            Class::NoOp
        );
    }

    #[tokio::test]
    async fn an_added_section_is_removed_with_its_lines() {
        let sys = FakeSystem::new().with_file(PATH, "[all]\na=1\n", FilePerms::default());
        let spec = s(json!({"section": "pi5", "settings": {"pciex1_gen": 3}}));
        let applied = apply(&FileIni, &cx(&sys), &spec).await;
        assert_eq!(
            plan_class(&FileIni, &cx_after(&sys, &applied), &spec).await,
            Class::NoOp
        );
        FileIni
            .remove(&cx(&sys), &record("file.ini", "k", &applied))
            .await
            .unwrap();
        assert_eq!(sys.file_string(PATH).unwrap(), "[all]\na=1\n");
    }

    #[tokio::test]
    async fn hand_edits_are_left_on_removal() {
        let sys = FakeSystem::new().with_file(PATH, "[all]\nenable_uart=0\n", FilePerms::default());
        let applied = apply(
            &FileIni,
            &cx(&sys),
            &s(json!({"section": "all", "settings": {"enable_uart": 1}})),
        )
        .await;
        put(&sys, PATH, "[all]\nenable_uart=2\n").await;
        let out = FileIni
            .remove(&cx(&sys), &record("file.ini", "k", &applied))
            .await
            .unwrap();
        assert!(!out.changed);
        assert_eq!(sys.file_string(PATH).unwrap(), "[all]\nenable_uart=2\n");
    }

    #[tokio::test]
    async fn missing_files_block_unless_created() {
        let sys = FakeSystem::new();
        let c = cx(&sys);
        let spec = s(json!({"section": "all", "settings": {"enable_uart": 1}}));
        assert_eq!(plan_class(&FileIni, &c, &spec).await, Class::Blocked);
        assert!(FileIni.apply(&c, &spec, &Planned::NoOp).await.is_err());

        let spec = s(
            json!({"path": "/etc/app.ini", "section": "main", "settings": {"a": 1}, "create": true}),
        );
        let applied = apply(&FileIni, &c, &spec).await;
        assert_eq!(sys.file_string("/etc/app.ini").unwrap(), "[main]\na=1\n");
        assert_eq!(
            plan_class(&FileIni, &cx_after(&sys, &applied), &spec).await,
            Class::NoOp
        );
        FileIni
            .remove(&c, &record("file.ini", "k", &applied))
            .await
            .unwrap();
        assert!(!sys.exists("/etc/app.ini"));
    }

    #[tokio::test]
    async fn plans_show_a_unified_diff() {
        let sys = FakeSystem::new().with_file(PATH, "[all]\nenable_uart=0\n", FilePerms::default());
        let planned = FileIni
            .plan(
                &cx(&sys),
                &s(json!({"section": "all", "settings": {"enable_uart": 1}})),
            )
            .await
            .unwrap();
        let Diff::Update {
            unified: Some(u), ..
        } = crate::tasks::hostkit::testkit::diff(&planned)
        else {
            panic!("{planned:?}")
        };
        assert!(u.get().contains("-enable_uart=0") && u.get().contains("+enable_uart=1"));
    }
}
