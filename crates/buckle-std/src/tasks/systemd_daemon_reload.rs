//! `systemd.daemon_reload`: reload systemd's unit files (usually used as a handler).

use super::sys_exec::run_checked;
use super::systemd_common::systemctl;
use async_trait::async_trait;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, Planned};
use buckle_core::task::{Applied, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Runs `systemctl daemon-reload`.
///
/// Takes no fields. Intended as a handler (`handlers:` in a module, notified
/// by file or unit tasks): it always plans a change, and the engine runs each
/// handler once per module however many tasks notified it, before restarts
/// and reloads. There is nothing to undo on removal.
pub struct SystemdDaemonReload;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {}

#[async_trait]
impl Task for SystemdDaemonReload {
    type Spec = Spec;
    const KIND: &'static str = "systemd.daemon_reload";
    const LOCKS: &'static [&'static str] = &["systemd"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_DEFAULT: RemovalPolicy = RemovalPolicy::Leave;
    const DESCRIPTION: &'static str = "Run systemctl daemon-reload (a handler).";

    fn key(_spec: &Spec) -> String {
        "daemon-reload".into()
    }

    fn describe(_spec: &Spec) -> String {
        "systemctl daemon-reload".into()
    }

    #[tracing::instrument(name = "task.systemd.daemon_reload.plan", skip_all)]
    async fn plan(&self, cx: &Cx, _spec: &Spec) -> Result<Planned> {
        if let Some(blocked) = super::systemd_common::unavailable(cx).await? {
            return Ok(blocked);
        }
        Ok(Planned::change(Diff::Opaque {
            summary: "daemon-reload".into(),
        }))
    }

    #[tracing::instrument(name = "task.systemd.daemon_reload.apply", skip_all)]
    async fn apply(&self, cx: &Cx, _spec: &Spec, _planned: &Planned) -> Result<Applied> {
        run_checked(cx, systemctl(&["daemon-reload"])).await?;
        Ok(Applied::changed("systemctl daemon-reload"))
    }
}

buckle_core::register_task!(SystemdDaemonReload);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of};
    use buckle_core::testing::FakeSystem;

    #[tokio::test]
    async fn always_plans_a_change_and_reloads_on_apply() {
        let sys = FakeSystem::new()
            .with_dir(
                "/run/systemd/system",
                buckle_core::system::FilePerms::mode(0o755),
            )
            .on_command_ok("systemctl", &["daemon-reload"], "");
        let cx = cx(&sys);
        let planned = SystemdDaemonReload.plan(&cx, &Spec {}).await.unwrap();
        match &planned {
            Planned::Change {
                diff: Diff::Opaque { summary },
            } => assert_eq!(summary, "daemon-reload"),
            other => panic!("unexpected {other:?}"),
        }
        let applied = SystemdDaemonReload
            .apply(&cx, &Spec {}, &planned)
            .await
            .unwrap();
        assert!(applied.changed);
        assert_eq!(sys.command_lines(), vec!["systemctl daemon-reload"]);
        // Still a change the second time: handlers are not idempotent probes.
        assert!(
            SystemdDaemonReload
                .plan(&cx, &Spec {})
                .await
                .unwrap()
                .is_change()
        );
    }

    #[tokio::test]
    async fn failures_and_removal() {
        let sys = FakeSystem::new()
            .with_dir(
                "/run/systemd/system",
                buckle_core::system::FilePerms::mode(0o755),
            )
            .on_command_fail(
                "systemctl",
                &["daemon-reload"],
                1,
                "Failed to connect to bus",
            );
        let cx = cx(&sys);
        let err = SystemdDaemonReload
            .apply(&cx, &Spec {}, &Planned::NoOp)
            .await
            .unwrap_err();
        assert!(err.description().contains("Failed to connect to bus"));
        let record = record_of(
            "systemd.daemon_reload",
            "daemon-reload",
            &Applied::default(),
        );
        let removed = SystemdDaemonReload.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert_eq!(SystemdDaemonReload::key(&Spec {}), "daemon-reload");
    }
}
