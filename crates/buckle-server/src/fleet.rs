//! Evaluating the fleet configuration against targets: labels, groups,
//! phases. A host's context reads as `host.*`, a cluster target's as
//! `cluster.*`; the same groups and phase selectors are evaluated against
//! both, each matching only the kind it is written for.

use crate::store::HostRecord;
use buckle_api::TargetKind;
use buckle_core::model::{FleetConfig, PhasePolicy, RolloutPolicy};
use buckle_core::selectors::{HostContext, HostIdentity, Labels};
use std::collections::BTreeMap;

/// Converts string labels into selector values: `true`/`false` become
/// booleans and canonical numbers become numbers, as they would in YAML, so
/// `host.labels.canary == true` works for a label set to `"true"`. The same
/// helper types the agent's labels, so both sides select alike.
pub fn labels_to_core(labels: &BTreeMap<String, String>) -> Labels {
    buckle_core::selectors::typed_labels(labels)
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
    let ctx = HostContext::new(HostIdentity {
        id: id.to_owned(),
        labels: labels_to_core(labels),
    })
    .with_facts(facts.clone());
    with_groups(ctx, fleet)
}

/// The selector context of a cluster target: `cluster.id`, `cluster.kind`,
/// `cluster.labels.*` and the groups written over `cluster.*`.
pub fn cluster_context(
    id: &str,
    kind: buckle_api::ClusterKind,
    labels: &BTreeMap<String, String>,
    fleet: &FleetConfig,
) -> HostContext {
    let ctx = HostContext::for_cluster(
        HostIdentity {
            id: id.to_owned(),
            labels: labels_to_core(labels),
        },
        crate::targets::core_kind(kind),
    );
    with_groups(ctx, fleet)
}

/// `ctx` with the fleet's groups evaluated in declaration order, so a group
/// may refer to earlier ones.
fn with_groups(mut ctx: HostContext, fleet: &FleetConfig) -> HostContext {
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

/// The selector context of a stored target: [`host_context`] for a host,
/// [`cluster_context`] for a cluster, so groups and `phase_for` work for
/// either.
pub fn record_context(
    host: &HostRecord,
    facts: &serde_json::Value,
    fleet: &FleetConfig,
) -> HostContext {
    match (&host.kind, &host.cluster) {
        (TargetKind::Cluster, Some(c)) => {
            cluster_context(&host.id, c.spec.kind, &host.effective_labels(), fleet)
        }
        _ => host_context(&host.id, &host.effective_labels(), facts, fleet),
    }
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

/// How the cluster target `name` grants identities: its `fleet.clusters`
/// declaration in `fleet`, else the stored cluster record (one created
/// through the API, or no longer declared). `None` when neither defines it.
pub fn cluster_trust(
    fleet: &FleetConfig,
    stored: Option<&HostRecord>,
    name: &str,
) -> Option<crate::identity::ClusterTrust> {
    use buckle_core::model::ClusterIdentity;
    if let Some(decl) = fleet.fleet.clusters.get(name) {
        return Some(crate::identity::ClusterTrust {
            target: name.to_owned(),
            cluster: decl.cluster.clone().unwrap_or_else(|| name.to_owned()),
            nodes: decl.nodes.clone(),
            executors: decl.executors().cloned(),
            buckle_jwt: decl.identity == ClusterIdentity::BuckleJwt,
        });
    }
    let record = stored.filter(|r| r.id == name && r.kind == TargetKind::Cluster)?;
    let info = record.cluster.as_ref()?;
    // An unparseable selector matches no one.
    let filter = |s: Option<&str>| s.and_then(|s| buckle_core::Filter::new(s).ok());
    Some(crate::identity::ClusterTrust {
        target: name.to_owned(),
        cluster: info.spec.cluster.clone().unwrap_or_else(|| name.to_owned()),
        nodes: filter(info.spec.nodes.as_deref()),
        executors: filter(info.spec.executors()),
        buckle_jwt: ClusterIdentity::parse(&info.spec.identity) == Ok(ClusterIdentity::BuckleJwt),
    })
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
    #[case("3", serde_json::json!(3))]
    #[case("007", serde_json::json!("007"))]
    #[case("home", serde_json::json!("home"))]
    fn labels_coerce_like_yaml(#[case] raw: &str, #[case] expected: serde_json::Value) {
        assert_eq!(buckle_core::selectors::label_value(raw), expected);
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
