//! Evaluating the fleet configuration against hosts: labels, groups, phases.

use crate::store::HostRecord;
use buckle_core::model::{FleetConfig, PhasePolicy, RolloutPolicy};
use buckle_core::selectors::{HostContext, HostIdentity, Labels};
use std::collections::BTreeMap;

/// Converts string labels into selector values: `true`/`false` become
/// booleans and canonical numbers become numbers, as they would in YAML, so
/// `host.labels.canary == true` works for a label set to `"true"`.
pub fn labels_to_core(labels: &BTreeMap<String, String>) -> Labels {
    labels
        .iter()
        .map(|(k, v)| (k.clone(), label_value(v)))
        .collect()
}

fn label_value(v: &str) -> serde_json::Value {
    match v {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        _ => match v.parse::<f64>() {
            Ok(n) if n.is_finite() && format_number(n) == v => serde_json::Number::from_f64(n)
                .map(serde_json::Value::Number)
                .unwrap_or_else(|| serde_json::Value::String(v.to_owned())),
            _ => serde_json::Value::String(v.to_owned()),
        },
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Builds the selector context for a host from its effective labels and
/// latest facts, with groups derived from the fleet config. Groups are
/// evaluated in declaration order, so a group may refer to earlier ones.
pub fn host_context(
    id: &str,
    labels: &BTreeMap<String, String>,
    facts: &serde_json::Value,
    fleet: &FleetConfig,
) -> HostContext {
    let mut ctx = HostContext::new(HostIdentity {
        id: id.to_owned(),
        labels: labels_to_core(labels),
    })
    .with_facts(facts.clone());
    let mut groups = Vec::new();
    for (name, filter) in &fleet.fleet.groups {
        if ctx.matches(filter) {
            groups.push(name.clone());
            ctx.groups = groups.clone();
        }
    }
    ctx.groups = groups;
    ctx
}

/// [`host_context`] for a stored host.
pub fn record_context(
    host: &HostRecord,
    facts: &serde_json::Value,
    fleet: &FleetConfig,
) -> HostContext {
    host_context(&host.id, &host.effective_labels(), facts, fleet)
}

/// The phases a rollout runs: the configured ones, or a single implicit
/// `fleet` phase selecting every host when none are configured.
pub fn effective_phases(policy: &RolloutPolicy) -> Vec<PhasePolicy> {
    if !policy.phases.is_empty() {
        return policy.phases.clone();
    }
    match buckle_core::Filter::new("true") {
        Ok(select) => vec![PhasePolicy {
            name: "fleet".into(),
            select,
            parallel: buckle_core::model::Parallelism::Count(1),
            approval: Default::default(),
            bake: None,
            max_failures: buckle_core::model::Parallelism::Count(0),
        }],
        Err(_) => Vec::new(),
    }
}

/// The index of the first phase whose selector matches the host.
pub fn phase_for(ctx: &HostContext, phases: &[PhasePolicy]) -> Option<usize> {
    phases.iter().position(|p| ctx.matches(&p.select))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const FLEET: &str = r#"
version: 2
fleet:
  groups:
    canary: 'host.labels.canary == true'
    home: 'host.labels.site == "home"'
    home_canary: 'host.groups contains "home" && host.groups contains "canary"'
rollout:
  phases:
    - { name: canary, select: 'host.groups contains "canary"' }
    - { name: home, select: 'host.groups contains "home"' }
"#;

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[rstest]
    #[case(&[("canary", "true"), ("site", "home")], &["canary", "home", "home_canary"], Some(0))]
    #[case(&[("site", "home")], &["home"], Some(1))]
    #[case(&[("site", "cloud")], &[], None)]
    fn groups_and_first_matching_phase(
        #[case] l: &[(&str, &str)],
        #[case] groups: &[&str],
        #[case] phase: Option<usize>,
    ) {
        let fleet = FleetConfig::parse(FLEET).unwrap();
        let ctx = host_context("h", &labels(l), &serde_json::Value::Null, &fleet);
        assert_eq!(ctx.groups, groups);
        assert_eq!(phase_for(&ctx, &effective_phases(&fleet.rollout)), phase);
    }

    #[rstest]
    #[case("true", serde_json::json!(true))]
    #[case("3", serde_json::json!(3.0))]
    #[case("007", serde_json::json!("007"))]
    #[case("home", serde_json::json!("home"))]
    fn labels_coerce_like_yaml(#[case] raw: &str, #[case] expected: serde_json::Value) {
        assert_eq!(label_value(raw), expected);
    }

    #[test]
    fn no_phases_means_one_phase_for_everyone() {
        let fleet = FleetConfig::parse("version: 2\n").unwrap();
        let phases = effective_phases(&fleet.rollout);
        assert_eq!(phases.len(), 1);
        let ctx = host_context("h", &BTreeMap::new(), &serde_json::Value::Null, &fleet);
        assert_eq!(phase_for(&ctx, &phases), Some(0));
    }
}
