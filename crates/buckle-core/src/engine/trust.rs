//! Trust decisions: selectors that decide who may hold a credential or a
//! secret (`executors`, an identity's `hosts`, a secret's `hosts`).
//!
//! They are evaluated in a context holding only what the server itself
//! knows about a host: its id, its verified tags and its reserved labels
//! ([`trust_context`]). Anything an agent reports is absent and reads as
//! null, so a host cannot talk its way into one.

use super::validate::ValidationReport;
use crate::model::FleetConfig;
use crate::selectors::{HostContext, Labels, reads_groups, untrusted_references};

/// The groups a trust decision may read, in declaration order: those whose
/// selector reads only trusted keys and no other group. Any other group has
/// no members there.
pub fn trusted_groups(fleet: &FleetConfig) -> Vec<&str> {
    fleet
        .fleet
        .groups
        .iter()
        .filter(|(_, f)| {
            !reads_groups(f.raw())
                && untrusted_references(f.raw(), fleet.reserved_labels()).is_empty()
        })
        .map(|(name, _)| name.as_str())
        .collect()
}

/// The context `fleet`'s trust selectors are evaluated in for a host: `tags`
/// are its verified tags (`None` when the server did not authenticate it
/// over the tailnet) and `server_labels` the labels set on the server, of
/// which the reserved ones are kept.
pub fn trust_context(
    fleet: &FleetConfig,
    id: &str,
    tags: Option<&[String]>,
    server_labels: &Labels,
) -> HostContext {
    let reserved = fleet.reserved_labels();
    let labels = server_labels
        .iter()
        .filter(|(k, _)| reserved.iter().any(|r| r.eq_ignore_ascii_case(k)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut ctx = HostContext::trusted(id, tags, labels);
    let trusted = trusted_groups(fleet);
    ctx.groups = fleet
        .fleet
        .groups
        .iter()
        .filter(|(name, f)| trusted.contains(&name.as_str()) && ctx.matches(f))
        .map(|(name, _)| name.clone())
        .collect();
    ctx
}

/// What a trust selector reads that a trust decision does not know, as
/// warnings: keys an agent reports, and groups defined over them.
pub fn trust_problems(fleet: &FleetConfig, raw: &str) -> Vec<String> {
    let mut problems: Vec<String> = untrusted_references(raw, fleet.reserved_labels())
        .into_iter()
        .map(|key| {
            format!(
                "decides trust but reads `{key}`, which is null here: a trust decision knows only `host.id`, the verified `host.facts.tailscale.tags` and the labels in `fleet.labels.reserved`, so a comparison with it never matches and a negated one always does"
            )
        })
        .collect();
    if reads_groups(raw) {
        let trusted = trusted_groups(fleet);
        for name in fleet.fleet.groups.keys() {
            let named = raw.contains(&format!("\"{name}\"")) || raw.contains(&format!("'{name}'"));
            if named && !trusted.contains(&name.as_str()) {
                problems.push(format!(
                    "decides trust but reads the group '{name}', which is defined over keys a trust decision does not know (or over other groups): no host is in it here"
                ));
            }
        }
    }
    problems
}

/// Validates what `fleet` declares about trust: reserved labels,
/// `fleet.identities`, secrets' `hosts` and each cluster's executors.
pub(super) fn validate_trust(report: &mut ValidationReport, fleet: &FleetConfig) {
    let reserved = fleet.reserved_labels();
    for (i, key) in reserved.iter().enumerate() {
        let location = format!("buckle.yaml fleet.labels.reserved[{i}]");
        if key.trim().is_empty() || key.contains(['.', ' ']) {
            report.error(
                None,
                &location,
                format!("'{key}' is not a label key a selector can read; use letters, digits, '_' or '-'"),
            );
        } else if reserved[..i].iter().any(|k| k.eq_ignore_ascii_case(key)) {
            report.error(None, &location, format!("reserves '{key}' twice"));
        }
    }
    let selector = |report: &mut ValidationReport, location: String, raw: &str| {
        if crate::selectors::selector_namespaces(raw).cluster {
            report.error(
                None,
                &location,
                "selects on `cluster.*`, but it chooses hosts; select on `host.*` instead",
            );
        }
        for problem in trust_problems(fleet, raw) {
            report.warning(None, &location, problem);
        }
    };
    for (name, decl) in &fleet.fleet.identities {
        let location = format!("buckle.yaml fleet.identities.{name}");
        let audience = decl.audience(name);
        if audience.trim().is_empty() {
            report.error(None, &location, "has an empty `audience`");
        }
        if fleet.fleet.clusters.contains_key(audience) {
            report.error(
                None,
                &location,
                format!(
                    "uses the audience '{audience}', which is the cluster target of that name: a token for it could be presented to the cluster, so choose another audience"
                ),
            );
        }
        selector(report, format!("{location}.hosts"), decl.hosts.raw());
    }
    for (name, decl) in &fleet.secrets {
        if let Some(hosts) = &decl.hosts {
            selector(
                report,
                format!("buckle.yaml secrets.{name}.hosts"),
                hosts.raw(),
            );
        }
    }
    for (name, decl) in &fleet.fleet.clusters {
        // Executors default to `nodes`, which then decides trust too.
        let (key, filter) = match (&decl.executors, &decl.nodes) {
            (Some(f), _) => ("executors", f),
            (None, Some(f)) => ("nodes", f),
            (None, None) => continue,
        };
        for problem in trust_problems(fleet, filter.raw()) {
            let problem = if key == "nodes" {
                format!("{problem} (`nodes` chooses the executors too while `executors` is unset)")
            } else {
                problem
            };
            report.warning(
                None,
                format!("buckle.yaml fleet.clusters.{name}.{key}"),
                problem,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const FLEET: &str = r#"
version: 2
fleet:
  labels:
    reserved: [zone]
  groups:
    z2: 'host.facts.tailscale.tags contains "tag:z2"'
    home: 'host.labels.zone == "home"'
    workers: 'host.labels.role == "worker"'
    home_workers: 'host.groups contains "home" && host.groups contains "workers"'
"#;

    fn fleet(extra: &str) -> FleetConfig {
        FleetConfig::parse(&format!("{FLEET}{extra}")).unwrap()
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect()
    }

    #[test]
    fn only_groups_over_trusted_keys_exist_in_a_trust_decision() {
        let fleet = fleet("");
        assert_eq!(trusted_groups(&fleet), vec!["z2", "home"]);
        let ctx = trust_context(
            &fleet,
            "pi-1",
            Some(&["tag:z2".to_owned()]),
            &labels(&[("zone", "home"), ("role", "worker")]),
        );
        assert_eq!(ctx.groups, vec!["z2", "home"]);
        assert!(!ctx.host.labels.contains_key("role"));
    }

    #[rstest]
    #[case::token_host(None, &[("zone", "home")], &["home"])]
    #[case::untagged(Some(&[][..]), &[], &[])]
    #[case::other_zone(Some(&["tag:z0".to_owned()][..]), &[("zone", "public")], &[])]
    fn trust_groups_follow_tags_and_reserved_labels(
        #[case] tags: Option<&[String]>,
        #[case] server: &[(&str, &str)],
        #[case] groups: &[&str],
    ) {
        let ctx = trust_context(&fleet(""), "pi-1", tags, &labels(server));
        assert_eq!(ctx.groups, groups);
    }

    #[rstest]
    #[case::id(r#"host.id == "pi-1""#, 0)]
    #[case::trusted_group(r#"host.groups contains "z2""#, 0)]
    #[case::agent_label(r#"host.labels.role == "steward""#, 1)]
    #[case::untrusted_group(r#"host.groups contains "workers""#, 1)]
    #[case::group_of_groups(r#"host.groups contains 'home_workers'"#, 1)]
    fn trust_selectors_reading_agent_reports_are_flagged(#[case] raw: &str, #[case] count: usize) {
        assert_eq!(trust_problems(&fleet(""), raw).len(), count, "{raw}");
    }

    #[rstest]
    #[case::clean(
        "  clusters:\n    nomad: { kind: nomad, executors: 'host.groups contains \"z2\"' }\n  identities:\n    vault: { hosts: 'host.labels.zone == \"home\"' }\nsecrets:\n  k: { env: K, hosts: 'host.id == \"pi-1\"' }\n",
        0,
        0
    )]
    #[case::executors_over_an_agent_label(
        "  clusters:\n    nomad: { kind: nomad, executors: 'host.labels.role == \"steward\"' }\n",
        0,
        1
    )]
    #[case::nodes_standing_in_for_executors(
        "  clusters:\n    nomad: { kind: nomad, nodes: 'host.groups contains \"workers\"' }\n",
        0,
        1
    )]
    #[case::nodes_beside_executors(
        "  clusters:\n    nomad: { kind: nomad, nodes: 'host.groups contains \"workers\"', executors: 'host.id == \"pi-1\"' }\n",
        0,
        0
    )]
    #[case::audience_of_a_cluster(
        "  clusters:\n    nomad: { kind: nomad, executors: 'host.id == \"pi-1\"' }\n  identities:\n    vault: { audience: nomad, hosts: 'host.id == \"pi-1\"' }\n",
        1,
        0
    )]
    #[case::secret_capped_by_a_fact(
        "secrets:\n  k: { env: K, hosts: 'host.facts.os.family == \"debian\"' }\n",
        0,
        1
    )]
    fn trust_declarations_are_validated(
        #[case] extra: &str,
        #[case] errors: usize,
        #[case] warnings: usize,
    ) {
        let mut report = ValidationReport::default();
        validate_trust(&mut report, &fleet(extra));
        assert_eq!(report.errors.len(), errors, "{report}");
        assert_eq!(report.warnings.len(), warnings, "{report}");
    }

    #[rstest]
    #[case::two_sources("secrets:\n  k: { env: K, file: /x }\n")]
    #[case::no_source("secrets:\n  k: { hosts: 'host.id == \"a\"' }\n")]
    #[case::unknown_key("secrets:\n  k: { env: K, host: 'true' }\n")]
    fn a_secret_names_exactly_one_source(#[case] yaml: &str) {
        assert!(FleetConfig::parse(&format!("version: 2\n{yaml}")).is_err());
    }
}
