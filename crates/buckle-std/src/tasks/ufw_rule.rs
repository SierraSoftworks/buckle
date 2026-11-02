//! `ufw.rule`: one firewall rule.

use super::sys_exec::{Scalar, run_checked};
use super::ufw_common::{Rule, installed, read_status, ufw};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Adds a ufw rule.
///
/// `rule` is `allow`, `deny`, `reject` or `limit`; `direction` is `in`
/// (default) or `out`. The selectors are `to_port` (a port, list `80,443` or
/// range `8000:8100`), `proto` (`tcp`, `udp` or `any`), `from_ip`, `to_ip`
/// (addresses or CIDR networks) and `interface`; at least one is required, so
/// that a rule cannot accidentally open everything. `comment` is stored with
/// the rule. A list or range of ports needs `proto`.
///
/// A rule is identified by its direction, protocol, port, addresses and
/// interface (not its action or comment). If a rule with the same selectors
/// but a different action exists, applying deletes it and adds this one. A
/// comment-only difference is not a change. Rules are added whether or not
/// ufw is enabled; enable the firewall with `ufw.state` after the rules that
/// keep you connected (SSH first).
///
/// Removal deletes exactly this rule (`ufw delete`), v4 and v6 together.
pub struct UfwRule;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// `allow`, `deny`, `reject` or `limit` (rate-limit, for brute-force protection).
    pub rule: Action,
    /// `in` (default) or `out`.
    #[serde(default)]
    pub direction: Direction,
    /// Destination port, list or range.
    #[serde(default)]
    pub to_port: Option<Scalar>,
    /// `tcp`, `udp` or `any`.
    #[serde(default)]
    pub proto: Option<Proto>,
    /// Source address or network.
    #[serde(default)]
    pub from_ip: Option<String>,
    /// Destination address or network.
    #[serde(default)]
    pub to_ip: Option<String>,
    /// Network interface the rule applies to.
    #[serde(default)]
    pub interface: Option<String>,
    /// A comment stored with the rule.
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Deny,
    Reject,
    Limit,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
    Any,
}

impl Spec {
    fn rule(&self) -> Result<Rule> {
        let invalid = |msg: &str| {
            buckle_core::error::user(
                format!("ufw.rule: {msg}"),
                &["See `buckle catalog ufw.rule` for the fields this task accepts."],
            )
        };
        let port = self.to_port.as_ref().map(Scalar::text);
        let proto = match self.proto {
            Some(Proto::Tcp) => Some("tcp".to_owned()),
            Some(Proto::Udp) => Some("udp".to_owned()),
            Some(Proto::Any) | None => None,
        };
        if port.is_none()
            && proto.is_none()
            && self.from_ip.is_none()
            && self.to_ip.is_none()
            && self.interface.is_none()
        {
            return Err(invalid(
                "give at least one of to_port, proto, from_ip, to_ip or interface.",
            ));
        }
        if let Some(p) = &port
            && (p.contains(',') || p.contains(':'))
            && proto.is_none()
        {
            return Err(invalid(
                "a list or range of ports needs `proto: tcp` or `proto: udp`.",
            ));
        }
        if self.rule == Action::Limit && self.direction == Direction::Out {
            return Err(invalid("`limit` only applies to incoming connections."));
        }
        Ok(Rule {
            action: match self.rule {
                Action::Allow => "allow",
                Action::Deny => "deny",
                Action::Reject => "reject",
                Action::Limit => "limit",
            }
            .into(),
            direction: match self.direction {
                Direction::In => "in",
                Direction::Out => "out",
            }
            .into(),
            to_ip: self.to_ip.clone().filter(|ip| ip != "any"),
            port,
            proto,
            from_ip: self.from_ip.clone().filter(|ip| ip != "any"),
            interface: self.interface.clone(),
            comment: self.comment.clone(),
            ..Default::default()
        })
    }
}

#[async_trait]
impl Task for UfwRule {
    type Spec = Spec;
    const KIND: &'static str = "ufw.rule";
    const LOCKS: &'static [&'static str] = &["ufw"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str = "Add a ufw firewall rule (allow, deny, reject or limit).";

    fn key(spec: &Spec) -> String {
        spec.rule()
            .map(|r| r.key())
            .unwrap_or_else(|_| format!("{:?} rule", spec.rule).to_lowercase())
    }

    fn describe(spec: &Spec) -> String {
        spec.rule()
            .map(|r| r.args().join(" "))
            .unwrap_or_else(|_| Self::key(spec))
    }

    #[tracing::instrument(name = "task.ufw.rule.plan", skip_all)]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        let want = spec.rule()?;
        let summary = format!("ufw {}", want.args().join(" "));
        if !installed(cx).await? {
            return Ok(Planned::change(Diff::Create {
                summary: format!("{summary} (ufw is not installed yet)"),
                preview: None,
            }));
        }
        let status = read_status(cx, true).await?;
        let matching = matching_rules(&status.rules, &want);
        if matching.iter().any(|r| r.action == want.action) {
            return Ok(Planned::NoOp);
        }
        if let Some(other) = matching.first() {
            return Ok(Planned::change(Diff::Opaque {
                summary: format!("replace the {} rule with {summary}", other.action),
            }));
        }
        Ok(Planned::change(Diff::Create {
            summary,
            preview: None,
        }))
    }

    #[tracing::instrument(name = "task.ufw.rule.apply", skip_all)]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        let want = spec.rule()?;
        let status = read_status(cx, true).await?;
        for conflicting in matching_rules(&status.rules, &want)
            .into_iter()
            .filter(|r| r.action != want.action)
        {
            let mut cmd = ufw(&["--force", "delete"]);
            cmd = cmd.args(conflicting.args());
            run_checked(cx, cmd).await?;
        }
        let mut cmd = ufw(&[]).args(want.args());
        if let Some(c) = &want.comment {
            cmd = cmd.arg("comment").arg(c.clone());
        }
        run_checked(cx, cmd).await?;
        let id = want.args().join(" ");
        Ok(
            Applied::changed(format!("added ufw rule: {id}")).with_artefact(Artefact::Rule {
                subsystem: "ufw".into(),
                id,
            }),
        )
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        match rule_id(record) {
            Some(id) => format!("ufw delete {id}"),
            None => format!(
                "nothing to delete for {} (Buckle did not add it)",
                record.key.key
            ),
        }
    }

    #[tracing::instrument(name = "task.ufw.rule.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let Some(id) = rule_id(record) else {
            return Ok(Applied::skipped(
                "Buckle did not add this rule; left in place",
            ));
        };
        let cmd = ufw(&["--force", "delete"]).args(id.split_whitespace());
        run_checked(cx, cmd).await?;
        Ok(Applied::changed(format!("deleted ufw rule: {id}")))
    }
}

/// Existing rules with the same selectors (v4 entries, or v6 ones for v6 addresses).
fn matching_rules<'a>(rules: &'a [Rule], want: &Rule) -> Vec<&'a Rule> {
    rules
        .iter()
        .filter(|r| !r.v6 || want.mentions_ipv6())
        .filter(|r| r.same_selector(want))
        .collect()
}

fn rule_id(record: &ResourceRecord) -> Option<String> {
    record.artefacts.iter().find_map(|a| match a {
        Artefact::Rule { subsystem, id } if subsystem == "ufw" => Some(id.clone()),
        _ => None,
    })
}

buckle_core::register_task!(UfwRule);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, record_of, spec};
    use crate::tasks::ufw_common::testkit::Ufw;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn host(ufw: &Ufw) -> (FakeSystem, Cx) {
        let sys = ufw.attach(FakeSystem::new());
        let cx = cx(&sys);
        (sys, cx)
    }

    #[rstest]
    #[case::port_with_v6_twin(json!({"rule": "allow", "to_port": 22, "proto": "tcp"}))]
    #[case::source_restricted(json!({"rule": "allow", "to_port": 5432, "proto": "tcp", "from_ip": "10.1.0.0/16", "to_ip": "192.168.1.5"}))]
    #[case::interface_only(json!({"rule": "allow", "interface": "tailscale0"}))]
    #[case::port_list(json!({"rule": "allow", "to_port": "80,443", "proto": "tcp", "from_ip": "10.0.0.0/8"}))]
    #[case::outbound(json!({"rule": "allow", "direction": "out", "to_port": 53, "proto": "udp"}))]
    #[case::comment_differs_is_not_a_change(json!({"rule": "allow", "to_port": 22, "proto": "tcp", "comment": "ssh"}))]
    #[tokio::test]
    async fn plan_is_a_noop_when_the_rule_exists(#[case] spec_json: serde_json::Value) {
        let ufw = Ufw::new()
            .active()
            .rule("allow in from any to any port 22 proto tcp")
            .rule("allow in from 10.1.0.0/16 to 192.168.1.5 port 5432 proto tcp")
            .rule("allow in on tailscale0 from any to any")
            .rule("allow in from 10.0.0.0/8 to any port 80,443 proto tcp")
            .rule("allow out from any to any port 53 proto udp");
        let (_sys, cx) = host(&ufw);
        assert_eq!(
            UfwRule.plan(&cx, &spec(spec_json)).await.unwrap(),
            Planned::NoOp
        );
    }

    #[rstest]
    #[case::absent(json!({"rule": "allow", "to_port": 8080, "proto": "tcp"}), "ufw allow in from any to any port 8080 proto tcp")]
    #[case::other_source(json!({"rule": "allow", "to_port": 22, "proto": "tcp", "from_ip": "10.0.0.0/8"}), "ufw allow in from 10.0.0.0/8 to any port 22 proto tcp")]
    #[case::other_proto(json!({"rule": "allow", "to_port": 22, "proto": "udp"}), "ufw allow in from any to any port 22 proto udp")]
    #[case::other_direction(json!({"rule": "allow", "direction": "out", "to_port": 22, "proto": "tcp"}), "ufw allow out from any to any port 22 proto tcp")]
    #[case::action_differs(json!({"rule": "deny", "to_port": 22, "proto": "tcp"}), "replace the allow rule with ufw deny in from any to any port 22 proto tcp")]
    #[tokio::test]
    async fn plan_is_a_change_when_it_does_not(
        #[case] spec_json: serde_json::Value,
        #[case] summary: &str,
    ) {
        let ufw = Ufw::new()
            .active()
            .rule("allow in from any to any port 22 proto tcp");
        let (_sys, cx) = host(&ufw);
        match UfwRule.plan(&cx, &spec(spec_json)).await.unwrap() {
            Planned::Change { diff } => assert_eq!(diff.summary(), summary),
            other => panic!("expected change, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rules_are_found_when_ufw_is_inactive() {
        let ufw = Ufw::new().rule("allow in from any to any port 22 proto tcp");
        let (_sys, cx) = host(&ufw);
        let s: Spec = spec(json!({"rule": "allow", "to_port": 22, "proto": "tcp"}));
        assert_eq!(UfwRule.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn a_missing_ufw_binary_plans_the_rule_as_a_pending_change() {
        let sys = FakeSystem::new();
        let cx = cx(&sys);
        let planned = UfwRule
            .plan(&cx, &spec(json!({"rule": "allow", "to_port": 22})))
            .await
            .unwrap();
        match planned {
            Planned::Change { diff } => assert!(diff.summary().contains("not installed yet")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[rstest]
    #[case(json!({"rule": "allow"}))]
    #[case(json!({"rule": "allow", "to_port": "80,443"}))]
    #[case(json!({"rule": "limit", "direction": "out", "to_port": 22, "proto": "tcp"}))]
    #[tokio::test]
    async fn invalid_specs_are_user_errors(#[case] j: serde_json::Value) {
        let (_sys, cx) = host(&Ufw::new().active());
        let err = UfwRule.plan(&cx, &spec(j)).await.unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User), "{err:?}");
    }

    #[tokio::test]
    async fn apply_adds_the_rule_with_its_comment_and_converges() {
        let ufw = Ufw::new().active();
        let (sys, cx) = host(&ufw);
        let s: Spec = spec(
            json!({"rule": "limit", "to_port": 22, "proto": "tcp", "interface": "eth0", "comment": "ssh brute-force guard"}),
        );
        let applied = UfwRule.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let add = sys
            .commands_run()
            .into_iter()
            .find(|c| c.args.first().map(String::as_str) == Some("limit"))
            .unwrap();
        assert_eq!(
            add.args,
            vec![
                "limit",
                "in",
                "on",
                "eth0",
                "from",
                "any",
                "to",
                "any",
                "port",
                "22",
                "proto",
                "tcp",
                "comment",
                "ssh brute-force guard"
            ]
        );
        assert_eq!(
            applied.artefacts,
            vec![Artefact::Rule {
                subsystem: "ufw".into(),
                id: "limit in on eth0 from any to any port 22 proto tcp".into()
            }]
        );
        assert_eq!(UfwRule.plan(&cx, &s).await.unwrap(), Planned::NoOp);
        assert_eq!(ufw.snapshot().rules.len(), 1);
    }

    #[tokio::test]
    async fn changing_the_action_replaces_the_old_rule() {
        let ufw = Ufw::new()
            .active()
            .rule("allow in from any to any port 22 proto tcp")
            .rule("allow in from any to any port 80 proto tcp");
        let (sys, cx) = host(&ufw);
        let s: Spec = spec(json!({"rule": "deny", "to_port": 22, "proto": "tcp"}));
        UfwRule.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let lines = sys.command_lines();
        let delete = lines
            .iter()
            .position(|l| l.starts_with("ufw --force delete allow in"))
            .unwrap();
        let add = lines
            .iter()
            .position(|l| l.starts_with("ufw deny in"))
            .unwrap();
        assert!(delete < add);
        let rules = ufw.snapshot().rules;
        assert_eq!(rules.len(), 2);
        assert!(
            rules
                .iter()
                .any(|r| r.action == "deny" && r.port.as_deref() == Some("22"))
        );
        assert!(
            rules
                .iter()
                .any(|r| r.action == "allow" && r.port.as_deref() == Some("80"))
        );
        assert_eq!(UfwRule.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn removal_deletes_only_the_rule_buckle_added() {
        let ufw = Ufw::new()
            .active()
            .rule("allow in from any to any port 22 proto tcp");
        let (_sys, cx) = host(&ufw);
        let s: Spec = spec(json!({"rule": "allow", "to_port": 8080, "proto": "tcp"}));
        let applied = UfwRule.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert_eq!(ufw.snapshot().rules.len(), 2);
        let record = record_of("ufw.rule", "x", &applied);
        assert_eq!(
            UfwRule.describe_removal(&record),
            "ufw delete allow in from any to any port 8080 proto tcp"
        );
        UfwRule.remove(&cx, &record).await.unwrap();
        let rules = ufw.snapshot().rules;
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].port.as_deref(), Some("22"));
    }

    #[tokio::test]
    async fn removal_of_an_unrecorded_rule_does_nothing() {
        let ufw = Ufw::new().active();
        let (sys, cx) = host(&ufw);
        let record = record_of("ufw.rule", "x", &Applied::default());
        let removed = UfwRule.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert!(sys.command_lines().is_empty());
    }
}
