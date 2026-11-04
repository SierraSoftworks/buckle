//! Helpers shared by the file, archive, command, user and system task kinds.
//!
//! Nothing here is registered as a task. The helpers keep the kinds small and
//! make their behaviour uniform: the same permission comparison, the same
//! previews, the same "changed outside Buckle, left in place" removal rule.

pub(crate) mod ownership;
#[cfg(test)]
pub(crate) mod testkit;
pub(crate) mod validate;

use buckle_core::error;
use buckle_core::plan::Planned;
use buckle_core::secrets::Redacted;
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, FileKind, FileMeta, FilePerms, Output};
use buckle_core::task::{Applied, Artefact, Cx};
use buckle_core::{Digest, Platform, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Previews of text content shown in plans stop after this many bytes.
pub const PREVIEW_LIMIT: usize = 64 * 1024;

/// Redacted shadows kept in state are dropped beyond this size.
pub const SHADOW_LIMIT: usize = 256 * 1024;

/// `present` or `absent`, for kinds that can remove what they manage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Ensure {
    #[default]
    Present,
    Absent,
}

/// A command given either as an argv list (run directly) or as a string (run
/// through `sh -c`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum StringOrArgv {
    /// A shell snippet, run with `sh -c`.
    Shell(String),
    /// A program and its arguments, run without a shell.
    Argv(Vec<String>),
}

impl StringOrArgv {
    /// Builds the process to run. An empty argv is a user error.
    pub fn to_command(&self, platform: Platform) -> Result<Command> {
        match self {
            StringOrArgv::Shell(script) if script.trim().is_empty() => Err(error::user(
                "A command was given as an empty string.",
                &["Provide the command to run, for example `run: [\"systemctl\", \"daemon-reload\"]`."],
            )),
            StringOrArgv::Shell(script) => Ok(if platform == Platform::Windows {
                Command::new("cmd").arg("/C").arg(script.clone())
            } else {
                Command::new("sh").arg("-c").arg(script.clone())
            }),
            StringOrArgv::Argv(argv) => Command::from_argv(argv).ok_or_else(|| {
                error::user(
                    "A command was given as an empty list.",
                    &["Provide the program as the first element, for example `[\"systemctl\", \"daemon-reload\"]`."],
                )
            }),
        }
    }

    /// The command as the operator would type it, for summaries.
    pub fn display(&self) -> String {
        match self {
            StringOrArgv::Shell(s) => s.trim().to_owned(),
            StringOrArgv::Argv(a) => a
                .iter()
                .map(|p| {
                    if p.is_empty() || p.contains(char::is_whitespace) {
                        format!("{p:?}")
                    } else {
                        p.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

/// A user error for a path that must be absolute.
pub fn require_absolute(path: &Path, field: &str) -> Result<()> {
    // `has_root` accepts `/etc/example.conf` on Windows too, where `is_absolute` wants a drive.
    if path.is_absolute() || path.has_root() {
        Ok(())
    } else {
        Err(error::user(
            format!("The `{field}` path '{}' is not absolute.", path.display()),
            &["Use an absolute path such as /etc/example.conf."],
        ))
    }
}

/// Reads a file from the module's `files/` directory, failing with advice when it is missing.
pub async fn read_module_file(cx: &Cx, rel: &str) -> Result<Vec<u8>> {
    cx.source.read(Path::new(rel)).await?.ok_or_else(|| {
        error::user(
            format!("The module file '{rel}' does not exist."),
            &["Create the file under the module's files/ directory, or fix the `src` path."],
        )
    })
}

/// Reads a module file that must be UTF-8 text.
pub async fn read_module_text(cx: &Cx, rel: &str) -> Result<String> {
    let bytes = read_module_file(cx, rel).await?;
    String::from_utf8(bytes).map_err(|_| {
        error::user(
            format!("The module file '{rel}' is not valid UTF-8 text."),
            &["Use file.copy for binary files; templates must be text."],
        )
    })
}

/// A redacted preview of content that did not pass through a secret-aware render
/// (module files and host files). Binary content previews as `binary`.
pub fn text_preview(bytes: &[u8]) -> Redacted<String> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Redacted::plain(truncate_preview(text.to_owned(), bytes.len())),
        Err(_) => Redacted::plain("binary".to_owned()),
    }
}

/// Truncates an already-redacted render to the preview limit.
pub fn truncate_redacted(text: Redacted<String>) -> Redacted<String> {
    text.map(|t| {
        let total = t.len();
        truncate_preview(t, total)
    })
}

fn truncate_preview(mut text: String, total: usize) -> String {
    if text.len() <= PREVIEW_LIMIT {
        return text;
    }
    let mut cut = PREVIEW_LIMIT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str(&format!("\n... (truncated, {total} bytes in total)\n"));
    text
}

/// The text of `bytes` when it is a UTF-8 document small enough to diff.
pub fn diffable_text(bytes: &[u8]) -> Option<&str> {
    if bytes.len() > SHADOW_LIMIT {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

/// A shadow for state when `bytes` is small UTF-8 text from a non-secret source.
pub fn shadow_of(bytes: &[u8]) -> Option<Redacted<String>> {
    diffable_text(bytes).map(|t| Redacted::plain(t.to_owned()))
}

/// `digest 9a1f6c2e → 02c7b4aa`, for content that must not be displayed.
pub fn digest_change(old: &Digest, new: &Digest) -> String {
    format!("digest {} → {}", old.short(), new.short())
}

/// Describes the fields of `perms` that are set: `0644 root:root`.
pub fn describe_perms(perms: &FilePerms) -> String {
    let mut parts = Vec::new();
    if let Some(mode) = perms.mode {
        parts.push(format!("{mode:04o}"));
    }
    match (&perms.owner, &perms.group) {
        (Some(o), Some(g)) => parts.push(format!("{o}:{g}")),
        (Some(o), None) => parts.push(o.clone()),
        (None, Some(g)) => parts.push(format!(":{g}")),
        (None, None) => {}
    }
    parts.join(" ")
}

/// The differences between a path's current permissions and the wanted ones, as
/// `mode 0644 → 0600` style fragments. Fields that are not set in `want` are ignored.
pub fn perm_changes(meta: &FileMeta, want: &FilePerms) -> Vec<String> {
    let mut changes = Vec::new();
    if let Some(mode) = want.mode {
        let current = meta.mode.map(|m| m & 0o7777);
        if current != Some(mode) {
            changes.push(match current {
                Some(c) => format!("mode {c:04o} → {mode:04o}"),
                None => format!("mode → {mode:04o}"),
            });
        }
    }
    if let Some(owner) = &want.owner
        && meta.owner.as_deref() != Some(owner.as_str())
    {
        changes.push(format!(
            "owner {} → {owner}",
            meta.owner.as_deref().unwrap_or("?")
        ));
    }
    if let Some(group) = &want.group
        && meta.group.as_deref() != Some(group.as_str())
    {
        changes.push(format!(
            "group {} → {group}",
            meta.group.as_deref().unwrap_or("?")
        ));
    }
    changes
}

/// The permissions to write with: the wanted ones, falling back to the existing
/// file's so that editing a file in place does not reset its mode and ownership.
pub fn effective_perms(want: &FilePerms, existing: Option<&FileMeta>) -> FilePerms {
    let Some(meta) = existing else {
        return want.clone();
    };
    FilePerms {
        mode: want.mode.or(meta.mode.map(|m| m & 0o7777)),
        owner: want.owner.clone().or_else(|| meta.owner.clone()),
        group: want.group.clone().or_else(|| meta.group.clone()),
    }
}

/// [`effective_perms`] for content that may hold secrets: a new file whose
/// mode is not given is created 0600 rather than with the default umask.
pub fn effective_perms_sensitive(
    want: &FilePerms,
    existing: Option<&FileMeta>,
    sensitive: bool,
) -> FilePerms {
    let mut perms = effective_perms(want, existing);
    if sensitive && existing.is_none() && perms.mode.is_none() {
        perms.mode = Some(0o600);
    }
    perms
}

/// The permissions an existing file already has, for rewriting it unchanged.
pub fn perms_of(meta: &FileMeta) -> FilePerms {
    FilePerms {
        mode: meta.mode.map(|m| m & 0o7777),
        owner: meta.owner.clone(),
        group: meta.group.clone(),
    }
}

/// Whether anything is set in `perms`.
pub fn has_perms(perms: &FilePerms) -> bool {
    perms.mode.is_some() || perms.owner.is_some() || perms.group.is_some()
}

/// The `Planned::Blocked` for a missing tool, or `None` when it is installed.
pub async fn missing_tool(cx: &Cx, tool: &str, hint: &str) -> Result<Option<Planned>> {
    if cx.system.which(tool).await?.is_some() {
        return Ok(None);
    }
    Ok(Some(Planned::blocked(format!(
        "`{tool}` is not installed on this host; {hint}"
    ))))
}

/// The user error raised in apply when a required tool is missing.
pub async fn require_tool(cx: &Cx, tool: &str, hint: &str) -> Result<()> {
    if cx.system.which(tool).await?.is_some() {
        return Ok(());
    }
    Err(error::user(
        format!("`{tool}` is not installed on this host; {hint}"),
        &[
            "Install the tool (for example with an apt.package task that runs earlier in the module).",
        ],
    ))
}

const OUTPUT_TAIL: usize = 2048;

/// The last part of a process's output, trimmed, for error messages.
pub fn output_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.len() <= OUTPUT_TAIL {
        return text.to_owned();
    }
    let mut start = text.len() - OUTPUT_TAIL;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &text[start..])
}

/// Runs a command and turns a non-zero exit into a user error carrying stderr
/// (withheld when the command's environment carried secrets).
pub async fn run_checked(cx: &Cx, command: Command, what: &str) -> Result<Output> {
    let sensitive = command.sensitive_env;
    let line = command.display_line();
    let out = cx.system.run(command).await?;
    if out.success() {
        return Ok(out);
    }
    Err(command_failed(cx, what, &line, &out, sensitive))
}

/// Scrubs every known secret value (and its common encodings) from `text`.
pub fn scrub(cx: &Cx, text: &str) -> String {
    if cx.secrets.is_empty() {
        return text.to_owned();
    }
    buckle_core::secrets::redact::Redactor::new(&cx.secrets)
        .redact(text)
        .into_inner()
}

/// The error for a command that did not succeed. Its output is scrubbed of
/// known secrets, and withheld entirely when `sensitive`.
pub fn command_failed(
    cx: &Cx,
    what: &str,
    line: &str,
    out: &Output,
    sensitive: bool,
) -> buckle_core::Error {
    let status = if out.timed_out {
        "timed out".to_owned()
    } else {
        match out.code {
            Some(code) => format!("exited with status {code}"),
            None => "was terminated by a signal".to_owned(),
        }
    };
    let detail = if sensitive {
        "Its output is withheld because the command's environment carried secrets.".to_owned()
    } else {
        let stderr = scrub(cx, &output_tail(&out.stderr));
        let stdout = scrub(cx, &output_tail(&out.stdout));
        match (stderr.is_empty(), stdout.is_empty()) {
            (false, _) => format!("stderr: {stderr}"),
            (true, false) => format!("stdout: {stdout}"),
            (true, true) => "It printed nothing.".to_owned(),
        }
    };
    error::user(
        format!("{what} failed: `{line}` {status}. {detail}"),
        &[
            "Run the command by hand on the host to see the failure in context.",
            "Check the arguments in the task and that any tools it needs are installed.",
        ],
    )
}

/// Removes the single file a task recorded, but only when its content still has
/// the digest recorded at apply time. Anything else is drift and is left alone.
pub async fn remove_recorded_file(cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
    let Some(path) = record.artefacts.iter().find_map(|a| match a {
        Artefact::File { path } => Some(path.clone()),
        _ => None,
    }) else {
        if ownership::any_adopted(&record.artefacts) {
            return Ok(Applied::skipped(format!(
                "{} existed before Buckle managed it (only its permissions were set), left in place",
                record.key.key
            )));
        }
        return Ok(Applied::skipped("nothing recorded to remove"));
    };
    let Some(bytes) = cx.system.read(&path).await? else {
        return Ok(Applied::unchanged(format!(
            "{} was already absent",
            path.display()
        )));
    };
    match &record.content_digest {
        Some(expected) if *expected == Digest::of(&bytes) => {
            cx.system.remove(&path, false).await?;
            Ok(Applied::changed(format!("deleted {}", path.display())))
        }
        Some(_) => Ok(Applied::skipped(format!(
            "{} changed outside Buckle, left in place",
            path.display()
        ))),
        None => Ok(Applied::skipped(format!(
            "{} has no recorded digest to verify against, left in place",
            path.display()
        ))),
    }
}

/// Serialises read-modify-write edits of one file across tasks and modules.
///
/// `Task::LOCKS` is static, so the executor cannot lock on a path; editors
/// (`file.line`, `file.block`, `ssh.authorized_key`) instead hold this
/// in-process lock, keyed by the path, from reading the file to writing it
/// back. Two modules editing `/etc/ssh/sshd_config` therefore never lose each
/// other's line, while edits of different files still run in parallel.
pub async fn lock_path(path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    use std::collections::HashMap;
    use std::sync::{Arc, LazyLock, Mutex, Weak};
    type Table = Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>;
    static LOCKS: LazyLock<Table> = LazyLock::new(|| Mutex::new(HashMap::new()));
    let lock = {
        let mut table = LOCKS.lock().unwrap_or_else(|p| p.into_inner());
        table.retain(|_, w| w.strong_count() > 0);
        match table.get(path).and_then(Weak::upgrade) {
            Some(lock) => lock,
            None => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                table.insert(path.to_path_buf(), Arc::downgrade(&lock));
                lock
            }
        }
    };
    lock.lock_owned().await
}

/// Splits text into lines without their terminators.
pub fn split_lines(text: &str) -> Vec<String> {
    text.lines().map(str::to_owned).collect()
}

/// Joins lines with a terminator after every line, including the last.
pub fn join_lines(lines: &[String]) -> String {
    let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    for line in lines {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Compiles a user-supplied regular expression with a helpful error.
pub fn compile_regex(pattern: &str, field: &str) -> Result<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| {
        error::wrap_user(
            e,
            format!("The `{field}` pattern '{pattern}' is not a valid regular expression."),
            &["Check the pattern's syntax; Buckle uses the Rust `regex` dialect."],
        )
    })
}

/// A path as the `Artefact::File` for it.
pub fn file_artefact(path: &Path) -> Artefact {
    Artefact::File {
        path: path.to_path_buf(),
    }
}

/// A timestamped backup path for `path`.
pub fn backup_path(cx: &Cx, path: &Path) -> PathBuf {
    let stamp = cx.system.now().format("%Y%m%dT%H%M%SZ");
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.{stamp}.bak"))
}

/// Copies an existing file aside before it is changed.
pub async fn backup_file(cx: &Cx, path: &Path, meta: &FileMeta, bytes: &[u8]) -> Result<PathBuf> {
    let target = backup_path(cx, path);
    cx.system
        .write_atomic(&target, bytes, &perms_of(meta))
        .await?;
    Ok(target)
}

/// Truncates a command for use in span fields.
pub fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &text[..cut])
}

/// The `Applied` for a file whose content Buckle wrote: the file artefact, the
/// digest of the real content and, when available, the redacted shadow.
pub fn written(
    summary: impl Into<String>,
    path: &Path,
    digest: Digest,
    shadow: Option<Redacted<String>>,
    changed: bool,
) -> Applied {
    let mut applied = if changed {
        Applied::changed(summary)
    } else {
        Applied::unchanged(summary)
    };
    applied.artefacts.push(file_artefact(path));
    applied.content_digest = Some(digest);
    applied.shadow = shadow;
    applied
}

/// A UTF-8 text file read from the host, with its metadata.
pub struct TextFile {
    pub meta: FileMeta,
    pub text: String,
}

/// Reads a text file for in-place editing. `Ok(None)` when it does not exist;
/// directories and non-UTF-8 files are user errors.
pub async fn load_text(cx: &Cx, path: &Path) -> Result<Option<TextFile>> {
    let Some(meta) = cx.system.stat(path).await? else {
        return Ok(None);
    };
    if meta.kind == FileKind::Directory {
        return Err(error::user(
            format!("{} is a directory, not a file.", path.display()),
            &["Point `path` at a file."],
        ));
    }
    let bytes = cx.system.read(path).await?.unwrap_or_default();
    let text = String::from_utf8(bytes).map_err(|_| {
        error::user(
            format!(
                "{} is not valid UTF-8 text, so it cannot be edited line by line.",
                path.display()
            ),
            &["Use file.copy or file.template to manage binary files."],
        )
    })?;
    Ok(Some(TextFile { meta, text }))
}

/// Writes edited text back, keeping the file's mode and ownership (or using
/// `default_perms` for a new file), optionally backing the old content up first.
pub async fn save_text(
    cx: &Cx,
    path: &Path,
    new_text: &str,
    existing: Option<&TextFile>,
    backup: bool,
    default_perms: &FilePerms,
) -> Result<()> {
    let perms = match existing {
        Some(file) => {
            if backup {
                backup_file(cx, path, &file.meta, file.text.as_bytes()).await?;
            }
            perms_of(&file.meta)
        }
        None => default_perms.clone(),
    };
    cx.system
        .write_atomic(path, new_text.as_bytes(), &perms)
        .await
}

/// The token prefix a redacted render uses for a secret (`«secret:NAME@fp»`).
pub const SECRET_TOKEN: &str = "\u{ab}secret:";

/// Files whose content commonly holds credentials or whose other lines are
/// nobody's business in a plan: `/etc/environment`, `/etc/fstab`, sudoers,
/// anything under `/etc/ssh` or `/etc/netplan`, `~/.ssh/*`, `/etc/shadow`.
/// Their diffs carry no context lines and their replaced lines are never
/// stored in state.
pub fn is_sensitive_path(path: &Path) -> bool {
    let text = path.to_string_lossy();
    path == Path::new("/etc/environment")
        || path == Path::new("/etc/fstab")
        || path == Path::new("/etc/shadow")
        || path == Path::new("/etc/gshadow")
        || path == Path::new("/etc/sudoers")
        || path.starts_with("/etc/sudoers.d")
        || path.starts_with("/etc/ssh")
        || path.starts_with("/etc/netplan")
        || text.contains("/.ssh/")
}

/// Whether this resource's previous record shows that the file held secrets
/// (its redacted shadow carries a secret token).
pub fn ever_sensitive(cx: &Cx) -> bool {
    cx.previous
        .as_deref()
        .and_then(|r| r.shadow.as_ref())
        .is_some_and(|s| s.get().contains(SECRET_TOKEN))
}

/// Context lines for diffs of `path`: none for sensitive files, else three.
pub fn diff_context(cx: &Cx, path: &Path) -> usize {
    if is_sensitive_path(path) || ever_sensitive(cx) {
        0
    } else {
        3
    }
}

/// A unified diff (`current` → `planned`) with `context` lines, scrubbed of
/// every known secret value.
pub fn redacted_unified(
    cx: &Cx,
    summary: impl Into<String>,
    old: &str,
    new: &str,
    context: usize,
) -> buckle_core::plan::Diff {
    let text = similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(context)
        .header("current", "planned")
        .to_string();
    buckle_core::plan::Diff::Update {
        summary: summary.into(),
        unified: Some(buckle_core::secrets::redact::Redactor::new(&cx.secrets).redact(&text)),
    }
}

/// A diff between the current text of host file `path` and its new text:
/// unified when small (no context lines for sensitive files, scrubbed of
/// known secrets), a summary when large, and without the old side at all
/// when the file is known to have held secrets.
pub fn text_diff(
    cx: &Cx,
    path: &Path,
    summary: impl Into<String>,
    old: &str,
    new: &str,
) -> buckle_core::plan::Diff {
    let summary = summary.into();
    if ever_sensitive(cx) {
        return buckle_core::plan::Diff::Update {
            summary: format!(
                "{summary}: content changed ({}); the current content is not shown because the file has held secrets",
                digest_change(&Digest::of(old.as_bytes()), &Digest::of(new.as_bytes()))
            ),
            unified: None,
        };
    }
    if old.len() <= SHADOW_LIMIT && new.len() <= SHADOW_LIMIT {
        redacted_unified(cx, summary, old, new, diff_context(cx, path))
    } else {
        buckle_core::plan::Diff::Update {
            summary,
            unified: None,
        }
    }
}

/// Where `insert_after` / `insert_before` place new lines: the index to insert
/// at. `after` and `before` are regular expressions, or the literals `EOF` and
/// `BOF`; with neither, lines go at the end. A pattern that matches nothing
/// falls back to the end of the file. The last match wins, as in Ansible.
pub fn insertion_index(
    lines: &[String],
    after: Option<&str>,
    before: Option<&str>,
) -> Result<usize> {
    if let Some(pattern) = after {
        if pattern == "EOF" {
            return Ok(lines.len());
        }
        let re = compile_regex(pattern, "insert_after")?;
        return Ok(lines
            .iter()
            .rposition(|l| re.is_match(l))
            .map(|i| i + 1)
            .unwrap_or(lines.len()));
    }
    if let Some(pattern) = before {
        if pattern == "BOF" {
            return Ok(0);
        }
        let re = compile_regex(pattern, "insert_before")?;
        return Ok(lines
            .iter()
            .rposition(|l| re.is_match(l))
            .unwrap_or(lines.len()));
    }
    Ok(lines.len())
}

/// How long a download may take before it is abandoned.
pub const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Resolves request headers whose values may reference secrets
/// (`Authorization: "Bearer {{ secrets.token }}"`) into the pairs to send.
///
/// buckle-core has no HTTP sink yet, so a sensitive value is opened here with
/// the transport sink's witness: an HTTP request header is a transport of the
/// secret to the server that needs it. This is the only place in buckle-std
/// that does so; the resolved pairs go straight into the request and are
/// never logged (requests are not formatted anywhere in this crate).
pub fn resolve_headers(
    cx: &Cx,
    headers: &std::collections::BTreeMap<String, buckle_core::secrets::SecretField>,
) -> Result<Vec<(String, String)>> {
    use buckle_core::secrets::Rendered;
    use buckle_core::secrets::sinks::{Sink, TransportSink};
    headers
        .iter()
        .map(|(name, value)| {
            let value = match value.resolve(&cx.templater, &cx.host)? {
                Rendered::Plain(text) => text,
                Rendered::Sensitive(secret) => secret.expose(TransportSink.witness()).clone(),
            };
            Ok((name.clone(), value))
        })
        .collect()
}

/// GETs `url` and returns the body, turning transport and status failures into
/// user errors that name the URL.
pub async fn http_get(cx: &Cx, url: &str, headers: &[(String, String)]) -> Result<Vec<u8>> {
    let mut request = buckle_core::system::HttpRequest::get(url).timeout(DOWNLOAD_TIMEOUT);
    for (name, value) in headers {
        request = request.header(name.clone(), value.clone());
    }
    let response = cx.system.http(request).await?;
    if (200..300).contains(&response.status) {
        return Ok(response.body);
    }
    Err(error::user(
        format!("GET {url} returned HTTP {}.", response.status),
        &[
            "Check the URL, and any credentials the server needs.",
            "For GitHub, a rate-limited or private repository needs a token (see `token_env` on github.release).",
        ],
    ))
}

/// Whether `s` looks like a SHA-256 digest in hex.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Extracts a SHA-256 from the text of a `.sha256` or `SHA256SUMS` file.
///
/// A single-line file may hold a bare digest or `digest  name`. A multi-line
/// file is searched for the line naming one of `names` (a leading `*` or `./`
/// on the name is ignored).
pub fn parse_sha256_text(text: &str, names: &[&str]) -> Option<String> {
    let entries: Vec<(&str, Option<&str>)> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let digest = parts.next()?;
            let name = parts
                .next()
                .map(|n| n.trim_start_matches('*').trim_start_matches("./"));
            is_sha256_hex(digest).then_some((digest, name))
        })
        .collect();
    if let [(digest, _)] = entries.as_slice() {
        return Some(digest.to_ascii_lowercase());
    }
    entries
        .iter()
        .find(|(_, name)| name.is_some_and(|n| names.contains(&n)))
        .map(|(digest, _)| digest.to_ascii_lowercase())
}

/// The last path segment of a URL, without query or fragment.
pub fn url_basename(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// The host's CPU architecture as `uname -m` spells it (`x86_64`, `aarch64`,
/// `armv7l`). Taken from the `hardware` facts when present, then `uname -m`,
/// then the architecture Buckle itself was built for.
pub async fn host_arch(cx: &Cx) -> String {
    for path in ["hardware.arch", "arch"] {
        if let Some(arch) = buckle_core::selectors::lookup_path(&cx.host.facts, path)
            .and_then(|v| v.as_str())
            .filter(|a| !a.is_empty())
        {
            return arch.to_owned();
        }
    }
    if let Ok(out) = cx.system.run(Command::new("uname").arg("-m")).await
        && out.success()
    {
        let arch = out.stdout_str().trim().to_owned();
        if !arch.is_empty() {
            return arch;
        }
    }
    std::env::consts::ARCH.to_owned()
}

/// The first version-looking token in `text` (`1.2.3`, `v1.2.3-rc.1`), without any leading `v`.
pub fn find_version(text: &str) -> Option<String> {
    static VERSION: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"v?(\d+\.\d+(?:\.\d+)?(?:[-+][0-9A-Za-z.\-+]+)?)")
            .expect("the version pattern is valid")
    });
    VERSION
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
}

/// `Planned::change` of [`text_diff`].
pub fn text_diff_planned(
    cx: &Cx,
    path: &Path,
    summary: impl Into<String>,
    old: &str,
    new: &str,
) -> Planned {
    Planned::change(text_diff(cx, path, summary, old, new))
}

#[cfg(test)]
mod registry_tests {
    use buckle_core::task::{lookup, registry};
    use serde_json::json;

    fn samples() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            (
                "file.copy",
                json!({"src": "a", "dest": "/b", "mode": "0644", "owner": "root", "group": "root", "backup": true}),
            ),
            (
                "file.template",
                json!({"src": "a.j2", "dest": "/b", "mode": "0600", "validate": ["x", "{path}"]}),
            ),
            (
                "file.tree",
                json!({"src": "d", "dest": "/d", "mode": "0640", "dir_mode": "0750", "owner": "root", "templates": true, "exclude": ["*.md"], "purge": true}),
            ),
            (
                "file.directory",
                json!({"path": "/d", "mode": "0750", "recurse": true}),
            ),
            ("file.absent", json!({"path": "/d", "recursive": true})),
            (
                "file.symlink",
                json!({"path": "/l", "target": "/t", "force": true}),
            ),
            (
                "file.line",
                json!({"path": "/f", "line": "x", "regex": "^x", "insert_after": "EOF", "create": true}),
            ),
            (
                "file.block",
                json!({"path": "/f", "block": "x\ny", "marker": "# {mark} x", "state": "absent"}),
            ),
            (
                "file.download",
                json!({"url": "https://x/y", "dest": "/y", "checksum": "sha256:abc", "headers": {"A": "b"}, "force": true}),
            ),
            (
                "archive.extract",
                json!({"src": "https://x/y.tgz", "dest": "/o", "strip_components": 1, "include": ["a"], "creates": "a", "mode": "0755"}),
            ),
            (
                "github.release",
                json!({"repo": "o/n", "asset": "a-{arch}", "dest": "/b", "arch_map": {"armv7l": "armv7"}, "probe": ["b", "-V"], "token_env": "T"}),
            ),
            (
                "command.run",
                json!({"run": "echo hi", "unless": ["true"], "changed_when": "exit:2", "env": {"A": "{{ secrets.s }}"}, "timeout": "5m", "capture": "never"}),
            ),
            (
                "user.account",
                json!({"name": "u", "groups": ["g"], "password": "{{ secrets.p }}", "create_home": false}),
            ),
            (
                "user.group",
                json!({"name": "g", "gid": 10, "system": true}),
            ),
            (
                "ssh.authorized_key",
                json!({"user": "u", "key": "ssh-ed25519 AAAA c", "exclusive": true}),
            ),
            ("hostname.set", json!({"name": "h"})),
            (
                "swap.file",
                json!({"path": "/s", "size_mb": 10, "swappiness": 5}),
            ),
            (
                "cron.job",
                json!({"name": "j", "schedule": "@daily", "command": "x"}),
            ),
            ("docker.image", json!({"name": "i:1", "pull": "always"})),
            (
                "git.repository",
                json!({"repo": "https://g.example/r.git", "path": "/r", "ref": "main", "owner": "u", "group": "g", "depth": 1, "recursive": true, "update": false}),
            ),
        ]
    }

    #[test]
    fn every_kind_is_registered_parses_its_documented_fields_and_has_a_schema() {
        for (kind, raw) in samples() {
            let task = lookup(kind).unwrap_or_else(|e| panic!("{kind}: {}", e.description()));
            let spec = task
                .parse(raw)
                .unwrap_or_else(|e| panic!("{kind}: {}", e.description()));
            assert!(!task.key(&spec).is_empty(), "{kind} has a key");
            assert!(!task.describe(&spec).is_empty(), "{kind} has a description");
            assert!(!task.description().is_empty(), "{kind} documents itself");
            let schema = serde_json::to_value(task.schema()).unwrap();
            assert!(schema["properties"].is_object(), "{kind} has a schema");
            assert!(registry().contains_key(kind));
        }
    }

    /// Minimal specs for the kinds whose samples live in `sys_exec`'s tests.
    fn system_samples() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            (
                "apt.repository",
                json!({"name": "x", "uri": "https://x.example", "suites": ["s"]}),
            ),
            ("apt.package", json!({"name": "nginx"})),
            ("apt.update", json!({})),
            ("systemd.unit", json!({"name": "nomad", "enabled": true})),
            (
                "systemd.dropin",
                json!({"unit": "nomad.service", "content": "[Service]\n"}),
            ),
            ("systemd.daemon_reload", json!({})),
            ("systemd.restart", json!({"name": "nomad"})),
            ("systemd.reload", json!({"name": "nomad"})),
            (
                "sysctl.param",
                json!({"name": "vm.swappiness", "value": 10}),
            ),
            (
                "ufw.default",
                json!({"direction": "incoming", "policy": "deny"}),
            ),
            ("ufw.rule", json!({"rule": "allow", "to_port": 22})),
            ("ufw.state", json!({"enabled": true})),
        ]
    }

    #[test]
    fn every_kind_rejects_an_unknown_field_naming_it() {
        let all: Vec<_> = samples().into_iter().chain(system_samples()).collect();
        let mut covered: Vec<&str> = all.iter().map(|(k, _)| *k).collect();
        covered.sort_unstable();
        let mut registered: Vec<&str> = registry()
            .keys()
            .copied()
            .filter(|k| !k.starts_with("test."))
            .collect();
        registered.sort_unstable();
        assert_eq!(covered, registered, "every registered kind has a sample");
        for (kind, mut raw) in all {
            let task = lookup(kind).unwrap();
            task.parse(raw.clone())
                .unwrap_or_else(|e| panic!("{kind}: the sample parses: {}", e.description()));
            raw.as_object_mut()
                .unwrap()
                .insert("bogus_field".into(), json!("x"));
            let err = match task.parse(raw) {
                Ok(_) => panic!("{kind} accepted an unknown field"),
                Err(e) => e,
            };
            let text = format!("{} {err:?}", err.description());
            assert!(text.contains(kind), "{kind}: {text}");
            assert!(text.contains("bogus_field"), "{kind}: {text}");
        }
    }

    #[test]
    fn a_typo_in_a_flattened_permission_field_is_rejected() {
        for kind in [
            "file.copy",
            "file.template",
            "file.directory",
            "file.tree",
            "file.download",
            "archive.extract",
            "github.release",
        ] {
            let (_, mut raw) = samples().into_iter().find(|(k, _)| *k == kind).unwrap();
            raw.as_object_mut()
                .unwrap()
                .insert("onwer".into(), json!("root"));
            assert!(
                lookup(kind).unwrap().parse(raw).is_err(),
                "{kind} accepted `onwer`"
            );
        }
    }

    /// Planning on a host without the tool a kind shells out to is Blocked
    /// with advice, never a plan error (a minimal container image).
    #[rstest::rstest]
    #[case("apt.package", json!({"name": "nginx"}))]
    #[case("apt.package", json!({"name": "nginx", "state": "absent"}))]
    #[case("apt.update", json!({}))]
    #[case("apt.repository", json!({"name": "x", "uri": "https://x.example", "suites": ["s"]}))]
    #[case("systemd.unit", json!({"name": "nomad", "enabled": true}))]
    #[case("systemd.dropin", json!({"unit": "nomad.service", "content": "[Service]\n"}))]
    #[case("systemd.daemon_reload", json!({}))]
    #[case("systemd.restart", json!({"name": "nomad"}))]
    #[case("systemd.reload", json!({"name": "nomad"}))]
    #[case("docker.image", json!({"name": "i:1"}))]
    #[case("git.repository", json!({"repo": "https://g.example/r.git", "path": "/r"}))]
    #[case("hostname.set", json!({"name": "web-1"}))]
    #[case("swap.file", json!({"path": "/swapfile", "size_mb": 10}))]
    #[case("user.account", json!({"name": "u"}))]
    #[case("user.group", json!({"name": "g"}))]
    #[tokio::test]
    async fn a_missing_tool_at_plan_time_is_blocked_not_an_error(
        #[case] kind: &str,
        #[case] raw: serde_json::Value,
    ) {
        let sys = buckle_core::testing::FakeSystem::new().with_hostname("old");
        let cx = super::testkit::cx(&sys);
        let task = lookup(kind).unwrap();
        let spec = task.parse(raw).unwrap();
        match task.plan(&cx, &spec).await {
            Ok(buckle_core::plan::Planned::Blocked { reason }) => {
                assert!(!reason.is_empty(), "{kind}")
            }
            other => panic!("{kind}: expected Blocked, got {other:?}"),
        }
        assert!(
            sys.commands_run().is_empty(),
            "{kind} ran a command it does not have"
        );
    }

    /// Kinds that need no tool to plan still plan without one.
    #[rstest::rstest]
    #[case("sysctl.param", json!({"name": "vm.swappiness", "value": 10}))]
    #[case("cron.job", json!({"name": "j", "schedule": "@daily", "command": "x"}))]
    #[case("ufw.rule", json!({"rule": "allow", "to_port": 22}))]
    #[case("command.run", json!({"run": ["x"], "unless": ["missing-probe"]}))]
    #[tokio::test]
    async fn kinds_that_need_no_tool_to_plan_do_not_fail_without_one(
        #[case] kind: &str,
        #[case] raw: serde_json::Value,
    ) {
        let sys = buckle_core::testing::FakeSystem::new();
        let cx = super::testkit::cx(&sys);
        let task = lookup(kind).unwrap();
        let spec = task.parse(raw).unwrap();
        task.plan(&cx, &spec)
            .await
            .unwrap_or_else(|e| panic!("{kind}: {}", e.description()));
    }

    #[test]
    fn locks_platforms_and_cautions_match_the_catalogue() {
        let get = |k| lookup(k).unwrap();
        assert_eq!(get("swap.file").locks(), &["fstab"]);
        assert_eq!(get("docker.image").locks(), &["docker"]);
        for kind in ["user.account", "user.group"] {
            assert!(get(kind).removal_caution(), "{kind}");
            assert!(get(kind).platforms().contains(buckle_core::Platform::Linux));
            assert!(!get(kind).platforms().contains(buckle_core::Platform::MacOs));
        }
        assert!(!get("docker.image").removal_caution());
        assert!(
            !get("swap.file")
                .platforms()
                .contains(buckle_core::Platform::Windows)
        );
        for (kind, _) in samples() {
            assert_eq!(
                get(kind).removal_default(),
                buckle_core::model::RemovalPolicy::Cleanup,
                "{kind}"
            );
        }
    }

    #[test]
    fn secret_capable_fields_are_marked_in_the_schema() {
        for (kind, pointer) in [
            ("command.run", "/properties/env/additionalProperties"),
            ("user.account", "/properties/password"),
            ("file.download", "/properties/headers/additionalProperties"),
            ("apt.repository", "/$defs/Auth/properties/password"),
        ] {
            let schema = serde_json::to_string(&lookup(kind).unwrap().schema()).unwrap();
            assert!(schema.contains("x-buckle-secret"), "{kind}: {pointer}");
        }
    }
}
