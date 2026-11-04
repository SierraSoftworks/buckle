//! `system.reboot`: reboot the host once the run has finished (usually a handler).

use super::hostkit::{self, StringOrArgv};
use async_trait::async_trait;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, Planned};
use buckle_core::reboot::RebootRequest;
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::instrument;

/// Reboots the host after the run.
///
/// ```yaml
/// handlers:
///   reboot:
///     system.reboot:
///       delay: 5s          # wait after the run has been reported (default 5s)
///       timeout: 10m       # how long a rollout waits for the host to come back (default 5m)
///
/// tasks:                   # or as a task, guarded so it only reboots when needed
///   - system.reboot: { only_if: "test -f /run/reboot-required" }
/// ```
///
/// The task never reboots in the middle of a run. Applying it asks for a
/// reboot; the run carries on through every module, handler and removal, and
/// the host reboots once, after the run has been reported (to the server, or
/// on screen for `buckle apply`). Health checks the run had not reached when
/// the reboot was asked for run after the reboot instead, as part of the bake.
/// In a rollout the host shows as rebooting, and fails only if its agent has
/// not reconnected from a new boot within `timeout`.
///
/// A drift correction never reboots on its own; the reboot it asked for is
/// logged and left to the next apply or to an operator.
///
/// Without `only_if` the task plans a change on every run, so as a task (not a
/// handler) it would reboot the host on every apply: plans flag it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// How long to wait after the run has been reported, e.g. `5s`.
    #[serde(default = "default_delay", with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub delay: Duration,
    /// How long a rollout waits for the host to come back, e.g. `5m`.
    #[serde(default = "default_timeout", with = "humantime_serde")]
    #[schemars(with = "Option<String>")]
    pub timeout: Duration,
    /// Reboot only when this read-only probe exits 0. Runs in plan mode too.
    #[serde(default)]
    pub only_if: Option<StringOrArgv>,
}

fn default_delay() -> Duration {
    Duration::from_secs(5)
}

fn default_timeout() -> Duration {
    Duration::from_secs(300)
}

pub struct SystemReboot;
buckle_core::register_task!(SystemReboot);

#[async_trait]
impl Task for SystemReboot {
    type Spec = Spec;
    const KIND: &'static str = "system.reboot";
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_DEFAULT: RemovalPolicy = RemovalPolicy::Leave;
    const DESCRIPTION: &'static str =
        "Reboot the host once the run has finished (a handler, or a task guarded by only_if).";

    fn key(_spec: &Spec) -> String {
        "reboot".into()
    }

    fn describe(spec: &Spec) -> String {
        match &spec.only_if {
            Some(probe) => format!("reboot when `{}` succeeds", probe.display()),
            None => "reboot".into(),
        }
    }

    fn always_runs(spec: &Spec) -> bool {
        spec.only_if.is_none()
    }

    #[instrument(name = "task.system.reboot.plan", skip_all)]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        if buckle_core::reboot::command(cx.system.as_ref())
            .await?
            .is_none()
        {
            return Ok(Planned::blocked(
                "neither `systemctl` (under systemd) nor `shutdown` is available to reboot with",
            ));
        }
        if let Some(only_if) = &spec.only_if {
            let out = hostkit::probe(cx, only_if.to_command(cx.platform())?).await?;
            if !out.success() {
                return Ok(Planned::NoOp);
            }
        }
        Ok(Planned::change(Diff::Opaque {
            summary: format!(
                "reboot after the run (in {}, back within {})",
                humantime_serde::re::humantime::format_duration(spec.delay),
                humantime_serde::re::humantime::format_duration(spec.timeout)
            ),
        }))
    }

    #[instrument(name = "task.system.reboot.apply", skip_all)]
    async fn apply(&self, _cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        Ok(
            Applied::changed("reboot requested; the host reboots after the run")
                .with_reboot(RebootRequest::new(spec.delay, spec.timeout)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use buckle_core::system::FilePerms;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn systemd() -> FakeSystem {
        FakeSystem::new()
            .with_dir("/run/systemd/system", FilePerms::mode(0o755))
            .with_program("systemctl")
    }

    #[derive(Debug, PartialEq)]
    enum Class {
        Change,
        NoOp,
        Blocked,
    }

    #[rstest]
    #[case::unguarded(json!({}), systemd(), Class::Change)]
    #[case::guard_passes(
        json!({"only_if": ["test", "-f", "/run/reboot-required"]}),
        systemd().on_command_ok("test", &[], ""),
        Class::Change
    )]
    #[case::guard_fails(
        json!({"only_if": "test -f /run/reboot-required"}),
        systemd().on_command_fail("sh", &[], 1, ""),
        Class::NoOp
    )]
    #[case::guard_missing(json!({"only_if": ["needrestart", "-b"]}), systemd(), Class::NoOp)]
    #[case::no_reboot_tool(json!({}), FakeSystem::new(), Class::Blocked)]
    #[tokio::test]
    async fn plans(
        #[case] input: serde_json::Value,
        #[case] sys: FakeSystem,
        #[case] class: Class,
    ) {
        let s: Spec = spec(input);
        let planned = SystemReboot.plan(&cx(&sys), &s).await.unwrap();
        let got = match planned {
            Planned::NoOp => Class::NoOp,
            Planned::Blocked { .. } => Class::Blocked,
            Planned::Change { .. } => Class::Change,
        };
        assert_eq!(got, class);
    }

    #[tokio::test]
    async fn apply_asks_for_a_reboot_without_touching_the_host() {
        let sys = systemd();
        let s: Spec = spec(json!({"delay": "30s", "timeout": "10m"}));
        let applied = SystemReboot
            .apply(&cx(&sys), &s, &Planned::NoOp)
            .await
            .unwrap();
        assert!(applied.changed);
        let request = applied.reboot.expect("a reboot request");
        assert_eq!(request.delay, Duration::from_secs(30));
        assert_eq!(request.timeout, Duration::from_secs(600));
        assert!(sys.command_lines().is_empty());
    }

    #[test]
    fn defaults_and_the_unguarded_smell() {
        let s: Spec = spec(json!({}));
        assert_eq!(s.delay, Duration::from_secs(5));
        assert_eq!(s.timeout, Duration::from_secs(300));
        assert!(SystemReboot::always_runs(&s));
        let guarded: Spec = spec(json!({"only_if": "test -f /run/reboot-required"}));
        assert!(!SystemReboot::always_runs(&guarded));
        assert!(serde_json::from_value::<Spec>(json!({"delay": "5s", "after": "1m"})).is_err());
    }

    #[tokio::test]
    async fn removal_leaves_the_host_alone() {
        let sys = systemd();
        let record = record_of("system.reboot", "reboot", &Applied::default());
        assert!(
            !SystemReboot
                .remove(&cx(&sys), &record)
                .await
                .unwrap()
                .changed
        );
        assert!(sys.command_lines().is_empty());
    }
}
