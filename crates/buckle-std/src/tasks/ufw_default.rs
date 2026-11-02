//! `ufw.default`: a default firewall policy.

use super::sys_exec::{legacy_ledger, run_checked};
use super::ufw_common::{installed, read_status, ufw};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Sets the default policy for a traffic direction (`ufw default <policy> <direction>`).
///
/// `direction` is `incoming`, `outgoing` or `routed`; `policy` is `allow`,
/// `deny` or `reject`. The current policy is read from `ufw status verbose`
/// (or from `/etc/default/ufw` while the firewall is inactive).
///
/// Removal restores the policy that was in place before Buckle first changed
/// it, and does nothing if Buckle never changed it.
pub struct UfwDefault;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// `incoming`, `outgoing` or `routed`.
    pub direction: Direction,
    /// `allow`, `deny` or `reject`.
    pub policy: Policy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Incoming,
    Outgoing,
    Routed,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Incoming => "incoming",
            Direction::Outgoing => "outgoing",
            Direction::Routed => "routed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    Allow,
    Deny,
    Reject,
}

impl Policy {
    fn as_str(self) -> &'static str {
        match self {
            Policy::Allow => "allow",
            Policy::Deny => "deny",
            Policy::Reject => "reject",
        }
    }
}

const PREVIOUS: &str = "ufw default ";

#[async_trait]
impl Task for UfwDefault {
    type Spec = Spec;
    const KIND: &'static str = "ufw.default";
    const LOCKS: &'static [&'static str] = &["ufw"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str = "Set a ufw default policy (incoming, outgoing or routed).";

    fn key(spec: &Spec) -> String {
        spec.direction.as_str().into()
    }

    fn describe(spec: &Spec) -> String {
        format!(
            "default {} {}",
            spec.policy.as_str(),
            spec.direction.as_str()
        )
    }

    #[tracing::instrument(name = "task.ufw.default.plan", skip_all, fields(direction = spec.direction.as_str()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        let summary = format!(
            "ufw default {} {}",
            spec.policy.as_str(),
            spec.direction.as_str()
        );
        if !installed(cx).await? {
            return Ok(Planned::change(Diff::Opaque {
                summary: format!("{summary} (ufw is not installed yet)"),
            }));
        }
        let status = read_status(cx, false).await?;
        match status.defaults.get(spec.direction.as_str()) {
            Some(current) if current == spec.policy.as_str() => Ok(Planned::NoOp),
            current => Ok(Planned::change(Diff::Opaque {
                summary: format!(
                    "{summary} (now {})",
                    current.map(String::as_str).unwrap_or("unknown")
                ),
            })),
        }
    }

    #[tracing::instrument(name = "task.ufw.default.apply", skip_all, fields(direction = spec.direction.as_str()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let dir = spec.direction.as_str();
        let status = read_status(cx, false).await?;
        // Remember the policy from before Buckle ever changed it: from this
        // resource's record, else (records written before the ledger was
        // retired) from the legacy ledger, else the policy in force now.
        let before = match cx.previous.as_deref().and_then(previous) {
            Some((_, policy)) => Some(policy),
            None if cx.previous.is_some() => legacy_ledger(cx, "ufw")
                .await?
                .remove(&format!("default.{dir}"))
                .or_else(|| status.defaults.get(dir).cloned()),
            None => status.defaults.get(dir).cloned(),
        };
        run_checked(cx, ufw(&["default", spec.policy.as_str(), dir])).await?;
        let mut applied = Applied::changed(format!("ufw default {} {}", spec.policy.as_str(), dir));
        if let Some(previous) = before {
            applied = applied.with_artefact(Artefact::Other {
                description: format!("{PREVIOUS}{dir} previous={previous}"),
            });
        }
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        match previous(record) {
            Some((dir, policy)) => {
                format!("ufw default {policy} {dir} (restoring the previous policy)")
            }
            None => format!(
                "leave the {} policy as it is (Buckle did not change it)",
                record.key.key
            ),
        }
    }

    #[tracing::instrument(name = "task.ufw.default.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some((dir, policy)) = previous(record) else {
            return Ok(Applied::skipped(
                "Buckle did not change this policy; left as it is",
            ));
        };
        run_checked(cx, ufw(&["default", &policy, &dir])).await?;
        Ok(Applied::changed(format!(
            "restored ufw default {policy} {dir}"
        )))
    }
}

/// `(direction, previous policy)` from the record's artefacts.
fn previous(record: &ResourceRecord) -> Option<(String, String)> {
    record.artefacts.iter().find_map(|a| match a {
        Artefact::Other { description } => {
            let rest = description.strip_prefix(PREVIOUS)?;
            let (dir, policy) = rest.split_once(" previous=")?;
            Some((dir.to_owned(), policy.to_owned()))
        }
        _ => None,
    })
}

buckle_core::register_task!(UfwDefault);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::{FakeExt, put};
    use crate::tasks::sys_exec::testkit::{cx, cx_after, record_of, spec};
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
    #[case(json!({"direction": "incoming", "policy": "deny"}), true)]
    #[case(json!({"direction": "outgoing", "policy": "allow"}), true)]
    #[case(json!({"direction": "routed", "policy": "deny"}), true)]
    #[case(json!({"direction": "incoming", "policy": "allow"}), false)]
    #[case(json!({"direction": "outgoing", "policy": "deny"}), false)]
    #[case(json!({"direction": "routed", "policy": "reject"}), false)]
    #[tokio::test]
    async fn plan_compares_the_status_line(
        #[case] spec_json: serde_json::Value,
        #[case] noop: bool,
    ) {
        let (_sys, cx) = host(&Ufw::new().active());
        let planned = UfwDefault.plan(&cx, &spec(spec_json)).await.unwrap();
        assert_eq!(planned == Planned::NoOp, noop, "{planned:?}");
    }

    #[tokio::test]
    async fn inactive_firewalls_are_read_from_etc_default_ufw() {
        let ufw = Ufw::new();
        let sys = ufw.attach(FakeSystem::new()).with_file(
            "/etc/default/ufw",
            "DEFAULT_INPUT_POLICY=\"ACCEPT\"\nDEFAULT_OUTPUT_POLICY=\"ACCEPT\"\nDEFAULT_FORWARD_POLICY=\"DROP\"\n",
            FilePerms::default(),
        );
        let cx = cx(&sys);
        let deny: Spec = spec(json!({"direction": "incoming", "policy": "deny"}));
        assert!(UfwDefault.plan(&cx, &deny).await.unwrap().is_change());
        let allow: Spec = spec(json!({"direction": "incoming", "policy": "allow"}));
        assert_eq!(UfwDefault.plan(&cx, &allow).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn apply_sets_the_policy_and_remembers_the_old_one() {
        let ufw = Ufw::new().active().default_policy("incoming", "allow");
        let (sys, cx) = host(&ufw);
        let s: Spec = spec(json!({"direction": "incoming", "policy": "deny"}));
        let applied = UfwDefault.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(
            sys.command_lines()
                .contains(&"ufw default deny incoming".to_owned())
        );
        assert_eq!(ufw.snapshot().defaults["incoming"], "deny");
        assert_eq!(
            applied.artefacts,
            vec![Artefact::Other {
                description: "ufw default incoming previous=allow".into()
            }]
        );
        assert_eq!(UfwDefault.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn removal_restores_the_original_policy_even_after_a_second_change() {
        let ufw = Ufw::new().active().default_policy("incoming", "allow");
        let (sys, cx) = host(&ufw);
        let first = UfwDefault
            .apply(
                &cx,
                &spec(json!({"direction": "incoming", "policy": "deny"})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        // The author later switches to reject; the original (allow) is still what we restore.
        let applied = UfwDefault
            .apply(
                &cx_after(&sys, record_of("ufw.default", "incoming", &first)),
                &spec(json!({"direction": "incoming", "policy": "reject"})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        let record = record_of("ufw.default", "incoming", &applied);
        assert_eq!(
            UfwDefault.describe_removal(&record),
            "ufw default allow incoming (restoring the previous policy)"
        );
        UfwDefault.remove(&cx, &record).await.unwrap();
        assert_eq!(ufw.snapshot().defaults["incoming"], "allow");
    }

    #[tokio::test]
    async fn a_record_from_before_the_ledger_was_retired_is_migrated_once() {
        let ufw = Ufw::new().active().default_policy("incoming", "deny");
        let (sys, _cx) = host(&ufw);
        // The old code kept "allow" (the policy before Buckle) only in the ledger.
        put(
            &sys,
            "/var/lib/buckle/ledger/ufw.json",
            r#"{"default.incoming":"allow"}"#,
        )
        .await;
        let old = record_of("ufw.default", "incoming", &Applied::changed("old"));
        let applied = UfwDefault
            .apply(
                &cx_after(&sys, old),
                &spec(json!({"direction": "incoming", "policy": "reject"})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        let record = record_of("ufw.default", "incoming", &applied);
        assert_eq!(
            UfwDefault.describe_removal(&record),
            "ufw default allow incoming (restoring the previous policy)"
        );
        // From now on the record is authoritative; the ledger is not consulted or written.
        sys.remove_for_test("/var/lib/buckle/ledger/ufw.json").await;
        let next = UfwDefault
            .apply(
                &cx_after(&sys, record),
                &spec(json!({"direction": "incoming", "policy": "deny"})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        assert!(
            UfwDefault
                .describe_removal(&record_of("ufw.default", "incoming", &next))
                .contains("allow incoming")
        );
        assert!(!sys.exists("/var/lib/buckle/ledger/ufw.json"));
    }

    #[tokio::test]
    async fn removal_without_a_recorded_change_does_nothing() {
        let ufw = Ufw::new().active();
        let (sys, cx) = host(&ufw);
        let record = record_of("ufw.default", "incoming", &Applied::default());
        let removed = UfwDefault.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert!(sys.command_lines().is_empty());
    }
}
