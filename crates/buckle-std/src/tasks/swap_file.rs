//! `swap.file`: a swap file, its fstab entry and swappiness as one resource.

use super::hostkit;
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, FilePerms};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result, error};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::instrument;

/// Creates and activates a swap file, and makes it permanent.
///
/// ```yaml
/// - swap.file:
///     path: /swapfile
///     size_mb: 2048
///     swappiness: 10        # optional; sets vm.swappiness live and persistently
/// ```
///
/// One resource owns the file, its `/etc/fstab` line and the `swapon` state:
/// the plan is a no-op when `/proc/swaps` lists the file at the right size, the
/// fstab entry exists, the file is mode 0600 and swappiness matches. Applying
/// creates the file (`fallocate`, or `dd` where that fails), sets mode 0600,
/// runs `mkswap`, adds `<path> none swap sw 0 0` to `/etc/fstab` and runs
/// `swapon`. A file of the wrong size is swapped off and recreated. Swappiness
/// is applied with `sysctl -w vm.swappiness=N` and persisted in
/// `/etc/sysctl.d/99-buckle-swap.conf`.
///
/// An existing file at `path` that is not an active swap area is only
/// recreated or formatted (`mkswap`) when Buckle created it or it already
/// carries a swap signature; anything else (a data file at the wrong path) is
/// reported as blocked and left untouched.
///
/// Removal runs `swapoff`, deletes the fstab line and the file, but only for a
/// swap file that Buckle created. It holds the `fstab` lock.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    /// Absolute path of the swap file.
    pub path: PathBuf,
    /// Size in MiB.
    pub size_mb: u64,
    #[serde(default)]
    pub swappiness: Option<u32>,
}

pub struct SwapFile;
buckle_core::register_task!(SwapFile);

const FSTAB: &str = "/etc/fstab";
const PROC_SWAPS: &str = "/proc/swaps";
const PROC_SWAPPINESS: &str = "/proc/sys/vm/swappiness";
const SYSCTL_FILE: &str = "/etc/sysctl.d/99-buckle-swap.conf";
const SYSCTL_MARKER: &str = "swap.file sysctl";
const MIB: u64 = 1024 * 1024;

fn fstab_line(spec: &Spec) -> String {
    format!("{} none swap sw 0 0", spec.path.display())
}

struct Observed {
    file_size: Option<u64>,
    file_mode_ok: bool,
    /// Size in KiB if the file is an active swap area.
    active_kib: Option<u64>,
    in_fstab: bool,
    swappiness_ok: bool,
}

impl Observed {
    fn size_ok(&self, spec: &Spec) -> bool {
        match self.active_kib {
            Some(kib) => kib == spec.size_mb * 1024,
            None => self.file_size == Some(spec.size_mb * MIB),
        }
    }

    fn needs_create(&self, spec: &Spec) -> bool {
        self.file_size.is_none() || !self.size_ok(spec)
    }

    fn needs_activate(&self, spec: &Spec) -> bool {
        self.needs_create(spec) || self.active_kib.is_none()
    }

    fn settled(&self, spec: &Spec) -> bool {
        !self.needs_activate(spec) && self.in_fstab && self.file_mode_ok && self.swappiness_ok
    }
}

async fn observe(cx: &Cx, spec: &Spec) -> Result<Observed> {
    let meta = cx.system.stat(&spec.path).await?;
    let swaps = cx
        .system
        .read(Path::new(PROC_SWAPS))
        .await?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let active_kib = swaps.lines().skip(1).find_map(|line| {
        let mut f = line.split_whitespace();
        (f.next().map(Path::new) == Some(spec.path.as_path()))
            .then(|| f.nth(1).and_then(|s| s.parse().ok()))
            .flatten()
    });
    let fstab = cx
        .system
        .read(Path::new(FSTAB))
        .await?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let in_fstab = fstab.lines().any(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        !line.trim_start().starts_with('#')
            && f.first().map(Path::new) == Some(spec.path.as_path())
            && f.get(2) == Some(&"swap")
    });
    let swappiness_ok = match spec.swappiness {
        None => true,
        Some(want) => {
            cx.system
                .read(Path::new(PROC_SWAPPINESS))
                .await?
                .and_then(|b| String::from_utf8_lossy(&b).trim().parse::<u32>().ok())
                == Some(want)
        }
    };
    Ok(Observed {
        file_size: meta.as_ref().map(|m| m.size),
        file_mode_ok: meta
            .as_ref()
            .is_none_or(|m| hostkit::perm_changes(m, &FilePerms::mode(0o600)).is_empty()),
        active_kib,
        in_fstab,
        swappiness_ok,
    })
}

/// Whether the file at `path` starts with a Linux swap header (`SWAPSPACE2`,
/// or the old `SWAP-SPACE`, in the last 10 bytes of the first page, for page
/// sizes up to 64 KiB). Read with `dd` so a multi-gigabyte file is not loaded.
async fn has_swap_signature(cx: &Cx, path: &Path) -> Result<bool> {
    if cx.system.which("dd").await?.is_none() {
        return Ok(false);
    }
    let out = cx
        .system
        .run(
            Command::new("dd")
                .arg(format!("if={}", path.display()))
                .arg("bs=65536")
                .arg("count=1")
                .arg("status=none"),
        )
        .await?;
    if !out.success() {
        return Ok(false);
    }
    Ok([4096usize, 8192, 16384, 65536].iter().any(|page| {
        out.stdout
            .get(page - 10..*page)
            .is_some_and(|sig| sig == b"SWAPSPACE2" || sig == b"SWAP-SPACE")
    }))
}

/// `Some(reason)` when applying would delete or format a file that is not a
/// swap area and that Buckle did not create.
async fn foreign_file(cx: &Cx, spec: &Spec, seen: &Observed) -> Result<Option<String>> {
    let touches_file = seen.file_size.is_some()
        && seen.active_kib.is_none()
        && (seen.needs_create(spec) || seen.needs_activate(spec));
    if !touches_file
        || hostkit::ownership::owns_file(cx, &spec.path)
        || has_swap_signature(cx, &spec.path).await?
    {
        return Ok(None);
    }
    Ok(Some(format!(
        "{} exists, has no swap signature and was not created by Buckle; refusing to delete or mkswap it. Move the file away or choose another `path`",
        spec.path.display()
    )))
}

fn artefact_line(spec: &Spec) -> Artefact {
    Artefact::Line {
        path: PathBuf::from(FSTAB),
        marker: fstab_line(spec),
    }
}

async fn run_tool(cx: &Cx, cmd: Command, what: &str) -> Result<()> {
    hostkit::require_tool(cx, &cmd.program, "install util-linux").await?;
    hostkit::run_checked(cx, cmd, what).await?;
    Ok(())
}

#[async_trait]
impl Task for SwapFile {
    type Spec = Spec;
    const KIND: &'static str = "swap.file";
    const LOCKS: &'static [&'static str] = &["fstab"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str =
        "Create a swap file, add it to fstab and activate it; one resource owns all three.";

    fn key(spec: &Spec) -> String {
        spec.path.display().to_string()
    }

    fn describe(spec: &Spec) -> String {
        format!("{} MiB swap file at {}", spec.size_mb, spec.path.display())
    }

    #[instrument(name = "task.swap.file.plan", skip_all, fields(path = %spec.path.display(), size_mb = spec.size_mb))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        hostkit::require_absolute(&spec.path, "path")?;
        if spec.size_mb == 0 {
            return Err(error::user(
                "`size_mb` of a swap file must be greater than zero.",
                &["Set the size in MiB, e.g. `size_mb: 2048`."],
            ));
        }
        let seen = observe(cx, spec).await?;
        if seen.settled(spec) {
            return Ok(Planned::NoOp);
        }
        if let Some(reason) = foreign_file(cx, spec, &seen).await? {
            return Ok(Planned::blocked(reason));
        }
        if seen.needs_activate(spec) {
            for tool in ["mkswap", "swapon"] {
                if let Some(blocked) = hostkit::missing_tool(cx, tool, "install util-linux").await?
                {
                    return Ok(blocked);
                }
            }
        }
        let mut steps = Vec::new();
        if seen.needs_create(spec) {
            steps.push(format!("create a {} MiB swap file", spec.size_mb));
        }
        if seen.needs_activate(spec) {
            steps.push("mkswap and swapon".to_owned());
        }
        if !seen.in_fstab {
            steps.push("add the fstab entry".to_owned());
        }
        if !seen.file_mode_ok {
            steps.push("set mode 0600".to_owned());
        }
        if !seen.swappiness_ok {
            steps.push(format!(
                "set vm.swappiness={}",
                spec.swappiness.unwrap_or_default()
            ));
        }
        Ok(Planned::change(Diff::Opaque {
            summary: format!("{}: {}", spec.path.display(), steps.join(", ")),
        }))
    }

    #[instrument(name = "task.swap.file.apply", skip_all, fields(path = %spec.path.display(), size_mb = spec.size_mb))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        hostkit::require_absolute(&spec.path, "path")?;
        let seen = observe(cx, spec).await?;
        if seen.settled(spec) {
            return Ok(hostkit::ownership::carry_forward(
                cx,
                Applied::unchanged(format!("{} already active", spec.path.display())),
            ));
        }
        if let Some(reason) = foreign_file(cx, spec, &seen).await? {
            return Err(error::user(
                format!("The swap file was not configured: {reason}."),
                &["A data file at the swap path is never overwritten."],
            ));
        }
        let path = spec.path.display().to_string();
        let mut applied = Applied::changed(format!("configured swap file {path}"));
        let create = seen.needs_create(spec);
        if create {
            if seen.active_kib.is_some() {
                run_tool(
                    cx,
                    Command::new("swapoff").arg(&path),
                    "Deactivating the old swap file",
                )
                .await?;
            }
            if seen.file_size.is_some() {
                cx.system.remove(&spec.path, false).await?;
            }
            let fallocated = if cx.system.which("fallocate").await?.is_some() {
                let out = cx
                    .system
                    .run(
                        Command::new("fallocate")
                            .arg("-l")
                            .arg(format!("{}M", spec.size_mb))
                            .arg(&path),
                    )
                    .await?;
                out.success()
            } else {
                false
            };
            if !fallocated {
                let dd = Command::new("dd")
                    .arg("if=/dev/zero")
                    .arg(format!("of={path}"))
                    .arg("bs=1M")
                    .arg(format!("count={}", spec.size_mb))
                    .arg("status=none")
                    .timeout(Duration::from_secs(1800));
                run_tool(cx, dd, "Creating the swap file").await?;
            }
            applied = applied.with_artefact(hostkit::file_artefact(&spec.path));
        }
        if create || !seen.file_mode_ok {
            cx.system
                .set_perms(&spec.path, &FilePerms::mode(0o600))
                .await?;
        }
        if seen.needs_activate(spec) {
            run_tool(
                cx,
                Command::new("mkswap").arg(&path),
                "Formatting the swap file",
            )
            .await?;
        }
        if !seen.in_fstab {
            let existing = hostkit::load_text(cx, Path::new(FSTAB)).await?;
            let mut text = existing
                .as_ref()
                .map(|f| f.text.clone())
                .unwrap_or_default();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&fstab_line(spec));
            text.push('\n');
            hostkit::save_text(
                cx,
                Path::new(FSTAB),
                &text,
                existing.as_ref(),
                false,
                &FilePerms::mode(0o644),
            )
            .await?;
            applied = applied.with_artefact(artefact_line(spec));
        }
        if seen.needs_activate(spec) {
            run_tool(
                cx,
                Command::new("swapon").arg(&path),
                "Activating the swap file",
            )
            .await?;
        }
        if let Some(want) = spec.swappiness
            && !seen.swappiness_ok
        {
            run_tool(
                cx,
                Command::new("sysctl")
                    .arg("-w")
                    .arg(format!("vm.swappiness={want}")),
                "Setting swappiness",
            )
            .await?;
            cx.system
                .write_atomic(
                    Path::new(SYSCTL_FILE),
                    format!("# Managed by Buckle (swap.file)\nvm.swappiness = {want}\n").as_bytes(),
                    &FilePerms::mode(0o644),
                )
                .await?;
            applied = applied.with_artefact(Artefact::Other {
                description: format!("{SYSCTL_MARKER}: {SYSCTL_FILE}"),
            });
        }
        // The file, fstab line and sysctl file Buckle created on earlier applies stay its own.
        Ok(hostkit::ownership::carry_forward(cx, applied))
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!(
            "swapoff and delete the swap file {} with its fstab line, if Buckle created it",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let mut actions = Vec::new();
        let swap_file = record.artefacts.iter().find_map(|a| match a {
            Artefact::File { path } => Some(path.clone()),
            _ => None,
        });
        if let Some(path) = &swap_file {
            let active =
                cx.system
                    .read(Path::new(PROC_SWAPS))
                    .await?
                    .map(|b| {
                        String::from_utf8_lossy(&b).lines().skip(1).any(|l| {
                            l.split_whitespace().next().map(Path::new) == Some(path.as_path())
                        })
                    })
                    .unwrap_or(false);
            if active {
                run_tool(
                    cx,
                    Command::new("swapoff").arg(path.display().to_string()),
                    "Deactivating the swap file",
                )
                .await?;
                actions.push("swapoff");
            }
        }
        for artefact in &record.artefacts {
            match artefact {
                Artefact::Line { path, marker } if path == Path::new(FSTAB) => {
                    if let Some(file) = hostkit::load_text(cx, path).await? {
                        let mut lines = hostkit::split_lines(&file.text);
                        let before = lines.len();
                        lines.retain(|l| l.trim() != marker);
                        if lines.len() != before {
                            hostkit::save_text(
                                cx,
                                path,
                                &hostkit::join_lines(&lines),
                                Some(&file),
                                false,
                                &FilePerms::default(),
                            )
                            .await?;
                            actions.push("removed the fstab line");
                        }
                    }
                }
                Artefact::Other { description } => {
                    if let Some(path) = description.strip_prefix(&format!("{SYSCTL_MARKER}: "))
                        && cx.system.remove(Path::new(path), false).await?
                    {
                        actions.push("deleted the sysctl file");
                    }
                }
                _ => {}
            }
        }
        if let Some(path) = &swap_file
            && cx.system.remove(path, false).await?
        {
            actions.push("deleted the swap file");
        }
        Ok(if actions.is_empty() {
            Applied::skipped(
                "nothing to undo: the swap file was not created by Buckle or is already gone",
            )
        } else {
            Applied::changed(actions.join(", "))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::Output;
    use buckle_core::testing::{FakeSystem, ok_output};
    use rstest::rstest;
    use serde_json::json;

    const HEADER: &str = "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n";

    fn s(extra: serde_json::Value) -> Spec {
        let mut raw = json!({"path": "/swapfile", "size_mb": 1});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        spec(raw)
    }

    fn zeros(mb: usize) -> Vec<u8> {
        vec![0; mb * 1024 * 1024]
    }

    /// A host whose swap tools update the fake's files like the real ones would.
    fn host() -> FakeSystem {
        let sys = FakeSystem::new()
            .with_file(
                "/etc/fstab",
                "UUID=abc / ext4 defaults 0 1\n",
                FilePerms::mode(0o644),
            )
            .with_file(PROC_SWAPS, HEADER, FilePerms::default())
            .with_file(PROC_SWAPPINESS, "60\n", FilePerms::default());
        let h = sys.clone();
        let fallocate = move |cmd: &Command| {
            let mb: usize = cmd.args[1].trim_end_matches('M').parse().unwrap();
            let _ =
                h.clone()
                    .with_file(cmd.args.last().unwrap(), zeros(mb), FilePerms::mode(0o644));
            ok_output("")
        };
        let h = sys.clone();
        let swapon = move |cmd: &Command| {
            let path = cmd.args.last().unwrap();
            let size_kib = h.file(path).map(|b| b.len() / 1024).unwrap_or(0);
            let current = h.file_string(PROC_SWAPS).unwrap_or_default();
            let _ = h.clone().with_file(
                PROC_SWAPS,
                format!("{current}{path}\tfile\t{size_kib}\t0\t-2\n"),
                FilePerms::default(),
            );
            ok_output("")
        };
        let h = sys.clone();
        let swapoff = move |cmd: &Command| {
            let path = cmd.args.last().unwrap();
            let kept: String = h
                .file_string(PROC_SWAPS)
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.starts_with(path.as_str()))
                .map(|l| format!("{l}\n"))
                .collect();
            let _ = h.clone().with_file(PROC_SWAPS, kept, FilePerms::default());
            ok_output("")
        };
        let h = sys.clone();
        let sysctl = move |cmd: &Command| {
            let value = cmd.args[1].split('=').nth(1).unwrap().to_owned();
            let _ =
                h.clone()
                    .with_file(PROC_SWAPPINESS, format!("{value}\n"), FilePerms::default());
            ok_output("")
        };
        let h = sys.clone();
        let dd = move |cmd: &Command| {
            let input = cmd.args[0].trim_start_matches("if=").to_owned();
            if input == "/dev/zero" {
                let out = cmd.args[1].trim_start_matches("of=").to_owned();
                let mb: usize = cmd.args[3].trim_start_matches("count=").parse().unwrap();
                let _ = h.clone().with_file(&out, zeros(mb), FilePerms::mode(0o644));
                return ok_output("");
            }
            let mut bytes = h.file(&input).unwrap_or_default();
            bytes.truncate(65536);
            Output {
                code: Some(0),
                stdout: bytes,
                stderr: vec![],
                timed_out: false,
            }
        };
        sys.on_command("dd", &[], dd)
            .on_command("fallocate", &[], fallocate)
            .on_command_ok("mkswap", &[], "")
            .on_command("swapon", &[], swapon)
            .on_command("swapoff", &[], swapoff)
            .on_command("sysctl", &[], sysctl)
    }

    #[tokio::test]
    async fn listed_in_proc_swaps_with_fstab_and_mode_is_a_noop() {
        let sys = host()
            .with_file("/swapfile", zeros(1), FilePerms::mode(0o600))
            .with_file(
                PROC_SWAPS,
                format!("{HEADER}/swapfile\tfile\t1024\t0\t-2\n"),
                FilePerms::default(),
            )
            .with_file(
                "/etc/fstab",
                "/swapfile none swap sw 0 0\n",
                FilePerms::mode(0o644),
            );
        assert_eq!(
            plan_class(&SwapFile, &cx(&sys), &s(json!({}))).await,
            Class::NoOp
        );
        // The active size comes from /proc/swaps, not from stat.
        let sys = sys.with_file("/swapfile", "tiny", FilePerms::mode(0o600));
        assert_eq!(
            plan_class(&SwapFile, &cx(&sys), &s(json!({}))).await,
            Class::NoOp
        );
    }

    #[rstest]
    #[case::nothing_exists(host(), Class::Change)]
    #[case::file_but_not_active(host().with_file("/swapfile", swap_header(1), FilePerms::mode(0o600)), Class::Change)]
    #[case::wrong_size(host().with_file("/swapfile", swap_header(2), FilePerms::mode(0o600)), Class::Change)]
    #[case::active_but_not_in_fstab(
        host().with_file("/swapfile", zeros(1), FilePerms::mode(0o600)).with_file(PROC_SWAPS, format!("{HEADER}/swapfile\tfile\t1024\t0\t-2\n"), FilePerms::default()),
        Class::Change
    )]
    #[case::active_with_loose_mode(
        host().with_file("/swapfile", zeros(1), FilePerms::mode(0o644)).with_file(PROC_SWAPS, format!("{HEADER}/swapfile\tfile\t1024\t0\t-2\n"), FilePerms::default()).with_file("/etc/fstab", "/swapfile none swap sw 0 0\n", FilePerms::default()),
        Class::Change
    )]
    #[case::active_but_wrong_size(
        host().with_file("/swapfile", zeros(1), FilePerms::mode(0o600)).with_file(PROC_SWAPS, format!("{HEADER}/swapfile\tfile\t2048\t0\t-2\n"), FilePerms::default()).with_file("/etc/fstab", "/swapfile none swap sw 0 0\n", FilePerms::default()),
        Class::Change
    )]
    #[case::mkswap_missing(
        FakeSystem::new().with_file(PROC_SWAPS, HEADER, FilePerms::default()),
        Class::Blocked
    )]
    #[tokio::test]
    async fn plan_outcomes(#[case] sys: FakeSystem, #[case] expected: Class) {
        assert_eq!(
            plan_class(&SwapFile, &cx(&sys), &s(json!({}))).await,
            expected
        );
    }

    #[tokio::test]
    async fn swappiness_drift_alone_is_a_change() {
        let settled = host()
            .with_file("/swapfile", zeros(1), FilePerms::mode(0o600))
            .with_file(
                PROC_SWAPS,
                format!("{HEADER}/swapfile\tfile\t1024\t0\t-2\n"),
                FilePerms::default(),
            )
            .with_file(
                "/etc/fstab",
                "/swapfile none swap sw 0 0\n",
                FilePerms::default(),
            );
        assert_eq!(
            plan_class(&SwapFile, &cx(&settled), &s(json!({"swappiness": 60}))).await,
            Class::NoOp
        );
        assert_eq!(
            plan_class(&SwapFile, &cx(&settled), &s(json!({"swappiness": 10}))).await,
            Class::Change
        );
    }

    #[tokio::test]
    async fn apply_creates_activates_persists_and_replans_as_noop() {
        let sys = host();
        let c = cx(&sys);
        let spec = s(json!({"swappiness": 10}));
        let applied = apply(&SwapFile, &c, &spec).await;
        assert!(applied.changed);
        let lines = sys.command_lines();
        let pos = |needle: &str| {
            lines
                .iter()
                .position(|l| l.starts_with(needle))
                .unwrap_or_else(|| panic!("{needle} in {lines:?}"))
        };
        assert!(
            pos("fallocate") < pos("mkswap") && pos("mkswap") < pos("swapon"),
            "{lines:?}"
        );
        assert!(
            sys.file_string("/etc/fstab")
                .unwrap()
                .ends_with("/swapfile none swap sw 0 0\n")
        );
        assert!(
            sys.file_string("/etc/fstab")
                .unwrap()
                .starts_with("UUID=abc"),
            "existing entries are kept"
        );
        assert_eq!(
            sys.file_string(SYSCTL_FILE).unwrap(),
            "# Managed by Buckle (swap.file)\nvm.swappiness = 10\n"
        );
        let meta = buckle_core::system::System::stat(&sys, Path::new("/swapfile"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.mode, Some(0o600));
        assert_eq!(plan_class(&SwapFile, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn dd_is_the_fallback_when_fallocate_fails() {
        let sys = host().on_command("fallocate", &[], |_| Output {
            code: Some(1),
            stdout: vec![],
            stderr: b"unsupported".to_vec(),
            timed_out: false,
        });
        let handle = sys.clone();
        let sys = sys.on_command("dd", &[], move |cmd| {
            let of = cmd
                .args
                .iter()
                .find_map(|a| a.strip_prefix("of="))
                .unwrap()
                .to_owned();
            let _ = handle
                .clone()
                .with_file(of, zeros(1), FilePerms::mode(0o644));
            ok_output("")
        });
        apply(&SwapFile, &cx(&sys), &s(json!({}))).await;
        assert!(
            sys.command_lines()
                .iter()
                .any(|l| l.starts_with("dd if=/dev/zero of=/swapfile bs=1M count=1"))
        );
    }

    #[tokio::test]
    async fn a_file_of_the_wrong_size_is_swapped_off_and_recreated() {
        let sys = host()
            .with_file("/swapfile", zeros(2), FilePerms::mode(0o600))
            .with_file(
                PROC_SWAPS,
                format!("{HEADER}/swapfile\tfile\t2048\t0\t-2\n"),
                FilePerms::default(),
            )
            .with_file(
                "/etc/fstab",
                "/swapfile none swap sw 0 0\n",
                FilePerms::default(),
            );
        let c = cx(&sys);
        let spec = s(json!({}));
        apply(&SwapFile, &c, &spec).await;
        assert!(sys.command_lines()[0].starts_with("swapoff"));
        assert_eq!(sys.file("/swapfile").unwrap().len(), 1024 * 1024);
        assert_eq!(
            sys.file_string("/etc/fstab")
                .unwrap()
                .matches("/swapfile")
                .count(),
            1,
            "no duplicate fstab line"
        );
        assert_eq!(plan_class(&SwapFile, &c, &spec).await, Class::NoOp);
    }

    #[tokio::test]
    async fn removal_undoes_everything_buckle_created() {
        let sys = host();
        let c = cx(&sys);
        let rec = record(
            "swap.file",
            "/swapfile",
            &apply(&SwapFile, &c, &s(json!({"swappiness": 10}))).await,
        );
        let out = SwapFile.remove(&c, &rec).await.unwrap();
        assert!(out.changed, "{}", out.summary);
        assert!(!sys.exists("/swapfile") && !sys.exists(SYSCTL_FILE));
        assert!(!sys.file_string("/etc/fstab").unwrap().contains("/swapfile"));
        assert!(!sys.file_string(PROC_SWAPS).unwrap().contains("/swapfile"));
        assert!(sys.file_string("/etc/fstab").unwrap().contains("UUID=abc"));
    }

    #[tokio::test]
    async fn pre_existing_swap_files_are_not_deleted_on_removal() {
        let sys = host()
            .with_file("/swapfile", zeros(1), FilePerms::mode(0o600))
            .with_file(
                PROC_SWAPS,
                format!("{HEADER}/swapfile\tfile\t1024\t0\t-2\n"),
                FilePerms::default(),
            );
        let c = cx(&sys);
        let applied = apply(&SwapFile, &c, &s(json!({}))).await;
        assert!(
            applied
                .artefacts
                .iter()
                .all(|a| !matches!(a, Artefact::File { .. }))
        );
        let rec = record("swap.file", "/swapfile", &applied);
        SwapFile.remove(&c, &rec).await.unwrap();
        assert!(sys.exists("/swapfile"));
        assert!(!sys.command_lines().iter().any(|l| l.starts_with("swapoff")));
    }

    #[tokio::test]
    async fn invalid_specs_are_user_errors() {
        let sys = host();
        assert!(
            SwapFile
                .plan(&cx(&sys), &s(json!({"size_mb": 0})))
                .await
                .is_err()
        );
        assert!(
            SwapFile
                .plan(&cx(&sys), &s(json!({"path": "swapfile"})))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_later_swappiness_change_keeps_everything_buckle_created() {
        let sys = host();
        let first = apply(&SwapFile, &cx(&sys), &s(json!({}))).await;
        assert!(
            first
                .artefacts
                .iter()
                .any(|a| matches!(a, Artefact::File { .. }))
        );
        let c = buckle_core::testing::CxBuilder::new(sys.clone())
            .previous(record("swap.file", "/swapfile", &first))
            .build();
        let second = apply(&SwapFile, &c, &s(json!({"swappiness": 10}))).await;
        for artefact in &first.artefacts {
            assert!(second.artefacts.contains(artefact), "{artefact:?} kept");
        }
        assert!(
            SwapFile
                .remove(&cx(&sys), &record("swap.file", "/swapfile", &second))
                .await
                .unwrap()
                .summary
                .contains("deleted the swap file")
        );
    }

    /// A swap header in the last 10 bytes of a 4 KiB page.
    fn swap_header(mb: usize) -> Vec<u8> {
        let mut bytes = zeros(mb);
        bytes[4096 - 10..4096].copy_from_slice(b"SWAPSPACE2");
        bytes
    }

    #[tokio::test]
    async fn a_data_file_at_the_swap_path_is_never_deleted_or_formatted() {
        for size in [1usize, 2] {
            // Right size (would be mkswap'd) and wrong size (would be deleted).
            let sys = host().with_file(
                "/swapfile",
                vec![7u8; size * 1024 * 1024],
                FilePerms::mode(0o600),
            );
            let planned = SwapFile.plan(&cx(&sys), &s(json!({}))).await.unwrap();
            assert!(
                matches!(&planned, Planned::Blocked { reason } if reason.contains("no swap signature")),
                "{planned:?}"
            );
            assert!(
                SwapFile
                    .apply(&cx(&sys), &s(json!({})), &Planned::NoOp)
                    .await
                    .is_err()
            );
            assert_eq!(sys.file("/swapfile").unwrap()[0], 7);
            assert!(
                !sys.command_lines()
                    .iter()
                    .any(|l| l.starts_with("mkswap") || l.starts_with("fallocate"))
            );
        }
    }

    #[tokio::test]
    async fn an_inactive_swap_area_or_a_file_buckle_created_may_be_reformatted() {
        let sys = host().with_file("/swapfile", swap_header(2), FilePerms::mode(0o600));
        assert_eq!(
            plan_class(&SwapFile, &cx(&sys), &s(json!({}))).await,
            Class::Change
        );
        apply(&SwapFile, &cx(&sys), &s(json!({}))).await;
        assert_eq!(sys.file("/swapfile").unwrap().len(), 1024 * 1024);

        // No signature, but Buckle's own file from an earlier apply.
        let sys = host().with_file("/swapfile", zeros(2), FilePerms::mode(0o600));
        let prev = record(
            "swap.file",
            "/swapfile",
            &Applied::changed("x").with_artefact(hostkit::file_artefact(Path::new("/swapfile"))),
        );
        let c = buckle_core::testing::CxBuilder::new(sys.clone())
            .previous(prev)
            .build();
        assert_eq!(
            plan_class(&SwapFile, &c, &s(json!({}))).await,
            Class::Change
        );
    }
}
