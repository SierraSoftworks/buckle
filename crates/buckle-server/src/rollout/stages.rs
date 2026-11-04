//! Dependency stages inside a phase.
//!
//! A phase's targets apply in waves, but `needs` may cross the layers: a
//! workload on a cluster may need a module on the cluster's nodes
//! (`module:nomad`), and a module on a host may need a workload on the
//! cluster the host is a node of (`workload:postgresql`). Those edges are
//! resolved here into **stages**: everything with no cross-layer dependency
//! first, then what depends on it, and so on. Inside a stage the phase's
//! `parallel` applies as before.
//!
//! A host whose modules sit on both sides of a cluster gets one **step** per
//! stage it appears in: the earlier steps name the modules that must go
//! first (a module-scoped plan, `module.name == …`), and the last step is
//! the whole revision, so that anything the server did not foresee is still
//! applied. Ordinary `needs` between two modules of one host never make a
//! stage on their own: only an edge that crosses a cluster boundary does.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// One edge of a `needs` list, as written: `module:name`, `workload:name`,
/// or a bare name, which refers to the same layer as the list's owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    Module(String),
    Workload(String),
}

impl Need {
    /// Parses a `needs` entry written on a module.
    pub fn on_module(raw: &str) -> Need {
        match raw.strip_prefix("workload:") {
            Some(w) => Need::Workload(w.trim().to_owned()),
            None => Need::Module(raw.strip_prefix("module:").unwrap_or(raw).trim().to_owned()),
        }
    }

    /// Parses a `needs` entry written on a workload.
    pub fn on_workload(raw: &str) -> Need {
        match raw.strip_prefix("module:") {
            Some(m) => Need::Module(m.trim().to_owned()),
            None => Need::Workload(
                raw.strip_prefix("workload:")
                    .unwrap_or(raw)
                    .trim()
                    .to_owned(),
            ),
        }
    }
}

/// A module as selected on one host of the phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleUnit {
    pub name: String,
    pub needs: Vec<Need>,
}

/// A host of the phase with the modules the revision selects on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInput {
    pub id: String,
    pub modules: Vec<ModuleUnit>,
}

/// A workload as selected on one cluster of the phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadUnit {
    pub name: String,
    pub needs: Vec<Need>,
}

/// A cluster of the phase: its node hosts (those of the phase) and the
/// workloads the revision selects on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterInput {
    pub id: String,
    pub nodes: Vec<String>,
    pub workloads: Vec<WorkloadUnit>,
}

/// One step of a target: the stage it runs in and the modules it applies
/// (`None` for the whole target: every module, or every workload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    #[serde(default)]
    pub stage: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modules: Option<Vec<String>>,
}

impl Step {
    /// The task selector of a RequestPlan for this step.
    pub fn select(&self) -> String {
        match &self.modules {
            None => String::new(),
            Some(modules) => modules
                .iter()
                .map(|m| format!("module.name == {}", serde_json::Value::String(m.clone())))
                .collect::<Vec<_>>()
                .join(" || "),
        }
    }
}

/// The stages of one phase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Staged {
    /// Each target's steps, in stage order (at least one per target).
    pub steps: BTreeMap<String, Vec<Step>>,
    /// How many stages the phase has (at least one).
    pub stages: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Unit {
    Module(String, String),
    Cluster(String),
}

impl Unit {
    fn target(&self) -> &str {
        match self {
            Unit::Module(h, _) | Unit::Cluster(h) => h,
        }
    }

    fn crosses(&self, other: &Unit) -> bool {
        matches!(
            (self, other),
            (Unit::Module(..), Unit::Cluster(_)) | (Unit::Cluster(_), Unit::Module(..))
        )
    }
}

/// Lays the phase's targets out in stages. Cycles cannot happen in a valid
/// revision (its loader rejects them); should one slip through, the units
/// on it stay in the stage their acyclic predecessors give them.
pub fn layer(hosts: &[HostInput], clusters: &[ClusterInput]) -> Staged {
    let mut units: BTreeSet<Unit> = BTreeSet::new();
    let mut preds: HashMap<Unit, Vec<Unit>> = HashMap::new();
    let mut edge = |from: Unit, to: Unit| {
        preds.entry(to).or_default().push(from);
    };
    let host_modules: HashMap<&str, BTreeSet<&str>> = hosts
        .iter()
        .map(|h| {
            (
                h.id.as_str(),
                h.modules.iter().map(|m| m.name.as_str()).collect(),
            )
        })
        .collect();
    for h in hosts {
        for m in &h.modules {
            let unit = Unit::Module(h.id.clone(), m.name.clone());
            units.insert(unit.clone());
            for need in &m.needs {
                match need {
                    Need::Module(n) => {
                        if host_modules[h.id.as_str()].contains(n.as_str()) {
                            edge(Unit::Module(h.id.clone(), n.clone()), unit.clone());
                        }
                    }
                    Need::Workload(w) => {
                        for c in clusters {
                            if c.nodes.iter().any(|n| n == &h.id)
                                && c.workloads.iter().any(|x| &x.name == w)
                            {
                                edge(Unit::Cluster(c.id.clone()), unit.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    for c in clusters {
        let unit = Unit::Cluster(c.id.clone());
        units.insert(unit.clone());
        for w in &c.workloads {
            for need in &w.needs {
                if let Need::Module(m) = need {
                    for node in &c.nodes {
                        if host_modules
                            .get(node.as_str())
                            .is_some_and(|ms| ms.contains(m.as_str()))
                        {
                            edge(Unit::Module(node.clone(), m.clone()), unit.clone());
                        }
                    }
                }
            }
        }
    }
    // Longest path over the units, counting only the edges that cross a
    // cluster boundary (Kahn's algorithm; units on a cycle are left out of
    // the ordering and keep stage 0).
    let mut indegree: HashMap<&Unit, usize> = units.iter().map(|u| (u, 0)).collect();
    let mut successors: HashMap<&Unit, Vec<&Unit>> = HashMap::new();
    for (to, froms) in &preds {
        for from in froms {
            if units.contains(from) && units.contains(to) {
                *indegree.get_mut(to).expect("unit") += 1;
                successors.entry(from).or_default().push(to);
            }
        }
    }
    let mut stage: HashMap<&Unit, u32> = units.iter().map(|u| (u, 0)).collect();
    let mut ready: Vec<&Unit> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(u, _)| *u)
        .collect();
    ready.sort();
    while let Some(u) = ready.pop() {
        let s = stage[u];
        if let Some(next) = successors.get(u) {
            for &v in next {
                let w = u32::from(u.crosses(v));
                let candidate = s + w;
                if candidate > stage[v] {
                    stage.insert(v, candidate);
                }
                let d = indegree.get_mut(v).expect("unit");
                *d -= 1;
                if *d == 0 {
                    ready.push(v);
                    ready.sort();
                }
            }
        }
    }

    let mut steps: BTreeMap<String, Vec<Step>> = BTreeMap::new();
    let mut by_target: BTreeMap<&str, BTreeMap<u32, Vec<String>>> = BTreeMap::new();
    for (u, s) in &stage {
        let slot = by_target
            .entry(u.target())
            .or_default()
            .entry(*s)
            .or_default();
        if let Unit::Module(_, m) = u {
            slot.push(m.clone());
        }
    }
    let mut stages = 1;
    for (target, levels) in by_target {
        let last = levels.len() - 1;
        let list: Vec<Step> = levels
            .into_iter()
            .enumerate()
            .map(|(i, (stage, mut modules))| {
                modules.sort();
                Step {
                    stage,
                    modules: (i < last).then_some(modules),
                }
            })
            .collect();
        stages = stages.max(list.last().map_or(0, |s| s.stage) + 1);
        steps.insert(target.to_owned(), list);
    }
    // Targets with nothing selected still have one whole step.
    for h in hosts {
        steps.entry(h.id.clone()).or_insert_with(|| vec![whole()]);
    }
    for c in clusters {
        steps.entry(c.id.clone()).or_insert_with(|| vec![whole()]);
    }
    Staged { steps, stages }
}

/// The one step of a target without cross-layer dependencies.
pub fn whole() -> Step {
    Step {
        stage: 0,
        modules: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(name: &str, needs: &[&str]) -> ModuleUnit {
        ModuleUnit {
            name: name.into(),
            needs: needs.iter().map(|n| Need::on_module(n)).collect(),
        }
    }

    fn workload(name: &str, needs: &[&str]) -> WorkloadUnit {
        WorkloadUnit {
            name: name.into(),
            needs: needs.iter().map(|n| Need::on_workload(n)).collect(),
        }
    }

    fn host(id: &str, modules: Vec<ModuleUnit>) -> HostInput {
        HostInput {
            id: id.into(),
            modules,
        }
    }

    fn cluster(id: &str, nodes: &[&str], workloads: Vec<WorkloadUnit>) -> ClusterInput {
        ClusterInput {
            id: id.into(),
            nodes: nodes.iter().map(|n| (*n).to_owned()).collect(),
            workloads,
        }
    }

    #[test]
    fn hosts_alone_are_one_stage_of_whole_steps() {
        let staged = layer(
            &[
                host("a", vec![module("base", &[]), module("web", &["base"])]),
                host("b", vec![]),
            ],
            &[],
        );
        assert_eq!(staged.stages, 1);
        assert_eq!(staged.steps["a"], vec![whole()]);
        assert_eq!(staged.steps["b"], vec![whole()]);
    }

    #[test]
    fn a_workload_needing_a_module_puts_the_cluster_after_its_nodes() {
        let staged = layer(
            &[
                host("pi-1", vec![module("nomad", &[])]),
                host("pi-2", vec![module("nomad", &[])]),
                host("laptop", vec![module("nomad", &[])]),
            ],
            &[cluster(
                "home",
                &["pi-1", "pi-2"],
                vec![workload("postgresql", &["module:nomad"])],
            )],
        );
        assert_eq!(staged.stages, 2);
        assert_eq!(staged.steps["pi-1"][0].stage, 0);
        assert_eq!(
            staged.steps["home"],
            vec![Step {
                stage: 1,
                modules: None
            }]
        );
        assert_eq!(staged.steps["laptop"], vec![whole()]);
    }

    #[test]
    fn a_host_on_both_sides_of_a_workload_gets_two_steps() {
        let staged = layer(
            &[host(
                "pi-1",
                vec![
                    module("tailscale", &[]),
                    module("nomad", &["tailscale"]),
                    module("backup-agent", &["workload:postgresql"]),
                    module("backup-cron", &["backup-agent"]),
                ],
            )],
            &[cluster(
                "home",
                &["pi-1"],
                vec![workload("postgresql", &["module:nomad"])],
            )],
        );
        assert_eq!(staged.stages, 3);
        let steps = &staged.steps["pi-1"];
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].stage, 0);
        assert_eq!(
            steps[0].modules.as_deref(),
            Some(&["nomad".to_owned(), "tailscale".to_owned()][..])
        );
        assert_eq!(
            steps[0].select(),
            r#"module.name == "nomad" || module.name == "tailscale""#
        );
        assert_eq!(
            steps[1],
            Step {
                stage: 2,
                modules: None
            }
        );
        assert_eq!(staged.steps["home"][0].stage, 1);
    }

    #[test]
    fn edges_to_hosts_outside_the_cluster_or_phase_are_ignored() {
        let staged = layer(
            &[host(
                "other",
                vec![module("backup-agent", &["workload:postgresql"])],
            )],
            &[cluster(
                "home",
                &["pi-1"],
                vec![workload("postgresql", &["module:nomad"])],
            )],
        );
        assert_eq!(staged.stages, 1);
        assert_eq!(staged.steps["other"], vec![whole()]);
        assert_eq!(staged.steps["home"], vec![whole()]);
    }

    #[test]
    fn needs_between_workloads_do_not_stage() {
        let staged = layer(
            &[host("pi-1", vec![module("nomad", &[])])],
            &[cluster(
                "home",
                &["pi-1"],
                vec![
                    workload("platform", &["module:nomad"]),
                    workload("paste", &["platform"]),
                ],
            )],
        );
        assert_eq!(staged.stages, 2);
        assert_eq!(staged.steps["home"].len(), 1);
    }

    #[test]
    fn need_prefixes_resolve_by_the_owner_layer() {
        assert_eq!(Need::on_module("nomad"), Need::Module("nomad".into()));
        assert_eq!(
            Need::on_module("module:nomad"),
            Need::Module("nomad".into())
        );
        assert_eq!(Need::on_module("workload:pg"), Need::Workload("pg".into()));
        assert_eq!(Need::on_workload("pg"), Need::Workload("pg".into()));
        assert_eq!(
            Need::on_workload("module:nomad"),
            Need::Module("nomad".into())
        );
    }
}
