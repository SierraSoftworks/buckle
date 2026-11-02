//! `archive.extract`: unpack a tar, tar.gz or zip archive onto the host.

use super::hostkit::{self, describe_perms};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{FileKind, FilePerms};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use tracing::instrument;

/// Extracts a `.tar.gz`, `.tgz`, `.tar` or `.zip` archive into `dest`.
///
/// ```yaml
/// - archive.extract:
///     src: https://example.com/tool-1.2.3-linux-amd64.tar.gz   # URL, absolute host path, or module file
///     dest: /opt/tool
///     strip_components: 1              # drop the archive's top-level directory
///     include: ["tool", "LICENSE"]     # optional; exact names, `*` globs, or directory prefixes
///     creates: /opt/tool/tool          # skip the task when this exists
///     owner: root
///     group: root
///     mode: "0755"                     # applied to extracted files (not directories)
/// ```
///
/// `src` is fetched over HTTP(S) when it starts with `http://` or `https://`,
/// read from the host when it is an absolute path, and otherwise read from the
/// module's `files/` directory. The archive format is detected from its
/// content, so unusual file names are fine.
///
/// Without `creates`, the plan compares every file in the archive with `dest`
/// and is a no-op only when they all match; for a URL that means downloading
/// the archive on every plan, so give remote archives a `creates:` path (which
/// may be relative to `dest`) to skip the work. `creates` must be produced by
/// the archive, or the task fails after extracting.
///
/// Entries that would land outside `dest` (absolute paths, `..`) are rejected,
/// as is writing beneath a symlink the archive itself created. The archive is
/// held in memory and may not expand beyond 1 GiB. Files keep the archive's
/// modes unless `mode` is set. A symlink entry never replaces an existing
/// file or directory (or a symlink Buckle did not create) unless
/// `force: true`; the plan is blocked instead.
///
/// Removal deletes the files Buckle extracted, and the directories it created
/// if they are then empty, but only if every file is still exactly as
/// extracted; otherwise it leaves everything in place. Files that were
/// already in `dest` before Buckle first extracted over them are not Buckle's
/// and are never deleted.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    /// URL, absolute host path, or path relative to the module's `files/` directory.
    pub src: String,
    /// Absolute directory to extract into (created if missing).
    pub dest: PathBuf,
    /// Number of leading path components to drop from every entry.
    #[serde(default)]
    pub strip_components: u32,
    /// Only extract entries matching one of these names, `*` globs or directory prefixes.
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(flatten)]
    pub perms: FilePerms,
    /// Skip the task when this path exists (relative paths are relative to `dest`).
    #[serde(default)]
    pub creates: Option<String>,
    /// Let symlink entries replace existing files, directories or foreign symlinks.
    #[serde(default)]
    pub force: bool,
}

/// Paths this resource extracted on earlier applies.
fn owned_paths(cx: &Cx) -> BTreeSet<PathBuf> {
    hostkit::ownership::previous_artefacts(cx)
        .iter()
        .filter_map(|a| match a {
            Artefact::File { path } | Artefact::Symlink { path } | Artefact::Directory { path } => {
                Some(path.clone())
            }
            _ => None,
        })
        .collect()
}

pub struct ArchiveExtract;
buckle_core::register_task!(ArchiveExtract);

/// The most an archive may expand to.
const MAX_UNPACKED: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File,
    Dir,
    Symlink(PathBuf),
}

/// One archive member, with its path made safe and relative.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    /// The path inside the archive, before `strip_components`.
    pub original: PathBuf,
    pub kind: EntryKind,
    pub mode: u32,
    pub data: Vec<u8>,
}

fn unsafe_path(name: &str) -> buckle_core::Error {
    error::user(
        format!(
            "The archive contains an entry '{name}' that would be written outside the destination."
        ),
        &[
            "Only extract archives from sources you trust; Buckle refuses absolute paths and `..` components.",
        ],
    )
}

/// Normalises an archive path: drops `.` components and rejects anything that could escape.
fn safe_path(name: &str) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in Path::new(name).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return Err(unsafe_path(name)),
        }
    }
    Ok(out)
}

fn too_big() -> buckle_core::Error {
    error::user(
        "The archive expands to more than 1 GiB, which Buckle refuses to hold in memory.",
        &["Extract very large archives with a command.run task instead."],
    )
}

fn read_limited(reader: &mut impl Read, budget: &mut u64) -> Result<Vec<u8>> {
    use buckle_core::error::HumanizableResult;
    let mut data = Vec::new();
    reader
        .take(*budget + 1)
        .read_to_end(&mut data)
        .to_human_error()?;
    if data.len() as u64 > *budget {
        return Err(too_big());
    }
    *budget -= data.len() as u64;
    Ok(data)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    TarGz,
    Tar,
    Zip,
}

fn detect(bytes: &[u8], name_hint: &str) -> Result<Format> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        return Ok(Format::TarGz);
    }
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") {
        return Ok(Format::Zip);
    }
    if bytes.len() > 262 && &bytes[257..262] == b"ustar" {
        return Ok(Format::Tar);
    }
    let lower = name_hint.to_ascii_lowercase();
    if lower.ends_with(".tar") {
        return Ok(Format::Tar);
    }
    Err(error::user(
        format!(
            "'{name_hint}' is not a recognised archive (expected .tar.gz, .tgz, .tar or .zip)."
        ),
        &["Check that the download is the archive itself and not an error page."],
    ))
}

/// Parses an archive held in memory into its entries.
pub(crate) fn read_archive(bytes: &[u8], name_hint: &str) -> Result<Vec<Entry>> {
    match detect(bytes, name_hint)? {
        Format::TarGz => read_tar(flate2::read::GzDecoder::new(bytes)),
        Format::Tar => read_tar(bytes),
        Format::Zip => read_zip(bytes),
    }
}

fn read_tar(reader: impl Read) -> Result<Vec<Entry>> {
    use buckle_core::error::HumanizableResult;
    let mut archive = tar::Archive::new(reader);
    let mut budget = MAX_UNPACKED;
    let mut entries = Vec::new();
    for entry in archive.entries().to_human_error()? {
        let mut entry = entry.to_human_error()?;
        let raw_path = entry
            .path()
            .to_human_error()?
            .to_string_lossy()
            .into_owned();
        let kind = match entry.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => EntryKind::File,
            tar::EntryType::Directory => EntryKind::Dir,
            tar::EntryType::Symlink => {
                let target = entry
                    .link_name()
                    .to_human_error()?
                    .map(|t| t.into_owned())
                    .unwrap_or_default();
                EntryKind::Symlink(target)
            }
            // Hard links, devices, pax headers and the like are not extracted.
            _ => continue,
        };
        let path = safe_path(&raw_path)?;
        if path.as_os_str().is_empty() {
            continue;
        }
        let mode = entry.header().mode().unwrap_or(0o644) & 0o7777;
        let data = if kind == EntryKind::File {
            read_limited(&mut entry, &mut budget)?
        } else {
            Vec::new()
        };
        entries.push(Entry {
            original: path,
            kind,
            mode,
            data,
        });
    }
    Ok(entries)
}

fn read_zip(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| {
        error::wrap_user(
            e,
            "The zip archive could not be read.",
            &["Check that the download is a complete, valid zip file."],
        )
    })?;
    let mut budget = MAX_UNPACKED;
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|e| {
            error::wrap_user(
                e,
                "A zip entry could not be read.",
                &["Check that the archive is complete and not encrypted."],
            )
        })?;
        let name = String::from_utf8_lossy(file.name_raw()).into_owned();
        let path = file.enclosed_name().ok_or_else(|| unsafe_path(&name))?;
        let path = safe_path(&path.to_string_lossy())?;
        if path.as_os_str().is_empty() {
            continue;
        }
        let unix_mode = file.unix_mode();
        let is_symlink = file.is_symlink();
        let kind = if file.is_dir() {
            EntryKind::Dir
        } else if is_symlink {
            let data = read_limited(&mut file, &mut budget)?;
            entries.push(Entry {
                original: path,
                kind: EntryKind::Symlink(PathBuf::from(
                    String::from_utf8_lossy(&data).into_owned(),
                )),
                mode: 0o777,
                data: Vec::new(),
            });
            continue;
        } else {
            EntryKind::File
        };
        let data = if kind == EntryKind::File {
            read_limited(&mut file, &mut budget)?
        } else {
            Vec::new()
        };
        entries.push(Entry {
            original: path,
            kind,
            mode: unix_mode.map(|m| m & 0o7777).unwrap_or(0o644),
            data,
        });
    }
    Ok(entries)
}

/// Whether `path` is selected by an `include` pattern: an exact name, a `*`/`?`
/// glob, or a directory prefix.
fn included(patterns: &[String], original: &Path, stripped: &Path) -> bool {
    if patterns.is_empty() {
        return true;
    }
    // Archive entry names always use `/`; `stripped` was rebuilt from components, so on
    // Windows it carries `\` and must be normalised before matching.
    let candidates = [
        original.to_string_lossy().replace('\\', "/"),
        stripped.to_string_lossy().replace('\\', "/"),
    ];
    patterns.iter().any(|pattern| {
        let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
        candidates.iter().any(|c| {
            glob_match(pattern, c)
                || c.strip_prefix(pattern)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    })
}

/// A minimal glob: `*` matches any run of characters, `?` any single one.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Applies `strip_components` and `include`, returning entries with their final relative paths.
pub(crate) fn select(
    entries: Vec<Entry>,
    strip_components: u32,
    include: &[String],
) -> Result<Vec<(PathBuf, Entry)>> {
    let mut out: Vec<(PathBuf, Entry)> = Vec::new();
    let mut symlinks: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in entries {
        let stripped: PathBuf = entry
            .original
            .components()
            .skip(strip_components as usize)
            .collect();
        if stripped.as_os_str().is_empty() || !included(include, &entry.original, &stripped) {
            continue;
        }
        if stripped.ancestors().skip(1).any(|a| symlinks.contains(a)) {
            return Err(unsafe_path(&entry.original.to_string_lossy()));
        }
        if matches!(entry.kind, EntryKind::Symlink(_)) {
            symlinks.insert(stripped.clone());
        }
        out.push((stripped, entry));
    }
    Ok(out)
}

/// Digest over everything extracted, so removal can tell whether anything changed.
fn manifest(items: impl Iterator<Item = (String, String)>) -> Digest {
    let mut lines: Vec<String> = items.map(|(k, v)| format!("{k}\0{v}")).collect();
    lines.sort();
    Digest::of(lines.join("\n").as_bytes())
}

fn entry_value(entry: &Entry) -> (String, String) {
    match &entry.kind {
        EntryKind::File => ("f".into(), Digest::of(&entry.data).as_str().to_owned()),
        EntryKind::Symlink(target) => ("l".into(), target.to_string_lossy().into_owned()),
        EntryKind::Dir => ("d".into(), String::new()),
    }
}

enum SrcKind<'a> {
    Url(&'a str),
    Host(&'a Path),
    Module(&'a str),
}

fn classify(src: &str) -> SrcKind<'_> {
    if src.starts_with("http://") || src.starts_with("https://") {
        SrcKind::Url(src)
    } else if src.starts_with('/') {
        SrcKind::Host(Path::new(src))
    } else {
        SrcKind::Module(src)
    }
}

async fn fetch(cx: &Cx, src: &str) -> Result<Vec<u8>> {
    match classify(src) {
        SrcKind::Url(url) => hostkit::http_get(cx, url, &[]).await,
        SrcKind::Host(path) => cx.system.read(path).await?.ok_or_else(|| {
            error::user(
                format!(
                    "The archive {} does not exist on this host.",
                    path.display()
                ),
                &["Download it in an earlier task, or fix `src`."],
            )
        }),
        SrcKind::Module(rel) => hostkit::read_module_file(cx, rel).await,
    }
}

fn creates_path(spec: &Spec) -> Option<PathBuf> {
    spec.creates.as_ref().map(|c| {
        let p = Path::new(c);
        if p.is_absolute() || p.has_root() {
            p.to_path_buf()
        } else {
            spec.dest.join(p)
        }
    })
}

struct Inspection {
    planned: Planned,
    items: Vec<(PathBuf, Entry)>,
    dest_exists: bool,
}

impl ArchiveExtract {
    async fn inspect(&self, cx: &Cx, spec: &Spec) -> Result<Inspection> {
        hostkit::require_absolute(&spec.dest, "dest")?;
        let dest = spec.dest.display();
        let creates = creates_path(spec);
        if let Some(path) = &creates
            && cx.system.stat(path).await?.is_some()
        {
            return Ok(Inspection {
                planned: Planned::NoOp,
                items: Vec::new(),
                dest_exists: true,
            });
        }
        let dest_exists = cx.system.stat(&spec.dest).await?.is_some();
        if creates.is_some() {
            // The archive is only fetched when the task runs.
            return Ok(Inspection {
                planned: Planned::change(Diff::Create {
                    summary: format!("extract {} into {dest}", spec.src),
                    preview: None,
                }),
                items: Vec::new(),
                dest_exists,
            });
        }
        let bytes = fetch(cx, &spec.src).await?;
        let entries = read_archive(&bytes, &hostkit::url_basename(&spec.src))?;
        let items = select(entries, spec.strip_components, &spec.include)?;
        let (mut new, mut changed) = (0usize, 0usize);
        let owned = owned_paths(cx);
        let mut clobbered = Vec::new();
        for (rel, entry) in &items {
            let target = spec.dest.join(rel);
            match (&entry.kind, cx.system.stat(&target).await?) {
                (EntryKind::Dir, None) => new += 1,
                (EntryKind::Dir, Some(_)) => {}
                (_, None) => new += 1,
                (EntryKind::File, Some(meta)) => {
                    let same = meta.kind == FileKind::File
                        && cx
                            .system
                            .read(&target)
                            .await?
                            .is_some_and(|c| c == entry.data)
                        && hostkit::perm_changes(
                            &meta,
                            &FilePerms {
                                mode: spec.perms.mode,
                                ..Default::default()
                            },
                        )
                        .is_empty();
                    if !same {
                        changed += 1;
                    }
                }
                (EntryKind::Symlink(link), Some(meta)) => {
                    let current = cx.system.read_link(&target).await?;
                    if current.as_deref() != Some(link.as_path()) {
                        changed += 1;
                        let ours = meta.kind == FileKind::Symlink && owned.contains(&target);
                        if !ours && !spec.force {
                            clobbered.push(target.display().to_string());
                        }
                    }
                }
            }
        }
        let planned = if !clobbered.is_empty() {
            Planned::blocked(format!(
                "the archive has symlinks that would replace existing paths Buckle did not create ({}); set `force: true` to replace them",
                clobbered.join(", ")
            ))
        } else if new + changed == 0 {
            Planned::NoOp
        } else {
            let summary = format!(
                "extract {} into {dest} ({} entries: {new} new, {changed} changed)",
                spec.src,
                items.len()
            );
            if dest_exists {
                Planned::change(Diff::Update {
                    summary,
                    unified: None,
                })
            } else {
                Planned::change(Diff::Create {
                    summary,
                    preview: None,
                })
            }
        };
        Ok(Inspection {
            planned,
            items,
            dest_exists,
        })
    }
}

#[async_trait]
impl Task for ArchiveExtract {
    type Spec = Spec;
    const KIND: &'static str = "archive.extract";
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "Extract a tar, tar.gz or zip archive from a URL, host path or module file into a directory.";

    fn key(spec: &Spec) -> String {
        spec.dest.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        let perms = describe_perms(&spec.perms);
        if perms.is_empty() {
            format!("{} into {}", spec.src, spec.dest.display())
        } else {
            format!("{} into {} ({perms})", spec.src, spec.dest.display())
        }
    }

    #[instrument(name = "task.archive.extract.plan", skip_all, fields(src = %spec.src, dest = %spec.dest.display()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(self.inspect(cx, spec).await?.planned)
    }

    #[instrument(name = "task.archive.extract.apply", skip_all, fields(src = %spec.src, dest = %spec.dest.display()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let mut inspection = self.inspect(cx, spec).await?;
        if let Planned::Blocked { reason } = &inspection.planned {
            return Err(error::user(
                format!(
                    "Nothing was extracted into {}: {reason}.",
                    spec.dest.display()
                ),
                &["Remove the conflicting paths, or set `force: true`."],
            ));
        }
        if !inspection.planned.is_change() {
            return Ok(hostkit::ownership::carry_forward(
                cx,
                Applied::unchanged(format!("{} already extracted", spec.dest.display())),
            ));
        }
        let owned = owned_paths(cx);
        if inspection.items.is_empty() {
            // `creates` was given, so planning did not read the archive.
            let bytes = fetch(cx, &spec.src).await?;
            let entries = read_archive(&bytes, &hostkit::url_basename(&spec.src))?;
            inspection.items = select(entries, spec.strip_components, &spec.include)?;
        }
        if inspection.items.is_empty() {
            return Err(error::user(
                format!(
                    "Nothing in {} matched `include` / `strip_components`.",
                    spec.src
                ),
                &["List the archive's contents (`tar tf`) and adjust the filters."],
            ));
        }
        let dir_perms = FilePerms {
            mode: None,
            owner: spec.perms.owner.clone(),
            group: spec.perms.group.clone(),
        };
        let mut applied = Applied::changed(format!(
            "extracted {} entries from {} into {}",
            inspection.items.len(),
            spec.src,
            spec.dest.display()
        ));
        if !inspection.dest_exists {
            cx.system.mkdir_all(&spec.dest, &dir_perms).await?;
            applied = applied.with_artefact(Artefact::Directory {
                path: spec.dest.clone(),
            });
        } else if owned.contains(&spec.dest) {
            applied = applied.with_artefact(Artefact::Directory {
                path: spec.dest.clone(),
            });
        }
        let mut manifest_items = Vec::new();
        let mut created_dirs: BTreeSet<PathBuf> = BTreeSet::new();
        for (rel, entry) in &inspection.items {
            let target = spec.dest.join(rel);
            // Directories that writing this entry will create implicitly are Buckle's to clean up.
            let mut implicit: Vec<PathBuf> = target
                .ancestors()
                .skip(1)
                .take_while(|a| *a != spec.dest.as_path())
                .map(Path::to_path_buf)
                .collect();
            implicit.reverse();
            for dir in implicit {
                if !created_dirs.contains(&dir)
                    && (owned.contains(&dir) || cx.system.stat(&dir).await?.is_none())
                {
                    created_dirs.insert(dir.clone());
                    applied = applied.with_artefact(Artefact::Directory { path: dir });
                }
            }
            // Whether this resource may claim the entry: it creates it now, or
            // created it on an earlier apply. A pre-existing file it overwrites
            // is not Buckle's, so removal never deletes it.
            let existed = cx.system.stat(&target).await?;
            let claim = existed.is_none() || owned.contains(&target);
            match &entry.kind {
                EntryKind::Dir => {
                    let existed = existed.is_some() && !owned.contains(&target);
                    let perms = FilePerms {
                        mode: Some(if entry.mode == 0 { 0o755 } else { entry.mode }),
                        ..dir_perms.clone()
                    };
                    cx.system.mkdir_all(&target, &perms).await?;
                    if !existed && created_dirs.insert(target.clone()) {
                        applied = applied.with_artefact(Artefact::Directory {
                            path: target.clone(),
                        });
                    }
                }
                EntryKind::File => {
                    let perms = FilePerms {
                        mode: Some(spec.perms.mode.unwrap_or(if entry.mode == 0 {
                            0o644
                        } else {
                            entry.mode
                        })),
                        ..dir_perms.clone()
                    };
                    cx.system.write_atomic(&target, &entry.data, &perms).await?;
                    if claim {
                        applied = applied.with_artefact(hostkit::file_artefact(&target));
                    }
                }
                EntryKind::Symlink(link) => {
                    let replace =
                        cx.system.read_link(&target).await?.as_deref() != Some(link.as_path());
                    if replace {
                        if existed.is_some() {
                            cx.system.remove(&target, false).await?;
                        }
                        cx.system.symlink(link, &target).await?;
                    }
                    if claim || replace {
                        // A link Buckle made (also over something else, with `force`) is Buckle's.
                        applied = applied.with_artefact(Artefact::Symlink {
                            path: target.clone(),
                        });
                    }
                }
            }
            // The manifest covers only what Buckle owns, which is what removal checks.
            if entry.kind != EntryKind::Dir
                && applied.artefacts.iter().any(|a| {
                    matches!(a, Artefact::File { path } | Artefact::Symlink { path } if *path == target)
                })
            {
                let (tag, value) = entry_value(entry);
                manifest_items.push((format!("{tag} {}", target.display()), value));
            }
        }
        applied.content_digest = Some(manifest(manifest_items.into_iter()));
        if let Some(path) = creates_path(spec)
            && cx.system.stat(&path).await?.is_none()
        {
            return Err(error::user(
                format!(
                    "The archive was extracted but did not produce {}, the path given as `creates`.",
                    path.display()
                ),
                &[
                    "Check `creates` against the archive's layout, including `strip_components`.",
                    "Without a matching `creates`, the task would extract again on every run.",
                ],
            ));
        }
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        let files = record
            .artefacts
            .iter()
            .filter(|a| matches!(a, Artefact::File { .. } | Artefact::Symlink { .. }))
            .count();
        format!(
            "delete the {files} extracted files under {} if none has changed",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let mut current = Vec::new();
        let mut files = Vec::new();
        let mut links = Vec::new();
        let mut dirs = Vec::new();
        for artefact in &record.artefacts {
            match artefact {
                Artefact::File { path } => files.push(path.clone()),
                Artefact::Symlink { path } => links.push(path.clone()),
                Artefact::Directory { path } => dirs.push(path.clone()),
                _ => {}
            }
        }
        for path in &files {
            match cx.system.read(path).await? {
                Some(bytes) => current.push((
                    format!("f {}", path.display()),
                    Digest::of(&bytes).as_str().to_owned(),
                )),
                None => return Ok(drifted(path)),
            }
        }
        for path in &links {
            match cx.system.read_link(path).await? {
                Some(target) => current.push((
                    format!("l {}", path.display()),
                    target.to_string_lossy().into_owned(),
                )),
                None => return Ok(drifted(path)),
            }
        }
        match &record.content_digest {
            Some(expected) if *expected == manifest(current.into_iter()) => {}
            _ => {
                return Ok(Applied::skipped(
                    "the extracted files changed outside Buckle, left in place",
                ));
            }
        }
        for path in files.iter().chain(links.iter()) {
            cx.system.remove(path, false).await?;
        }
        // Deepest directories first; a directory with other content stays.
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for dir in &dirs {
            if cx.system.list_dir(dir).await?.is_some_and(|e| e.is_empty()) {
                cx.system.remove(dir, false).await?;
            }
        }
        Ok(Applied::changed(format!(
            "deleted {} extracted files",
            files.len() + links.len()
        )))
    }
}

fn drifted(path: &Path) -> Applied {
    Applied::skipped(format!(
        "{} is missing or changed outside Buckle, extracted files left in place",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::testing::{FakeSystem, MemorySource};
    use rstest::rstest;
    use serde_json::json;

    const URL: &str = "https://dl.example.com/tool-1.0.tar.gz";

    fn archive() -> Vec<u8> {
        tar_gz(&[
            ("tool-1.0/", "", 0o755),
            ("tool-1.0/tool", "#!/bin/sh\necho hi\n", 0o755),
            ("tool-1.0/README", "readme", 0o644),
            ("tool-1.0/docs/guide.txt", "guide", 0o644),
        ])
    }

    fn host() -> FakeSystem {
        let body = archive();
        FakeSystem::new().on_http(URL, move |_| buckle_core::system::HttpResponse {
            status: 200,
            headers: vec![],
            body: body.clone(),
        })
    }

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"src": URL, "dest": "/opt/tool", "strip_components": 1});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    #[tokio::test]
    async fn extracts_with_strip_components_modes_and_is_idempotent() {
        let sys = host();
        let c = cx(&sys);
        let spec = s(json!({}));
        let applied = apply(&ArchiveExtract, &c, &spec).await;
        assert!(applied.changed);
        assert_eq!(
            sys.file_string("/opt/tool/tool").unwrap(),
            "#!/bin/sh\necho hi\n"
        );
        assert_eq!(
            sys.file_string("/opt/tool/docs/guide.txt").unwrap(),
            "guide"
        );
        assert!(!sys.exists("/opt/tool/tool-1.0"));
        let meta = buckle_core::system::System::stat(&sys, Path::new("/opt/tool/tool"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.mode, Some(0o755));
        let files = applied
            .artefacts
            .iter()
            .filter(|a| matches!(a, Artefact::File { .. }))
            .count();
        assert_eq!(files, 3);
        assert_eq!(plan_class(&ArchiveExtract, &c, &spec).await, Class::NoOp);

        // A modified file is noticed.
        put(&sys, "/opt/tool/README", "edited").await;
        assert_eq!(plan_class(&ArchiveExtract, &c, &spec).await, Class::Change);
    }

    #[rstest]
    #[case::exact(vec!["tool"], vec!["/opt/tool/tool"])]
    #[case::glob(vec!["*.txt"], vec!["/opt/tool/docs/guide.txt"])]
    #[case::directory_prefix(vec!["docs"], vec!["/opt/tool/docs/guide.txt"])]
    #[case::original_names_work_too(vec!["tool-1.0/README"], vec!["/opt/tool/README"])]
    #[tokio::test]
    async fn include_filters_entries(#[case] include: Vec<&str>, #[case] expected: Vec<&str>) {
        let sys = host();
        apply(&ArchiveExtract, &cx(&sys), &s(json!({"include": include}))).await;
        let files: Vec<String> = sys
            .paths()
            .into_iter()
            .filter(|p| p.starts_with("/opt/tool") && sys.file(p).is_some())
            .map(|p| p.display().to_string().replace('\\', "/"))
            .collect();
        assert_eq!(files, expected);
    }

    #[tokio::test]
    async fn creates_skips_the_task_and_avoids_fetching() {
        let sys = host().with_file("/opt/tool/tool", "x", FilePerms::default());
        let c = cx(&sys);
        let spec = s(json!({"creates": "tool"}));
        assert_eq!(plan_class(&ArchiveExtract, &c, &spec).await, Class::NoOp);
        assert!(sys.http_requests().is_empty());

        let sys = host();
        let c = cx(&sys);
        assert_eq!(plan_class(&ArchiveExtract, &c, &spec).await, Class::Change);
        assert!(
            sys.http_requests().is_empty(),
            "plan does not download when `creates` decides"
        );
        apply(&ArchiveExtract, &c, &spec).await;
        assert_eq!(plan_class(&ArchiveExtract, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn a_creates_path_the_archive_does_not_make_is_an_error() {
        let sys = host();
        let err = ArchiveExtract
            .apply(
                &cx(&sys),
                &s(json!({"creates": "nothing-like-this"})),
                &Planned::NoOp,
            )
            .await
            .unwrap_err();
        assert!(err.description().contains("creates"));
    }

    #[tokio::test]
    async fn host_paths_and_module_files_are_valid_sources_and_zip_works() {
        let zip = zip_of(&[("bin/tool", "zip content"), ("bin/other", "o")]);
        let sys = FakeSystem::new().with_file("/tmp/tool.zip", zip.clone(), FilePerms::default());
        let c = cx(&sys);
        let from_host = s(json!({"src": "/tmp/tool.zip", "strip_components": 0}));
        apply(&ArchiveExtract, &c, &from_host).await;
        assert_eq!(
            sys.file_string("/opt/tool/bin/tool").unwrap(),
            "zip content"
        );
        assert_eq!(
            plan_class(&ArchiveExtract, &c, &from_host).await,
            Class::NoOp
        );

        let sys = FakeSystem::new();
        let c = cx_src(&sys, MemorySource::new().with("dist/tool.zip", zip));
        let from_module = s(
            json!({"src": "dist/tool.zip", "strip_components": 1, "mode": "0700", "include": ["tool"]}),
        );
        apply(&ArchiveExtract, &c, &from_module).await;
        let meta = buckle_core::system::System::stat(&sys, Path::new("/opt/tool/tool"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.mode, Some(0o700), "mode overrides the archive's");
        assert!(!sys.exists("/opt/tool/other"));
    }

    #[tokio::test]
    async fn plain_tar_files_are_supported() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(1);
        h.set_mode(0o600);
        h.set_cksum();
        builder.append_data(&mut h, "a.txt", &b"a"[..]).unwrap();
        let sys = FakeSystem::new().with_file(
            "/tmp/a.tar",
            builder.into_inner().unwrap(),
            FilePerms::default(),
        );
        apply(
            &ArchiveExtract,
            &cx(&sys),
            &s(json!({"src": "/tmp/a.tar", "strip_components": 0})),
        )
        .await;
        assert_eq!(sys.file_string("/opt/tool/a.txt").unwrap(), "a");
    }

    #[rstest]
    #[case::parent_escape("../evil")]
    #[case::nested_escape("a/../../evil")]
    #[tokio::test]
    async fn path_traversal_is_rejected(#[case] name: &str) {
        // tar::Builder refuses to write such paths, so build the header by hand.
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_mode(0o644);
        let bytes = name.as_bytes();
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        header.set_cksum();
        let mut tar = header.as_bytes().to_vec();
        tar.extend_from_slice(&[b'x'; 1]);
        tar.resize(tar.len() + 511, 0);
        tar.extend_from_slice(&[0u8; 1024]);
        let sys = FakeSystem::new().with_file("/tmp/evil.tar", tar, FilePerms::default());
        let err = ArchiveExtract
            .plan(
                &cx(&sys),
                &s(json!({"src": "/tmp/evil.tar", "strip_components": 0})),
            )
            .await
            .unwrap_err();
        assert!(
            err.description().contains("outside the destination"),
            "{}",
            err.description()
        );
        assert!(!sys.exists("/evil") && !sys.exists("/opt/evil"));
    }

    #[tokio::test]
    async fn unknown_formats_and_missing_sources_are_user_errors() {
        let sys = FakeSystem::new().with_file("/tmp/x.bin", "not an archive", FilePerms::default());
        let c = cx(&sys);
        let err = ArchiveExtract
            .plan(&c, &s(json!({"src": "/tmp/x.bin"})))
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
        assert!(
            ArchiveExtract
                .plan(&c, &s(json!({"src": "/tmp/nope.tgz"})))
                .await
                .is_err()
        );
        assert!(
            ArchiveExtract
                .plan(&c, &s(json!({"src": "missing/module.tgz"})))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn removal_deletes_extracted_files_only_while_none_has_changed() {
        let sys = host();
        let c = cx(&sys);
        let rec = record(
            "archive.extract",
            "/opt/tool",
            &apply(&ArchiveExtract, &c, &s(json!({}))).await,
        );

        put(&sys, "/opt/tool/README", "edited").await;
        let out = ArchiveExtract.remove(&c, &rec).await.unwrap();
        assert!(
            !out.changed && out.summary.contains("changed outside Buckle"),
            "{}",
            out.summary
        );
        assert!(sys.exists("/opt/tool/tool"));

        put(&sys, "/opt/tool/README", "readme").await;
        put(&sys, "/opt/tool/extra-user-file", "mine").await;
        let out = ArchiveExtract.remove(&c, &rec).await.unwrap();
        assert!(out.changed);
        assert!(!sys.exists("/opt/tool/tool") && !sys.exists("/opt/tool/docs"));
        assert!(
            sys.exists("/opt/tool/extra-user-file"),
            "files Buckle did not extract stay"
        );
        assert!(
            sys.exists("/opt/tool"),
            "a directory that still has content stays"
        );
    }

    #[tokio::test]
    async fn removal_cleans_up_directories_it_created_when_empty() {
        let sys = host();
        let c = cx(&sys);
        let rec = record(
            "archive.extract",
            "/opt/tool",
            &apply(&ArchiveExtract, &c, &s(json!({}))).await,
        );
        assert!(ArchiveExtract.remove(&c, &rec).await.unwrap().changed);
        assert!(!sys.exists("/opt/tool"));
    }

    #[test]
    fn glob_matching_covers_the_basics() {
        assert!(glob_match("*.txt", "a.txt"));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("bin/*", "bin/tool"));
        assert!(!glob_match("*.txt", "a.md"));
        assert!(glob_match("*", ""));
    }

    #[tokio::test]
    async fn files_that_existed_before_are_overwritten_but_never_claimed() {
        let sys = host().with_file("/opt/tool/README", "the host's own", FilePerms::mode(0o644));
        let applied = apply(
            &ArchiveExtract,
            &cx(&sys),
            &s(json!({"strip_components": 1})),
        )
        .await;
        assert!(!applied.artefacts.contains(&Artefact::File {
            path: "/opt/tool/README".into()
        }));
        assert!(
            applied
                .artefacts
                .iter()
                .any(|a| matches!(a, Artefact::File { .. }))
        );
        let rec = record("archive.extract", "/opt/tool", &applied);
        assert!(
            ArchiveExtract
                .remove(&cx(&sys), &rec)
                .await
                .unwrap()
                .changed
        );
        assert!(
            sys.exists("/opt/tool/README"),
            "the pre-existing file stays"
        );
    }

    #[tokio::test]
    async fn a_symlink_entry_never_replaces_an_existing_file_without_force() {
        let bytes = {
            let mut builder = tar::Builder::new(Vec::new());
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            builder
                .append_link(&mut header, "current", "releases/1.0")
                .unwrap();
            let tar = builder.into_inner().unwrap();
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut gz, &tar).unwrap();
            gz.finish().unwrap()
        };
        let sys = FakeSystem::new()
            .with_file("/srv/app/current", "precious", FilePerms::default())
            .with_file("/srv/a.tgz", bytes, FilePerms::default());
        let spec_of = |force: bool| -> Spec {
            spec(json!({"src": "/srv/a.tgz", "dest": "/srv/app", "force": force}))
        };
        assert_eq!(
            plan_class(&ArchiveExtract, &cx(&sys), &spec_of(false)).await,
            Class::Blocked
        );
        assert!(
            ArchiveExtract
                .apply(&cx(&sys), &spec_of(false), &Planned::NoOp)
                .await
                .is_err()
        );
        assert_eq!(
            sys.file_string("/srv/app/current").as_deref(),
            Some("precious")
        );
        apply(&ArchiveExtract, &cx(&sys), &spec_of(true)).await;
        assert!(
            sys.file("/srv/app/current").is_none(),
            "replaced by the link"
        );
    }
}
