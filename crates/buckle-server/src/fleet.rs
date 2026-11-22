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

/// The labels selectors read for a host on `fleet`: what its agent reports
/// without the keys the fleet reserves, with the labels set on the server
/// overlaid. A reserved label so reads the server's value or null, never
/// the agent's.
pub fn effective_labels(host: &HostRecord, fleet: &FleetConfig) -> BTreeMap<String, String> {
    let reserved = fleet.reserved_labels();
    let mut out: BTreeMap<String, String> = host
        .agent_labels
        .iter()
        .filter(|(k, _)| {
            host.kind != TargetKind::Host || !reserved.iter().any(|r| r.eq_ignore_ascii_case(k))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    out.extend(host.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
    out
}

/// The reserved labels of `fleet` an agent reports with a value other than
/// the server's (which may be unset): key, then what the agent says.
pub fn label_conflicts(host: &HostRecord, fleet: &FleetConfig) -> BTreeMap<String, String> {
    if host.kind != TargetKind::Host {
        return BTreeMap::new();
    }
    let reserved = fleet.reserved_labels();
    host.agent_labels
        .iter()
        .filter(|(k, v)| {
            reserved.iter().any(|r| r.eq_ignore_ascii_case(k)) && host.labels.get(*k) != Some(*v)
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// The reserved labels of `fleet` the host has no server-set value for.
pub fn missing_reserved(labels: &BTreeMap<String, String>, fleet: &FleetConfig) -> Vec<String> {
    fleet
        .reserved_labels()
        .iter()
        .filter(|r| {
            !labels
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case(r) && !v.trim().is_empty())
        })
        .cloned()
        .collect()
}

/// The context `fleet`'s trust decisions are evaluated in for a host
/// (`executors`, an identity's `hosts`, a secret's `hosts`): its id, the
/// tags the server verified and its reserved labels. Nothing the agent
/// reports is in it.
pub fn trust_context(host: &HostRecord, fleet: &FleetConfig) -> HostContext {
    buckle_core::engine::trust::trust_context(
        fleet,
        &host.id,
        host.verified_tags.as_deref(),
        &labels_to_core(&host.labels),
    )
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

/// The selector context of a stored target: a host's from its effective
/// labels ([`effective_labels`]) and facts, with its verified tags in place
/// of the reported ones; a cluster's from [`cluster_context`]. Groups and
/// `phase_for` work for either.
pub fn record_context(
    host: &HostRecord,
    facts: &serde_json::Value,
    fleet: &FleetConfig,
) -> HostContext {
    match (&host.kind, &host.cluster) {
        (TargetKind::Cluster, Some(c)) => {
            cluster_context(&host.id, c.spec.kind, &host.effective_labels(), fleet)
        }
        _ => {
            let ctx = HostContext::new(HostIdentity {
                id: host.id.clone(),
                labels: labels_to_core(&effective_labels(host, fleet)),
            })
            .with_facts(facts.clone());
            // What the server verified stands in for what the agent says.
            let ctx = match &host.verified_tags {
                Some(tags) => ctx.with_verified_tags(tags),
                None => ctx,
            };
            with_groups(ctx, fleet)
        }
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
            scoped: decl.cluster.is_some(),
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
        scoped: info.spec.cluster.is_some(),
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

    const ZONED: &str = r#"
version: 2
fleet:
  labels:
    reserved: [zone]
  groups:
    home: 'host.labels.zone == "home"'
    edge: 'host.facts.tailscale.tags contains "tag:edge"'
    workers: 'host.labels.role == "worker"'
"#;

    /// What a host is selected on, against what it says about itself: a
    /// reserved label and its tags come from the server, everything else
    /// may be the agent's report.
    #[rstest]
    #[case::report_alone(&[("zone", "home"), ("role", "worker")], &[], None, &["edge", "workers"], &[])]
    #[case::server_set(&[("zone", "public")], &[("zone", "home")], None, &["home", "edge"], &["home"])]
    #[case::reported_tags_stand_without_whois(&[], &[], None, &["edge"], &[])]
    #[case::verified_tags_replace_the_report(&[], &[], Some(&["tag:core"][..]), &[], &[])]
    #[case::verified_tag(&[("role", "worker")], &[], Some(&["tag:edge"][..]), &["edge", "workers"], &["edge"])]
    #[tokio::test]
    async fn selection_reads_reports_and_trust_reads_only_the_server(
        #[case] reported: &[(&str, &str)],
        #[case] set: &[(&str, &str)],
        #[case] verified: Option<&[&str]>,
        #[case] groups: &[&str],
        #[case] trusted_groups: &[&str],
    ) {
        let fleet = FleetConfig::parse(ZONED).unwrap();
        let host = HostRecord::fixture("pi-1", reported, set, verified).await;
        // The agent always claims the edge tag.
        let facts = serde_json::json!({"tailscale": {"tags": ["tag:edge"]}});
        assert_eq!(record_context(&host, &facts, &fleet).groups, groups);
        assert_eq!(trust_context(&host, &fleet).groups, trusted_groups);
    }

    #[tokio::test]
    async fn a_conflicting_report_of_a_reserved_label_is_noticed() {
        let fleet = FleetConfig::parse(ZONED).unwrap();
        let agrees = HostRecord::fixture("a", &[("zone", "home")], &[("zone", "home")], None).await;
        assert!(label_conflicts(&agrees, &fleet).is_empty());
        let differs = HostRecord::fixture(
            "b",
            &[("zone", "home"), ("role", "x")],
            &[("zone", "public")],
            None,
        )
        .await;
        assert_eq!(
            label_conflicts(&differs, &fleet),
            labels(&[("zone", "home")])
        );
        assert_eq!(
            missing_reserved(&agrees.labels, &fleet),
            Vec::<String>::new()
        );
        assert_eq!(missing_reserved(&BTreeMap::new(), &fleet), vec!["zone"]);
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
