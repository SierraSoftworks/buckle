//! `sysctl.param`: a kernel parameter, live and persisted.

use super::sys_exec::{Scalar, run_checked, squash_ws};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::Redacted;
use buckle_core::state::ResourceRecord;
use buckle_core::system::{Command, FilePerms};
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Digest, Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Sets a kernel parameter and keeps it set across reboots.
///
/// `name` is the sysctl key (`net.ipv4.ip_forward`) and `value` its value; a
/// bare YAML number or boolean is fine (`value: 1`). With `persist: true`
/// (the default) the setting is written as `name = value` to `file`, which
/// defaults to `/etc/sysctl.d/90-buckle-<module>.conf`; every `sysctl.param` in
/// a module shares that file and other lines in it are preserved. Applying
/// also runs `sysctl -w name=value` so the change takes effect immediately.
/// With `persist: false` only the live value is managed.
///
/// A plan compares the live value (`sysctl -n`, whitespace-normalised) and the
/// persisted line. Removal deletes the line, and the file when nothing but
/// comments is left. It does not touch the live value: the kernel keeps what
/// it was last given until the next reboot, because Buckle cannot know the
/// previous value.
pub struct SysctlParam;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The sysctl key, e.g. `vm.swappiness`.
    pub name: String,
    /// The value to set (a string, number or boolean).
    pub value: Scalar,
    /// Also write the setting to a sysctl.d file so it survives reboots.
    #[serde(default = "default_true")]
    pub persist: bool,
    /// The file to persist into; defaults to `/etc/sysctl.d/90-buckle-<module>.conf`.
    #[serde(default)]
    pub file: Option<PathBuf>,
}

fn default_true() -> bool {
    true
}

impl Spec {
    fn wanted(&self) -> String {
        squash_ws(&self.value.text())
    }

    fn file(&self, cx: &Cx) -> PathBuf {
        self.file.clone().unwrap_or_else(|| {
            let module: String = cx
                .module
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            PathBuf::from(format!("/etc/sysctl.d/90-buckle-{module}.conf"))
        })
    }
}

/// `key = value` of a sysctl.conf line; comments and blanks are `None`.
fn parse_line(line: &str) -> Option<(&str, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    Some((k.trim().trim_start_matches('-'), squash_ws(v)))
}

/// The persisted value of `name` (the last assignment wins, as in sysctl).
fn persisted_value(content: &str, name: &str) -> Option<String> {
    content
        .lines()
        .filter_map(parse_line)
        .filter(|(k, _)| *k == name)
        .map(|(_, v)| v)
        .next_back()
}

/// `content` with `name` set to `value`: the first assignment is rewritten in
/// place and duplicates dropped; otherwise a line is appended.
fn with_setting(content: Option<&str>, module: &str, name: &str, value: &str) -> String {
    let new_line = format!("{name} = {value}");
    let Some(content) = content else {
        return format!("# Managed by Buckle (module {module})\n{new_line}\n");
    };
    let mut out = Vec::new();
    let mut written = false;
    for line in content.lines() {
        match parse_line(line) {
            Some((k, _)) if k == name => {
                if !written {
                    out.push(new_line.clone());
                    written = true;
                }
            }
            _ => out.push(line.to_owned()),
        }
    }
    if !written {
        out.push(new_line);
    }
    out.join("\n") + "\n"
}

/// `content` without any assignment of `name`, and whether any setting remains.
fn without_setting(content: &str, name: &str) -> (String, bool) {
    let kept: Vec<&str> = content
        .lines()
        .filter(|l| parse_line(l).is_none_or(|(k, _)| k != name))
        .collect();
    let any_left = kept.iter().any(|l| parse_line(l).is_some());
    (kept.join("\n") + "\n", any_left)
}

async fn live_value(cx: &Cx, name: &str) -> Result<Option<String>> {
    let out = cx
        .system
        .run(Command::new("sysctl").args(["-n", name]))
        .await?;
    Ok(out.success().then(|| squash_ws(&out.stdout_str())))
}

#[async_trait]
impl Task for SysctlParam {
    type Spec = Spec;
    const KIND: &'static str = "sysctl.param";
    const LOCKS: &'static [&'static str] = &["sysctl"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str =
        "Set a kernel parameter live and persist it in /etc/sysctl.d.";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        format!("{} = {}", spec.name, spec.wanted())
    }

    #[tracing::instrument(name = "task.sysctl.param.plan", skip_all, fields(param = %spec.name))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        let wanted = spec.wanted();
        let live = live_value(cx, &spec.name).await?;
        let live_ok = live.as_deref() == Some(wanted.as_str());

        let mut file_change: Option<Diff> = None;
        if spec.persist {
            let path = spec.file(cx);
            let current = cx
                .system
                .read(&path)
                .await?
                .map(|b| String::from_utf8_lossy(&b).into_owned());
            let persisted = current
                .as_deref()
                .and_then(|c| persisted_value(c, &spec.name));
            if persisted.as_deref() != Some(wanted.as_str()) {
                let updated = with_setting(current.as_deref(), &cx.module, &spec.name, &wanted);
                let summary = format!("persist {} = {} in {}", spec.name, wanted, path.display());
                file_change = Some(match current {
                    None => Diff::Create {
                        summary,
                        preview: Some(Redacted::plain(updated)),
                    },
                    Some(old) => {
                        Diff::unified(summary, &Redacted::plain(old), &Redacted::plain(updated))
                    }
                });
            }
        }
        match (live_ok, file_change) {
            (true, None) => Ok(Planned::NoOp),
            (true, Some(diff)) => Ok(Planned::change(diff)),
            (false, None) => Ok(Planned::change(Diff::Opaque {
                summary: format!(
                    "sysctl -w {}={} (now {})",
                    spec.name,
                    wanted,
                    live.as_deref().unwrap_or("unset")
                ),
            })),
            (false, Some(Diff::Update { summary, unified })) => Ok(Planned::change(Diff::Update {
                summary: format!(
                    "{summary}; sysctl -w {}={} (now {})",
                    spec.name,
                    wanted,
                    live.as_deref().unwrap_or("unset")
                ),
                unified,
            })),
            (false, Some(Diff::Create { summary, preview })) => Ok(Planned::change(Diff::Create {
                summary: format!("{summary}; sysctl -w {}={}", spec.name, wanted),
                preview,
            })),
            (false, Some(other)) => Ok(Planned::change(other)),
        }
    }

    #[tracing::instrument(name = "task.sysctl.param.apply", skip_all, fields(param = %spec.name))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let wanted = spec.wanted();
        let mut applied = Applied::changed(format!("set {} = {}", spec.name, wanted));
        if spec.persist {
            let path = spec.file(cx);
            let current = cx
                .system
                .read(&path)
                .await?
                .map(|b| String::from_utf8_lossy(&b).into_owned());
            let updated = with_setting(current.as_deref(), &cx.module, &spec.name, &wanted);
            if current.as_deref() != Some(updated.as_str()) {
                cx.system
                    .write_atomic(&path, updated.as_bytes(), &FilePerms::mode(0o644))
                    .await?;
            }
            let line = format!("{} = {}", spec.name, wanted);
            applied = applied
                .with_shadow(Redacted::plain(line.clone()), Digest::of(line.as_bytes()))
                .with_artefact(Artefact::Line {
                    path,
                    marker: spec.name.clone(),
                });
        }
        run_checked(
            cx,
            Command::new("sysctl")
                .arg("-w")
                .arg(format!("{}={}", spec.name, wanted)),
        )
        .await?;
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        match line_artefact(record) {
            Some((path, marker)) => format!(
                "remove {marker} from {} (the live value is left as it is until reboot)",
                path.display()
            ),
            None => format!(
                "nothing to remove for {} (it was not persisted)",
                record.key.key
            ),
        }
    }

    #[tracing::instrument(name = "task.sysctl.param.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some((path, marker)) = line_artefact(record) else {
            return Ok(Applied::skipped(
                "the parameter was not persisted; the live value is left as it is",
            ));
        };
        let Some(bytes) = cx.system.read(&path).await? else {
            return Ok(Applied::unchanged(format!(
                "{} no longer exists",
                path.display()
            )));
        };
        let content = String::from_utf8_lossy(&bytes).into_owned();
        if persisted_value(&content, &marker).is_none() {
            return Ok(Applied::unchanged(format!(
                "{marker} is no longer in {}",
                path.display()
            )));
        }
        let (updated, any_left) = without_setting(&content, &marker);
        if any_left {
            cx.system
                .write_atomic(&path, updated.as_bytes(), &FilePerms::default())
                .await?;
            Ok(Applied::changed(format!(
                "removed {marker} from {}",
                path.display()
            )))
        } else {
            cx.system.remove(&path, false).await?;
            Ok(Applied::changed(format!(
                "removed {marker} and deleted the now empty {}",
                path.display()
            )))
        }
    }
}

fn line_artefact(record: &ResourceRecord) -> Option<(PathBuf, String)> {
    record.artefacts.iter().find_map(|a| match a {
        Artefact::Line { path, marker } => Some((path.clone(), marker.clone())),
        _ => None,
    })
}

buckle_core::register_task!(SysctlParam);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use buckle_core::system::Output;
    use buckle_core::testing::{FakeSystem, ok_output};
    use rstest::rstest;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    const FILE: &str = "/etc/sysctl.d/90-buckle-web.conf";

    /// A pretend kernel: `sysctl -n` reads, `sysctl -w` writes.
    fn kernel(values: &[(&str, &str)]) -> (FakeSystem, Arc<Mutex<BTreeMap<String, String>>>) {
        let world: Arc<Mutex<BTreeMap<String, String>>> = Arc::new(Mutex::new(
            values
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ));
        let w = world.clone();
        let sys = FakeSystem::new().on_command("sysctl", &[], move |cmd| {
            let mut w = w.lock().unwrap();
            match cmd.args.as_slice() {
                [flag, name] if flag == "-n" => match w.get(name) {
                    // The kernel separates multi-value settings with tabs.
                    Some(v) => ok_output(&format!("{}\n", v.replace(' ', "\t"))),
                    None => Output {
                        code: Some(255),
                        stdout: vec![],
                        stderr: format!(
                            "sysctl: cannot stat /proc/sys/{name}: No such file or directory\n"
                        )
                        .into_bytes(),
                        timed_out: false,
                    },
                },
                [flag, assignment] if flag == "-w" => {
                    let (k, v) = assignment.split_once('=').unwrap();
                    w.insert(k.into(), v.into());
                    ok_output(&format!("{k} = {v}\n"))
                }
                _ => ok_output(""),
            }
        });
        (sys, world)
    }

    #[rstest]
    #[case::everything_matches(json!({"name": "net.ipv4.ip_forward", "value": 1}), "1", Some("net.ipv4.ip_forward = 1\n"), true)]
    #[case::equals_without_spaces(json!({"name": "net.ipv4.ip_forward", "value": "1"}), "1", Some("net.ipv4.ip_forward=1\n"), true)]
    #[case::multi_value_whitespace(json!({"name": "net.ipv4.tcp_rmem", "value": "4096  87380 6291456"}), "4096 87380 6291456", Some("# tuning\nnet.ipv4.tcp_rmem = 4096\t87380 6291456\n"), true)]
    #[case::not_persisted_so_file_ignored(json!({"name": "vm.swappiness", "value": 10, "persist": false}), "10", None, true)]
    #[case::live_differs(json!({"name": "net.ipv4.ip_forward", "value": 1}), "0", Some("net.ipv4.ip_forward = 1\n"), false)]
    #[case::file_missing(json!({"name": "net.ipv4.ip_forward", "value": 1}), "1", None, false)]
    #[case::file_has_old_value(json!({"name": "net.ipv4.ip_forward", "value": 1}), "1", Some("net.ipv4.ip_forward = 0\n"), false)]
    #[case::boolean_scalar(json!({"name": "net.ipv4.ip_forward", "value": true}), "true", Some("net.ipv4.ip_forward = true\n"), true)]
    #[tokio::test]
    async fn plan_compares_live_and_persisted_values(
        #[case] spec_json: serde_json::Value,
        #[case] live: &str,
        #[case] file: Option<&str>,
        #[case] noop: bool,
    ) {
        let name = spec_json["name"].as_str().unwrap().to_owned();
        let (mut sys, _world) = kernel(&[(name.as_str(), live)]);
        if let Some(file) = file {
            sys = sys.with_file(FILE, file, FilePerms::mode(0o644));
        }
        let cx = cx(&sys);
        let planned = SysctlParam.plan(&cx, &spec(spec_json)).await.unwrap();
        assert_eq!(planned == Planned::NoOp, noop, "{planned:?}");
    }

    #[tokio::test]
    async fn an_unknown_key_plans_a_change() {
        let (sys, _) = kernel(&[]);
        let cx = cx(&sys);
        let planned = SysctlParam
            .plan(
                &cx,
                &spec(json!({"name": "net.nf_conntrack_max", "value": 262144, "persist": false})),
            )
            .await
            .unwrap();
        match planned {
            Planned::Change { diff } => assert!(diff.summary().contains("now unset")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_creates_the_file_sets_the_value_and_converges() {
        let (sys, world) = kernel(&[("net.ipv4.ip_forward", "0")]);
        let cx = cx(&sys);
        let s: Spec = spec(json!({"name": "net.ipv4.ip_forward", "value": 1}));
        let applied = SysctlParam.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert_eq!(
            sys.file_string(FILE).as_deref(),
            Some("# Managed by Buckle (module web)\nnet.ipv4.ip_forward = 1\n")
        );
        assert_eq!(world.lock().unwrap()["net.ipv4.ip_forward"], "1");
        assert!(
            sys.command_lines()
                .contains(&"sysctl -w net.ipv4.ip_forward=1".to_owned())
        );
        assert_eq!(
            applied.artefacts,
            vec![Artefact::Line {
                path: FILE.into(),
                marker: "net.ipv4.ip_forward".into()
            }]
        );
        assert_eq!(SysctlParam.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn apply_keeps_other_lines_and_rewrites_the_existing_one_in_place() {
        let (sys, _) = kernel(&[("vm.swappiness", "60"), ("vm.max_map_count", "65530")]);
        let sys = sys.with_file(
            FILE,
            "# Managed by Buckle (module web)\nvm.max_map_count = 262144\nvm.swappiness = 60\nvm.swappiness = 30\n",
            FilePerms::mode(0o644),
        );
        let cx = cx(&sys);
        SysctlParam
            .apply(
                &cx,
                &spec(json!({"name": "vm.swappiness", "value": 10})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        assert_eq!(
            sys.file_string(FILE).as_deref(),
            Some(
                "# Managed by Buckle (module web)\nvm.max_map_count = 262144\nvm.swappiness = 10\n"
            )
        );
    }

    #[tokio::test]
    async fn custom_files_are_honoured() {
        let (sys, _) = kernel(&[("vm.swappiness", "60")]);
        let cx = cx(&sys);
        let s: Spec = spec(
            json!({"name": "vm.swappiness", "value": 10, "file": "/etc/sysctl.d/50-custom.conf"}),
        );
        SysctlParam.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(sys.exists("/etc/sysctl.d/50-custom.conf"));
        assert!(!sys.exists(FILE));
        assert_eq!(SysctlParam.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn removal_drops_only_its_line_and_leaves_the_live_value() {
        let (sys, world) = kernel(&[("vm.swappiness", "60"), ("net.ipv4.ip_forward", "0")]);
        let cx = cx(&sys);
        let a = SysctlParam
            .apply(
                &cx,
                &spec(json!({"name": "vm.swappiness", "value": 10})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        SysctlParam
            .apply(
                &cx,
                &spec(json!({"name": "net.ipv4.ip_forward", "value": 1})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        let record = record_of("sysctl.param", "vm.swappiness", &a);
        assert!(
            SysctlParam
                .describe_removal(&record)
                .contains("live value is left")
        );
        let before = sys.commands_run().len();
        let removed = SysctlParam.remove(&cx, &record).await.unwrap();
        assert!(removed.changed);
        let file = sys.file_string(FILE).unwrap();
        assert!(
            !file.contains("vm.swappiness") && file.contains("net.ipv4.ip_forward = 1"),
            "{file}"
        );
        assert_eq!(
            world.lock().unwrap()["vm.swappiness"],
            "10",
            "live value untouched"
        );
        assert_eq!(
            sys.commands_run().len(),
            before,
            "no sysctl command on removal"
        );
    }

    #[tokio::test]
    async fn removing_the_last_line_deletes_the_file() {
        let (sys, _) = kernel(&[("vm.swappiness", "60")]);
        let cx = cx(&sys);
        let a = SysctlParam
            .apply(
                &cx,
                &spec(json!({"name": "vm.swappiness", "value": 10})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        let record = record_of("sysctl.param", "vm.swappiness", &a);
        let removed = SysctlParam.remove(&cx, &record).await.unwrap();
        assert!(removed.summary.contains("deleted"));
        assert!(!sys.exists(FILE));
        // Removing again is harmless.
        assert!(!SysctlParam.remove(&cx, &record).await.unwrap().changed);
    }

    #[tokio::test]
    async fn live_only_params_have_nothing_to_remove() {
        let (sys, _) = kernel(&[("vm.swappiness", "60")]);
        let cx = cx(&sys);
        let a = SysctlParam
            .apply(
                &cx,
                &spec(json!({"name": "vm.swappiness", "value": 10, "persist": false})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        assert!(a.artefacts.is_empty());
        let record = record_of("sysctl.param", "vm.swappiness", &a);
        assert!(!SysctlParam.remove(&cx, &record).await.unwrap().changed);
    }

    #[rstest]
    #[case("net.ipv4.ip_forward = 1", Some("1"))]
    #[case("net.ipv4.ip_forward=1", Some("1"))]
    #[case("  net.ipv4.ip_forward   =   1  ", Some("1"))]
    #[case("# net.ipv4.ip_forward = 1", None)]
    #[case("; net.ipv4.ip_forward = 1", None)]
    #[case("-net.ipv4.ip_forward = 1", Some("1"))]
    #[case("", None)]
    fn lines_parse_like_sysctl_conf(#[case] line: &str, #[case] value: Option<&str>) {
        assert_eq!(parse_line(line).map(|(_, v)| v).as_deref(), value);
    }

    #[test]
    fn default_file_names_the_module() {
        let sys = FakeSystem::new();
        let cx = crate::tasks::sys_exec::testkit::cx(&sys);
        let s: Spec = spec(json!({"name": "a.b", "value": 1}));
        assert_eq!(
            s.file(&cx),
            PathBuf::from("/etc/sysctl.d/90-buckle-web.conf")
        );
        let _ = Path::new("");
    }
}
