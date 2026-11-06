//! A rollout's units: a host, an old-style cluster row, or a placement (one
//! workload on one cluster, `host` naming the cluster). Rows are keyed,
//! named and judged unchanged here so every view agrees.

use buckle_api::{HostRolloutState, PlanGroup, RolloutHost, TargetKind};

/// What identifies a unit within a rollout: the host, or `cluster/workload`.
pub fn key(h: &RolloutHost) -> String {
    buckle_api::placement_key(&h.host, h.workload.as_deref())
}

/// Whether two entries are the same unit.
pub fn same(a: &RolloutHost, b: &RolloutHost) -> bool {
    a.host == b.host && a.workload == b.workload
}

/// What a unit is called: the workload, else the target.
pub fn name(h: &RolloutHost) -> &str {
    h.workload.as_deref().unwrap_or(&h.host)
}

/// The cluster a placement runs on, `None` for a host or cluster row.
pub fn cluster(h: &RolloutHost) -> Option<&str> {
    h.workload.as_ref().map(|_| h.host.as_str())
}

/// Whether the Targets card's Changed view lists the unit: anything not
/// known to be a no-op, and a no-op that still went wrong.
pub fn changed_view(h: &RolloutHost) -> bool {
    !unchanged(h)
        || h.needs_attention
        || matches!(
            h.state,
            HostRolloutState::Failed | HostRolloutState::Held | HostRolloutState::RolledBack
        )
}

/// Whether distinct plan `g` is the unit's: its digest, or the group lists
/// the unit by key.
pub fn in_group(g: &PlanGroup, h: &RolloutHost) -> bool {
    h.plan_digest.as_deref() == Some(g.digest.as_str()) || g.hosts.iter().any(|n| *n == key(h))
}

/// Whether the entry is one workload on one cluster.
pub fn is_placement(h: &RolloutHost) -> bool {
    h.workload.is_some() || h.kind == TargetKind::Workload
}

/// A unit whose plan changes and removes nothing: it rolls without a
/// gate, bake or slot, and the Targets card hides it until asked.
pub fn unchanged(h: &RolloutHost) -> bool {
    h.plan_changes == Some(0) && h.plan_removals.unwrap_or(0) == 0
}

/// Units counted by kind, hosts first: `2 failed hosts (pi-1, pi-2) and
/// 1 failed workload (grafana on nomad)`. `adjective` qualifies each noun
/// ("failed"); `names` lists who is counted.
pub fn tally(units: &[&RolloutHost], adjective: &str, names: bool) -> String {
    let mut kinds: [(&str, &str, Vec<String>); 3] = [
        ("host", "hosts", vec![]),
        ("cluster", "clusters", vec![]),
        ("workload", "workloads", vec![]),
    ];
    for h in units {
        let (slot, label) = match cluster(h) {
            Some(c) => (2, format!("{} on {c}", name(h))),
            None if is_placement(h) => (2, h.host.clone()),
            None if h.kind == TargetKind::Cluster => (1, h.host.clone()),
            None => (0, h.host.clone()),
        };
        kinds[slot].2.push(label);
    }
    let adj = |n: &str| match adjective {
        "" => n.to_string(),
        a => format!("{a} {n}"),
    };
    let parts: Vec<String> = kinds
        .iter()
        .filter(|(_, _, of)| !of.is_empty())
        .map(|(one, many, of)| {
            let n = crate::logic::format::count(of.len(), &adj(one), &adj(many));
            if names {
                format!("{n} ({})", of.join(", "))
            } else {
                n
            }
        })
        .collect();
    match parts.as_slice() {
        [] => format!("no {}", adj("hosts")),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(host: &str, workload: Option<&str>, changes: Option<u32>) -> RolloutHost {
        let mut h: RolloutHost = serde_json::from_value(serde_json::json!({
            "host": host, "phase": "p", "state": "planned", "updated_at": "2026-11-05T00:00:00Z",
        }))
        .unwrap();
        h.workload = workload.map(Into::into);
        h.plan_changes = changes;
        h
    }

    #[test]
    fn placements_are_keyed_and_named_by_their_workload() {
        let p = unit("nomad", Some("grafana"), None);
        assert_eq!(key(&p), "nomad/grafana");
        assert_eq!(name(&p), "grafana");
        assert_eq!(cluster(&p), Some("nomad"));
        let h = unit("pi-1", None, None);
        assert_eq!(
            (key(&h).as_str(), name(&h), cluster(&h)),
            ("pi-1", "pi-1", None)
        );
        assert!(!same(&p, &unit("nomad", Some("loki"), None)));
    }

    #[test]
    fn tallies_count_hosts_and_workloads_apart() {
        let (a, b, g) = (
            unit("pi-1", None, None),
            unit("pi-2", None, None),
            unit("nomad", Some("grafana"), None),
        );
        let cases: [(&[&RolloutHost], &str); 4] = [
            (
                &[&a, &g, &b],
                "2 failed hosts (pi-1, pi-2) and 1 failed workload (grafana on nomad)",
            ),
            (&[&g], "1 failed workload (grafana on nomad)"),
            (&[&a], "1 failed host (pi-1)"),
            (&[], "no failed hosts"),
        ];
        for (units, want) in cases {
            assert_eq!(tally(units, "failed", true), want);
        }
        assert_eq!(tally(&[&a, &g], "", false), "1 host and 1 workload");
    }

    #[test]
    fn only_a_planned_no_op_is_unchanged() {
        assert!(unchanged(&unit("a", None, Some(0))));
        assert!(!unchanged(&unit("a", None, Some(2))));
        assert!(!unchanged(&unit("a", None, None)), "not planned yet");
    }

    #[test]
    fn the_changed_view_keeps_no_ops_that_went_wrong() {
        let mut quiet = unit("a", None, Some(0));
        assert!(!changed_view(&quiet));
        quiet.state = HostRolloutState::Failed;
        assert!(changed_view(&quiet));
        assert!(changed_view(&unit("b", None, Some(1))));
        assert!(
            changed_view(&unit("c", None, None)),
            "unknown until planned"
        );
    }
}
