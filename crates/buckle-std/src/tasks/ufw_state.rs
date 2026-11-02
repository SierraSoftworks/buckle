//! `ufw.state`: enable or disable the firewall and set its logging level.

use super::sys_exec::{legacy_ledger, run_checked};
use super::ufw_common::{installed, read_status, ufw};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Enables or disables ufw and optionally sets its logging level.
///
/// `enabled: true` runs `ufw --force enable` (no confirmation prompt), so
/// order it after the `ufw.rule` that keeps SSH reachable. `logging` is one
/// of `off`, `on`, `low`, `medium`, `high` or `full`; `on` accepts whatever
/// level is already active. When ufw is inactive its status command says
/// nothing about logging, so the level is read from `/etc/ufw/ufw.conf`.
///
/// Removal puts back what was there before Buckle first changed it: it
/// disables ufw if Buckle enabled it, and restores the previous logging level.
pub struct UfwState;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Whether the firewall is enabled.
    pub enabled: bool,
    /// Logging level: `off`, `on`, `low`, `medium`, `high` or `full`.
    #[serde(default)]
    pub logging: Option<String>,
}

const LEVELS: &[&str] = &["off", "on", "low", "medium", "high", "full"];
const ENABLED_PREFIX: &str = "ufw state previous enabled=";
const LOGGING_PREFIX: &str = "ufw state previous logging=";

fn validate(spec: &Spec) -> Result<()> {
    if let Some(l) = &spec.logging
        && !LEVELS.contains(&l.as_str())
    {
        return Err(buckle_core::error::user(
            format!(
                "ufw.state: logging '{l}' is not one of {}.",
                LEVELS.join(", ")
            ),
            &["Use off, on, low, medium, high or full."],
        ));
    }
    Ok(())
}

fn logging_matches(current: &str, wanted: &str) -> bool {
    current == wanted || (wanted == "on" && current != "off")
}

#[async_trait]
impl Task for UfwState {
    type Spec = Spec;
    const KIND: &'static str = "ufw.state";
    const LOCKS: &'static [&'static str] = &["ufw"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str =
        "Enable or disable the ufw firewall and set its logging level.";

    fn key(_spec: &Spec) -> String {
        "state".into()
    }

    fn describe(spec: &Spec) -> String {
        let mut s = format!("ufw {}", if spec.enabled { "enabled" } else { "disabled" });
        if let Some(l) = &spec.logging {
            s.push_str(&format!(", logging {l}"));
        }
        s
    }

    #[tracing::instrument(name = "task.ufw.state.plan", skip_all)]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        validate(spec)?;
        if !installed(cx).await? {
            return Ok(Planned::change(Diff::Opaque {
                summary: format!("{} (ufw is not installed yet)", Self::describe(spec)),
            }));
        }
        let status = read_status(cx, false).await?;
        let mut changes = Vec::new();
        if status.active != spec.enabled {
            changes.push(
                if spec.enabled {
                    "enable ufw"
                } else {
                    "disable ufw"
                }
                .to_owned(),
            );
        }
        if let Some(wanted) = &spec.logging {
            match &status.logging {
                Some(current) if logging_matches(current, wanted) => {}
                current => changes.push(format!(
                    "logging {} (now {})",
                    wanted,
                    current.as_deref().unwrap_or("unknown")
                )),
            }
        }
        Ok(if changes.is_empty() {
            Planned::NoOp
        } else {
            Planned::change(Diff::Opaque {
                summary: changes.join(", "),
            })
        })
    }

    #[tracing::instrument(name = "task.ufw.state.apply", skip_all)]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        validate(spec)?;
        let status = read_status(cx, false).await?;
        // What was there before Buckle first changed it: from this resource's
        // record, else (records written before the ledger was retired) from
        // the legacy ledger.
        let (mut was_enabled, mut was_logging) =
            cx.previous.as_deref().map(previous).unwrap_or_default();
        if cx.previous.is_some() && (was_enabled.is_none() || was_logging.is_none()) {
            let legacy = legacy_ledger(cx, "ufw").await?;
            was_enabled =
                was_enabled.or_else(|| legacy.get("state.enabled").and_then(|v| v.parse().ok()));
            was_logging = was_logging.or_else(|| legacy.get("state.logging").cloned());
        }
        let mut done = Vec::new();

        if let Some(wanted) = &spec.logging
            && !status
                .logging
                .as_deref()
                .is_some_and(|c| logging_matches(c, wanted))
        {
            if was_logging.is_none() {
                was_logging = status.logging.clone();
            }
            run_checked(cx, ufw(&["logging", wanted])).await?;
            done.push(format!("logging {wanted}"));
        }
        if status.active != spec.enabled {
            if was_enabled.is_none() {
                was_enabled = Some(status.active);
            }
            if spec.enabled {
                run_checked(cx, ufw(&["--force", "enable"])).await?;
                done.push("enabled ufw".into());
            } else {
                run_checked(cx, ufw(&["--force", "disable"])).await?;
                done.push("disabled ufw".into());
            }
        }
        let mut applied = if done.is_empty() {
            Applied::unchanged("ufw already in the desired state")
        } else {
            Applied::changed(done.join(", "))
        };
        if let Some(prev) = was_enabled {
            applied = applied.with_artefact(Artefact::Other {
                description: format!("{ENABLED_PREFIX}{prev}"),
            });
        }
        if let Some(prev) = was_logging {
            applied = applied.with_artefact(Artefact::Other {
                description: format!("{LOGGING_PREFIX}{prev}"),
            });
        }
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        let (enabled, logging) = previous(record);
        let mut parts = Vec::new();
        if enabled == Some(false) {
            parts.push("disable ufw".to_owned());
        }
        if let Some(l) = logging {
            parts.push(format!("restore logging {l}"));
        }
        if parts.is_empty() {
            "leave ufw as it is (Buckle did not change its state)".into()
        } else {
            parts.join(", ")
        }
    }

    #[tracing::instrument(name = "task.ufw.state.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let (enabled, logging) = previous(record);
        let mut done = Vec::new();
        if let Some(l) = logging {
            run_checked(cx, ufw(&["logging", &l])).await?;
            done.push(format!("restored logging {l}"));
        }
        // Only a firewall Buckle switched on is switched off again; one that
        // was already on stays on whatever Buckle asked.
        if enabled == Some(false) {
            run_checked(cx, ufw(&["--force", "disable"])).await?;
            done.push("disabled ufw".to_owned());
        }
        if done.is_empty() {
            Ok(Applied::skipped(
                "Buckle did not change the firewall state; left as it is",
            ))
        } else {
            Ok(Applied::changed(done.join(", ")))
        }
    }
}

/// `(previous enabled, previous logging)` from the record.
fn previous(record: &ResourceRecord) -> (Option<bool>, Option<String>) {
    let mut enabled = None;
    let mut logging = None;
    for a in &record.artefacts {
        if let Artefact::Other { description } = a {
            if let Some(v) = description.strip_prefix(ENABLED_PREFIX) {
                enabled = v.parse().ok();
            } else if let Some(v) = description.strip_prefix(LOGGING_PREFIX) {
                logging = Some(v.to_owned());
            }
        }
    }
    (enabled, logging)
}

buckle_core::register_task!(UfwState);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use crate::tasks::ufw_common::testkit::Ufw;
    use buckle_core::system::FilePerms;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn host(ufw: &Ufw) -> (FakeSystem, Cx) {
        let sys = ufw.attach(FakeSystem::new());
        let cx = cx(&sys);
        (sys, cx)
    }

    #[rstest]
    #[case(json!({"enabled": true}), true, true)]
    #[case(json!({"enabled": true, "logging": "low"}), true, true)]
    #[case(json!({"enabled": true, "logging": "on"}), true, true)]
    #[case(json!({"enabled": true, "logging": "high"}), true, false)]
    #[case(json!({"enabled": false}), true, false)]
    #[case(json!({"enabled": true}), false, false)]
    #[tokio::test]
    async fn plan_compares_status_and_logging(
        #[case] spec_json: serde_json::Value,
        #[case] active: bool,
        #[case] noop: bool,
    ) {
        let ufw = if active {
            Ufw::new().active()
        } else {
            Ufw::new()
        };
        let sys = ufw.attach(FakeSystem::new());
        let cx = cx(&sys);
        let planned = UfwState.plan(&cx, &spec(spec_json)).await.unwrap();
        assert_eq!(planned == Planned::NoOp, noop, "{planned:?}");
    }

    #[tokio::test]
    async fn inactive_logging_comes_from_ufw_conf() {
        let ufw = Ufw::new();
        let sys = ufw.attach(FakeSystem::new()).with_file(
            "/etc/ufw/ufw.conf",
            "ENABLED=no\nLOGLEVEL=medium\n",
            FilePerms::default(),
        );
        let cx = cx(&sys);
        let planned = UfwState
            .plan(&cx, &spec(json!({"enabled": false, "logging": "medium"})))
            .await
            .unwrap();
        assert_eq!(planned, Planned::NoOp);
    }

    #[tokio::test]
    async fn invalid_logging_is_a_user_error() {
        let (_sys, cx) = host(&Ufw::new());
        let err = UfwState
            .plan(&cx, &spec(json!({"enabled": true, "logging": "loud"})))
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }

    #[tokio::test]
    async fn apply_enables_with_force_and_sets_logging_first() {
        let ufw = Ufw::new();
        let sys = ufw.attach(FakeSystem::new()).with_file(
            "/etc/ufw/ufw.conf",
            "ENABLED=no\nLOGLEVEL=low\n",
            FilePerms::default(),
        );
        let cx = cx(&sys);
        let s: Spec = spec(json!({"enabled": true, "logging": "medium"}));
        let applied = UfwState.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let ufw_lines: Vec<String> = sys
            .command_lines()
            .into_iter()
            .filter(|l| !l.starts_with("ufw status"))
            .collect();
        assert_eq!(ufw_lines, vec!["ufw logging medium", "ufw --force enable"]);
        let world = ufw.snapshot();
        assert!(world.active);
        assert_eq!(world.logging, "medium");
        assert_eq!(applied.artefacts.len(), 2);
        assert_eq!(UfwState.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn disabling_an_active_firewall() {
        let ufw = Ufw::new().active();
        let (sys, cx) = host(&ufw);
        let s: Spec = spec(json!({"enabled": false}));
        UfwState.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(
            sys.command_lines()
                .contains(&"ufw --force disable".to_owned())
        );
        assert!(!ufw.snapshot().active);
    }

    #[tokio::test]
    async fn removal_reverts_only_what_buckle_changed() {
        let ufw = Ufw::new();
        let sys = ufw.attach(FakeSystem::new()).with_file(
            "/etc/ufw/ufw.conf",
            "ENABLED=no\nLOGLEVEL=low\n",
            FilePerms::default(),
        );
        let cx = cx(&sys);
        let s: Spec = spec(json!({"enabled": true, "logging": "high"}));
        let applied = UfwState.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let record = record_of("ufw.state", "state", &applied);
        assert_eq!(
            UfwState.describe_removal(&record),
            "disable ufw, restore logging low"
        );
        UfwState.remove(&cx, &record).await.unwrap();
        let world = ufw.snapshot();
        assert!(!world.active);
        assert_eq!(world.logging, "low");
    }

    #[tokio::test]
    async fn removal_leaves_a_firewall_that_was_already_on() {
        let ufw = Ufw::new().active();
        let (sys, cx) = host(&ufw);
        let s: Spec = spec(json!({"enabled": true}));
        let applied = UfwState.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(!applied.changed);
        let record = record_of("ufw.state", "state", &applied);
        let before = sys.commands_run().len();
        let removed = UfwState.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert_eq!(sys.commands_run().len(), before);
        assert!(ufw.snapshot().active);
    }
}
