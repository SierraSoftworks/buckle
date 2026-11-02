//! `systemd.restart`: restart a unit (usually used as a handler).

use super::sys_exec::run_checked;
use super::systemd_common::systemctl;
use async_trait::async_trait;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, Planned};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Runs `systemctl restart <name>`.
///
/// Intended as a handler notified by the tasks that change the unit's files or
/// configuration; the engine runs each handler once per module and a restart
/// subsumes a reload of the same unit. It always plans a change. Restarting a
/// unit that is not running starts it. There is nothing to undo on removal.
pub struct SystemdRestart;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The unit to restart, e.g. `nomad.service`.
    pub name: String,
}

#[async_trait]
impl Task for SystemdRestart {
    type Spec = Spec;
    const KIND: &'static str = "systemd.restart";
    const LOCKS: &'static [&'static str] = &["systemd"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_DEFAULT: RemovalPolicy = RemovalPolicy::Leave;
    const DESCRIPTION: &'static str = "Restart a systemd unit (a handler).";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        format!("restart {}", spec.name)
    }

    #[tracing::instrument(name = "task.systemd.restart.plan", skip_all, fields(unit = %spec.name))]
    async fn plan(&self, _cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(Planned::change(Diff::Opaque {
            summary: format!("restart {}", spec.name),
        }))
    }

    #[tracing::instrument(name = "task.systemd.restart.apply", skip_all, fields(unit = %spec.name))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        run_checked(cx, systemctl(&["restart", &spec.name])).await?;
        Ok(Applied::changed(format!("restarted {}", spec.name)))
    }
}

buckle_core::register_task!(SystemdRestart);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use buckle_core::testing::FakeSystem;
    use serde_json::json;

    #[tokio::test]
    async fn restarts_the_named_unit() {
        let sys = FakeSystem::new().on_command_ok("systemctl", &["restart"], "");
        let cx = cx(&sys);
        let s: Spec = spec(json!({"name": "nomad.service"}));
        let planned = SystemdRestart.plan(&cx, &s).await.unwrap();
        assert_eq!(planned.is_change(), true);
        let applied = SystemdRestart.apply(&cx, &s, &planned).await.unwrap();
        assert_eq!(applied.summary, "restarted nomad.service");
        assert_eq!(sys.command_lines(), vec!["systemctl restart nomad.service"]);
        assert_eq!(SystemdRestart::key(&s), "nomad.service");
    }

    #[tokio::test]
    async fn failures_surface_and_removal_is_a_noop() {
        let sys = FakeSystem::new().on_command_fail(
            "systemctl",
            &["restart"],
            5,
            "Failed to restart nomad.service: Unit nomad.service not found.",
        );
        let cx = cx(&sys);
        let s: Spec = spec(json!({"name": "nomad.service"}));
        let err = SystemdRestart
            .apply(&cx, &s, &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("not found"));
        let record = record_of("systemd.restart", "nomad.service", &Applied::default());
        assert!(!SystemdRestart.remove(&cx, &record).await.unwrap().changed);
    }
}
