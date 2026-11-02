//! `systemd.reload`: reload a unit's configuration (usually used as a handler).

use super::sys_exec::run_checked;
use super::systemd_common::systemctl;
use async_trait::async_trait;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, Planned};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Runs `systemctl reload-or-restart <name>`.
///
/// Intended as a handler: the unit re-reads its configuration without
/// dropping connections when it supports `ExecReload=`, and is restarted
/// when it does not. A restart handler for the same unit in the same module
/// makes this one redundant and the engine runs only the restart. It always
/// plans a change. There is nothing to undo on removal.
pub struct SystemdReload;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The unit to reload, e.g. `caddy.service`.
    pub name: String,
}

#[async_trait]
impl Task for SystemdReload {
    type Spec = Spec;
    const KIND: &'static str = "systemd.reload";
    const LOCKS: &'static [&'static str] = &["systemd"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_DEFAULT: RemovalPolicy = RemovalPolicy::Leave;
    const DESCRIPTION: &'static str =
        "Reload a systemd unit's configuration, restarting it if it cannot reload (a handler).";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        format!("reload {}", spec.name)
    }

    #[tracing::instrument(name = "task.systemd.reload.plan", skip_all, fields(unit = %spec.name))]
    async fn plan(&self, _cx: &Cx, spec: &Spec) -> Result<Planned> {
        Ok(Planned::change(Diff::Opaque {
            summary: format!("reload {}", spec.name),
        }))
    }

    #[tracing::instrument(name = "task.systemd.reload.apply", skip_all, fields(unit = %spec.name))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        run_checked(cx, systemctl(&["reload-or-restart", &spec.name])).await?;
        Ok(Applied::changed(format!("reloaded {}", spec.name)))
    }
}

buckle_core::register_task!(SystemdReload);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use buckle_core::testing::FakeSystem;
    use serde_json::json;

    #[tokio::test]
    async fn reloads_or_restarts_the_named_unit() {
        let sys = FakeSystem::new().on_command_ok("systemctl", &["reload-or-restart"], "");
        let cx = cx(&sys);
        let s: Spec = spec(json!({"name": "caddy"}));
        let planned = SystemdReload.plan(&cx, &s).await.unwrap();
        assert!(planned.is_change());
        let applied = SystemdReload.apply(&cx, &s, &planned).await.unwrap();
        assert!(applied.changed);
        assert_eq!(
            sys.command_lines(),
            vec!["systemctl reload-or-restart caddy"]
        );
    }

    #[tokio::test]
    async fn removal_is_a_noop() {
        let sys = FakeSystem::new();
        let cx = cx(&sys);
        let record = record_of("systemd.reload", "caddy", &Applied::default());
        let removed = SystemdReload.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert!(sys.command_lines().is_empty());
    }
}
