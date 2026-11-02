//! `user.group`: manage a local group.

use super::hostkit::{self, Ensure};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::system::Command;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::instrument;

/// Ensures a local group exists (or is absent).
///
/// ```yaml
/// - user.group:
///     name: nomad
///     gid: 4646          # optional
///     system: true       # a system group (low gid)
///     state: present     # or absent
/// ```
///
/// Uses `groupadd`, `groupmod` and `groupdel`. Removal is flagged as a
/// cautionary change: when the task leaves the configuration the group is
/// deleted, but only if Buckle created it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Spec {
    pub name: String,
    #[serde(default)]
    pub gid: Option<u32>,
    #[serde(default)]
    pub system: bool,
    #[serde(default)]
    pub state: Ensure,
}

pub struct UserGroup;
buckle_core::register_task!(UserGroup);

pub(crate) fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$'));
    if ok {
        Ok(())
    } else {
        Err(buckle_core::error::user(
            format!("'{name}' is not a valid user or group name."),
            &["Use letters, digits, '_', '-' and '.', not starting with '-'."],
        ))
    }
}

#[async_trait]
impl Task for UserGroup {
    type Spec = Spec;
    const KIND: &'static str = "user.group";
    const LOCKS: &'static [&'static str] = &["users"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const REMOVAL_CAUTION: bool = true;
    const DESCRIPTION: &'static str =
        "Ensure a local group exists with the given gid, or is absent.";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        match spec.state {
            Ensure::Present => format!("group {}", spec.name),
            Ensure::Absent => format!("group {} absent", spec.name),
        }
    }

    #[instrument(name = "task.user.group.plan", skip_all, fields(group = %spec.name, state = ?spec.state))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        valid_name(&spec.name)?;
        let tool = match spec.state {
            Ensure::Present => "groupadd",
            Ensure::Absent => "groupdel",
        };
        if let Some(blocked) =
            hostkit::missing_tool(cx, tool, "install the `passwd` package (shadow-utils)").await?
        {
            return Ok(blocked);
        }
        let current = cx.system.lookup_group(&spec.name).await?;
        Ok(match (spec.state, current) {
            (Ensure::Present, None) => Planned::change(Diff::Create {
                summary: match spec.gid {
                    Some(gid) => format!("create group {} (gid {gid})", spec.name),
                    None => format!("create group {}", spec.name),
                },
                preview: None,
            }),
            (Ensure::Present, Some(group)) => match spec.gid {
                Some(gid) if gid != group.gid => Planned::change(Diff::Update {
                    summary: format!("group {}: gid {} → {gid}", spec.name, group.gid),
                    unified: None,
                }),
                _ => Planned::NoOp,
            },
            (Ensure::Absent, Some(_)) => Planned::change(Diff::Delete {
                summary: format!("delete group {}", spec.name),
            }),
            (Ensure::Absent, None) => Planned::NoOp,
        })
    }

    #[instrument(name = "task.user.group.apply", skip_all, fields(group = %spec.name, state = ?spec.state))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        valid_name(&spec.name)?;
        let current = cx.system.lookup_group(&spec.name).await?;
        match (spec.state, current) {
            (Ensure::Present, None) => {
                hostkit::require_tool(cx, "groupadd", "install the `passwd` package").await?;
                let mut cmd = Command::new("groupadd");
                if spec.system {
                    cmd = cmd.arg("-r");
                }
                if let Some(gid) = spec.gid {
                    cmd = cmd.arg("-g").arg(gid.to_string());
                }
                hostkit::run_checked(cx, cmd.arg(&spec.name), "Creating the group").await?;
                Ok(
                    Applied::changed(format!("created group {}", spec.name)).with_artefact(
                        Artefact::Group {
                            name: spec.name.clone(),
                        },
                    ),
                )
            }
            (Ensure::Present, Some(group)) => match spec.gid {
                Some(gid) if gid != group.gid => {
                    hostkit::require_tool(cx, "groupmod", "install the `passwd` package").await?;
                    let cmd = Command::new("groupmod")
                        .arg("-g")
                        .arg(gid.to_string())
                        .arg(&spec.name);
                    hostkit::run_checked(cx, cmd, "Changing the group's gid").await?;
                    Ok(Applied::changed(format!(
                        "changed the gid of group {}",
                        spec.name
                    )))
                }
                _ => Ok(Applied::unchanged(format!(
                    "group {} already up to date",
                    spec.name
                ))),
            },
            (Ensure::Absent, Some(_)) => {
                hostkit::require_tool(cx, "groupdel", "install the `passwd` package").await?;
                hostkit::run_checked(
                    cx,
                    Command::new("groupdel").arg(&spec.name),
                    "Deleting the group",
                )
                .await?;
                Ok(Applied::changed(format!("deleted group {}", spec.name)))
            }
            (Ensure::Absent, None) => Ok(Applied::unchanged(format!(
                "group {} was already absent",
                spec.name
            ))),
        }
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!(
            "delete group {} (only if Buckle created it)",
            record.key.key
        )
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some(name) = record.artefacts.iter().find_map(|a| match a {
            Artefact::Group { name } => Some(name.clone()),
            _ => None,
        }) else {
            return Ok(Applied::skipped(
                "the group existed before Buckle managed it, left in place",
            ));
        };
        if cx.system.lookup_group(&name).await?.is_none() {
            return Ok(Applied::unchanged(format!(
                "group {name} was already absent"
            )));
        }
        hostkit::run_checked(
            cx,
            Command::new("groupdel").arg(&name),
            "Deleting the group",
        )
        .await?;
        Ok(Applied::changed(format!("deleted group {name}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::hostkit::testkit::*;
    use buckle_core::system::GroupInfo;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn group(name: &str, gid: u32) -> GroupInfo {
        GroupInfo {
            name: name.into(),
            gid,
            members: vec![],
        }
    }

    #[rstest]
    #[case::missing(with_account_tools(FakeSystem::new()), json!({}), Class::Change)]
    #[case::present(with_account_tools(FakeSystem::new().with_group(group("app", 500))), json!({}), Class::NoOp)]
    #[case::gid_matches(with_account_tools(FakeSystem::new().with_group(group("app", 500))), json!({"gid": 500}), Class::NoOp)]
    #[case::gid_differs(with_account_tools(FakeSystem::new().with_group(group("app", 500))), json!({"gid": 600}), Class::Change)]
    #[case::absent_present(with_account_tools(FakeSystem::new().with_group(group("app", 500))), json!({"state": "absent"}), Class::Change)]
    #[case::absent_missing(with_account_tools(FakeSystem::new()), json!({"state": "absent"}), Class::NoOp)]
    #[case::tool_missing(FakeSystem::new(), json!({}), Class::Blocked)]
    #[tokio::test]
    async fn plan_outcomes(
        #[case] sys: FakeSystem,
        #[case] extra: serde_json::Value,
        #[case] expected: Class,
    ) {
        let mut raw = json!({"name": "app"});
        raw.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert_eq!(
            plan_class(&UserGroup, &cx(&sys), &spec(raw)).await,
            expected
        );
    }

    #[tokio::test]
    async fn creating_a_group_is_idempotent_and_records_the_artefact() {
        let sys = with_account_tools(FakeSystem::new());
        let c = cx(&sys);
        let s: Spec = spec(json!({"name": "app", "gid": 4646, "system": true}));
        let applied = apply(&UserGroup, &c, &s).await;
        assert!(applied.changed);
        assert_eq!(
            applied.artefacts,
            vec![Artefact::Group { name: "app".into() }]
        );
        assert_eq!(sys.command_lines(), vec!["groupadd -r -g 4646 app"]);
        assert_eq!(plan_class(&UserGroup, &c, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn changing_the_gid_uses_groupmod() {
        let sys = with_account_tools(FakeSystem::new().with_group(group("app", 500)));
        let c = cx(&sys);
        let s: Spec = spec(json!({"name": "app", "gid": 600}));
        apply(&UserGroup, &c, &s).await;
        assert_eq!(sys.command_lines(), vec!["groupmod -g 600 app"]);
        assert_eq!(plan_class(&UserGroup, &c, &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn absent_deletes_the_group() {
        let sys = with_account_tools(FakeSystem::new().with_group(group("app", 500)));
        let s: Spec = spec(json!({"name": "app", "state": "absent"}));
        apply(&UserGroup, &cx(&sys), &s).await;
        assert_eq!(sys.command_lines(), vec!["groupdel app"]);
        // Post-state: the group is gone.
        let after = with_account_tools(FakeSystem::new());
        assert_eq!(plan_class(&UserGroup, &cx(&after), &s).await, Class::NoOp);
    }

    #[tokio::test]
    async fn removal_only_deletes_groups_buckle_created() {
        let sys = with_account_tools(FakeSystem::new());
        let c = cx(&sys);
        let created = record(
            "user.group",
            "app",
            &apply(&UserGroup, &c, &spec(json!({"name": "app"}))).await,
        );
        assert!(UserGroup::REMOVAL_CAUTION);
        assert!(UserGroup.remove(&c, &created).await.unwrap().changed);
        assert!(sys.command_lines().iter().any(|l| l == "groupdel app"));

        let sys = with_account_tools(FakeSystem::new().with_group(group("wheel", 10)));
        let c = cx(&sys);
        let adopted: Spec = spec(json!({"name": "wheel", "gid": 10}));
        let rec = record("user.group", "wheel", &Applied::unchanged("adopted"));
        let _ = adopted;
        let out = UserGroup.remove(&c, &rec).await.unwrap();
        assert!(!out.changed && out.summary.contains("existed before"));
        assert!(sys.command_lines().is_empty());
    }

    #[test]
    fn invalid_names_are_rejected() {
        for bad in ["", "-x", "a b", "a:b", "x;rm"] {
            assert!(valid_name(bad).is_err(), "{bad}");
        }
        assert!(valid_name("nomad-1").is_ok());
    }
}
