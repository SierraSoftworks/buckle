//! The fleet's workloads, gathered from every cluster that runs them: one
//! entry per workload with one placement per cluster, read with the same
//! row logic the cluster page's Workloads card uses.

use buckle_api::{ActionInfo, HostDetail, RunReport};
use chrono::{DateTime, Utc};

use super::clusters::{WorkloadRow, WorkloadState, workload_rows};

/// One workload on one cluster.
#[derive(Debug, Clone, PartialEq)]
pub struct Placement {
    pub cluster: String,
    pub row: WorkloadRow,
    /// Its resources differ from what was applied.
    pub drifted: bool,
    /// When its checks were last read.
    pub read_at: Option<DateTime<Utc>>,
}

/// A workload across the fleet.
#[derive(Debug, Clone, PartialEq)]
pub struct FleetWorkload {
    pub name: String,
    pub description: String,
    pub needs: Vec<String>,
    pub placements: Vec<Placement>,
    /// The actions any of its placements offers, once each by name, in the
    /// order its clusters declare them: what a workload-wide action rollout
    /// can run.
    pub actions: Vec<ActionInfo>,
}

impl FleetWorkload {
    /// The worst state of any placement.
    pub fn state(&self) -> WorkloadState {
        self.placements
            .iter()
            .map(|p| p.row.state)
            .min()
            .unwrap_or(WorkloadState::NotApplied)
    }

    pub fn drifted(&self) -> bool {
        self.placements.iter().any(|p| p.drifted)
    }

    /// The newest revision applied on any cluster.
    pub fn applied(&self) -> Option<&(DateTime<Utc>, String)> {
        self.placements
            .iter()
            .filter_map(|p| p.row.applied.as_ref())
            .max_by_key(|(at, _)| *at)
    }

    /// The newest health reading on any cluster.
    pub fn read_at(&self) -> Option<DateTime<Utc>> {
        self.placements.iter().filter_map(|p| p.read_at).max()
    }

    /// The workloads that list this one in `needs`.
    pub fn needed_by<'a>(&self, all: &'a [FleetWorkload]) -> Vec<&'a str> {
        all.iter()
            .filter(|w| w.needs.contains(&self.name))
            .map(|w| w.name.as_str())
            .collect()
    }
}

/// Every workload the clusters run, worst first then by name; each
/// placement in cluster order. `runs` is each cluster's run in progress.
pub fn gather(clusters: &[(HostDetail, Option<RunReport>)]) -> Vec<FleetWorkload> {
    let mut out: Vec<FleetWorkload> = Vec::new();
    for (d, run) in clusters {
        let views = &d.workloads;
        for row in workload_rows(d, run.as_ref().or(d.latest_run.as_ref())) {
            let placement = Placement {
                cluster: d.summary.id.clone(),
                drifted: d.drift.iter().any(|x| x.module == row.name),
                read_at: d
                    .health_checks
                    .iter()
                    .filter(|c| c.module == row.name)
                    .map(|c| c.at)
                    .max(),
                row,
            };
            let needs = views
                .iter()
                .find(|v| v.name == placement.row.name)
                .map(|v| v.needs.clone())
                .unwrap_or_default();
            let actions: Vec<ActionInfo> = d
                .actions
                .iter()
                .filter(|a| a.module == placement.row.name)
                .cloned()
                .collect();
            match out.iter_mut().find(|w| w.name == placement.row.name) {
                Some(w) => {
                    for a in actions {
                        if !w.actions.iter().any(|b| b.name == a.name) {
                            w.actions.push(a);
                        }
                    }
                    if w.description.is_empty() {
                        w.description = placement.row.description.clone();
                    }
                    for n in needs {
                        if !w.needs.contains(&n) {
                            w.needs.push(n);
                        }
                    }
                    w.placements.push(placement);
                }
                None => out.push(FleetWorkload {
                    name: placement.row.name.clone(),
                    description: placement.row.description.clone(),
                    needs,
                    placements: vec![placement],
                    actions,
                }),
            }
        }
    }
    for w in &mut out {
        w.placements.sort_by(|a, b| a.cluster.cmp(&b.cluster));
    }
    out.sort_by(|a, b| (a.state(), &a.name).cmp(&(b.state(), &b.name)));
    out
}

/// Whether a workload matches a search: its name, description or a cluster.
pub fn matches(w: &FleetWorkload, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty()
        || w.name.to_lowercase().contains(&q)
        || w.description.to_lowercase().contains(&q)
        || w.placements
            .iter()
            .any(|p| p.cluster.to_lowercase().contains(&q))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{ActionInfo, DriftItem, HostCheck, HostSummary, WorkloadView};

    fn cluster(id: &str, workloads: &[(&str, &[&str])], failing: &[&str]) -> HostDetail {
        let summary: HostSummary = serde_json::from_value(serde_json::json!({
            "id": id, "kind": "cluster", "status": "active", "connection": "offline",
            "agent_labels": {}, "labels": {}, "effective_labels": {}, "groups": [],
            "platform": null, "arch": null, "agent_version": null, "applied_revision": null,
            "health": "unknown", "health_at": null, "drifted": false,
            "first_seen": "2026-11-01T00:00:00Z", "last_seen": null,
        }))
        .unwrap();
        HostDetail {
            summary,
            facts: serde_json::Value::Null,
            facts_at: None,
            resources: vec![],
            recent_runs: vec![],
            health_results: vec![],
            health_checks: failing
                .iter()
                .map(|w| HostCheck {
                    module: w.to_string(),
                    check: "job".into(),
                    kind: "nomad.job".into(),
                    required: true,
                    status: "fail".into(),
                    reason: None,
                    scope: "drift".into(),
                    at: "2026-11-05T00:00:00Z".parse().unwrap(),
                    history: vec![],
                })
                .collect(),
            drift: vec![DriftItem {
                module: "paste".into(),
                kind: "nomad.job".into(),
                key: "default/paste".into(),
                summary: "count 1 → 2".into(),
                unified: None,
                detected_at: None,
            }]
            .into_iter()
            .filter(|_| id == "lab")
            .collect(),
            latest_run: None,
            workloads: workloads
                .iter()
                .map(|(n, needs)| WorkloadView {
                    name: n.to_string(),
                    needs: needs.iter().map(|s| s.to_string()).collect(),
                    ..Default::default()
                })
                .collect(),
            executors: vec![],
            actions: vec![],
        }
    }

    fn action(module: &str, name: &str) -> ActionInfo {
        ActionInfo {
            module: module.into(),
            name: name.into(),
            label: name.into(),
            kind: name.into(),
            description: None,
            confirm: true,
            implicit: true,
            instances: None,
        }
    }

    #[test]
    fn a_workload_on_several_clusters_is_one_entry_worst_first() {
        let all = gather(&[
            (
                cluster(
                    "home",
                    &[("postgresql", &[]), ("paste", &["postgresql"])],
                    &[],
                ),
                None,
            ),
            (
                cluster("lab", &[("paste", &[]), ("grafana", &[])], &["grafana"]),
                None,
            ),
        ]);
        let names: Vec<&str> = all.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names[0], "grafana", "the failing workload leads");
        let paste = all.iter().find(|w| w.name == "paste").unwrap();
        let on: Vec<&str> = paste
            .placements
            .iter()
            .map(|p| p.cluster.as_str())
            .collect();
        assert_eq!(on, vec!["home", "lab"]);
        assert!(paste.drifted());
        let pg = all.iter().find(|w| w.name == "postgresql").unwrap();
        assert_eq!(pg.needed_by(&all), vec!["paste"]);
    }

    #[test]
    fn a_workload_offers_its_placements_actions_once_each() {
        let mut home = cluster("home", &[("paste", &[]), ("grafana", &[])], &[]);
        home.actions = vec![action("paste", "restart"), action("grafana", "restart")];
        let mut lab = cluster("lab", &[("paste", &[])], &[]);
        lab.actions = vec![action("paste", "restart"), action("paste", "dispatch")];
        let all = gather(&[(home, None), (lab, None)]);
        let paste = all.iter().find(|w| w.name == "paste").unwrap();
        let names: Vec<&str> = paste.actions.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["restart", "dispatch"]);
        let grafana = all.iter().find(|w| w.name == "grafana").unwrap();
        assert!(grafana.actions.iter().all(|a| a.module == "grafana"));
    }

    #[test]
    fn search_covers_names_and_clusters() {
        let all = gather(&[(cluster("lab", &[("grafana", &[])], &[]), None)]);
        assert!(matches(&all[0], "LAB"));
        assert!(matches(&all[0], "graf"));
        assert!(!matches(&all[0], "home"));
    }
}
