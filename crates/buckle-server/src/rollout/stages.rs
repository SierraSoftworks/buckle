//! Dependency scheduling: what each target of a rollout waits for.
//!
//! `needs` may leave a target. A workload may need a module on its
//! cluster's nodes (`module:nomad`), another workload on its cluster, a
//! workload on one cluster (`workload:vault:nomad`) or a fleet group
//! (`group:k3s-server`); a module may need a workload (on the cluster its
//! host is a node of, else on every cluster that runs it), a group, or a
//! module its own host does not run (then: on every host that does). Each
//! such entry is resolved here into the rows that satisfy it, and recorded
//! on the target's step as a [`Wait`]: the controller starts a target once
//! every wait of its current step is met, a phase slot is free and the
//! `parallel` limits of what it runs allow it. There are no barriers
//! between stages: a target waits for what it needs and nothing else.
//!
//! A host is one unit: it waits whole for everything any of its modules
//! needs. Only when a cycle runs through a host (a workload needs one of
//! its modules while another of its modules needs that workload) is it laid
//! out in **steps**: the earlier steps name the modules that must go first
//! (a module-scoped plan, `module.name == …`), and the last step is the
//! whole revision, so that anything the server did not foresee is still
//! applied. A cycle no such split resolves is reported, and the rollout is
//! refused. Ordinary `needs` between two modules of one host never make a
//! step: the engine orders those inside the one apply.
//!
//! A cluster rolled by **placements** has one unit per workload (the row of
//! that workload on the cluster, keyed `cluster/workload`); a cluster
//! applied as one unit (an executor that predates placements) is one unit
//! with the cluster's key, and needs between its workloads order the apply
//! inside it instead.
//!
//! Only needs inside one phase order it. A need on a target of another
//! phase (or of none) is still recorded as a wait, and the controller
//! decides it when the target comes up: done by then, already running, or
//! missed.

use buckle_core::model::Need;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A module as selected on one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleUnit {
    pub name: String,
    pub needs: Vec<Need>,
}

/// A host of the rollout, the phase it is in and the modules the revision
/// selects on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInput {
    pub id: String,
    pub phase: usize,
    pub modules: Vec<ModuleUnit>,
}

/// A workload as selected on one cluster, and the phase its placement is
/// in. For a removal placement, `needs` names the placements that needed
/// the workload: it goes after them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadUnit {
    pub name: String,
    pub phase: usize,
    pub needs: Vec<Need>,
}

/// A cluster of the rollout: its node hosts and the workloads the revision
/// selects on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterInput {
    pub id: String,
    /// The cluster the target is a part of: itself, or for a scoped target
    /// the cluster it is scoped to. A workload's `needs` reach the other
    /// targets of the same cluster.
    pub family: String,
    /// The phase of a cluster applied as one unit (its placements carry
    /// their own).
    pub phase: usize,
    pub nodes: Vec<String>,
    pub workloads: Vec<WorkloadUnit>,
    /// Each workload is a placement with a row of its own (keyed
    /// `cluster/workload`); otherwise the cluster applies as one unit.
    pub placements: bool,
}

/// What a rollout schedules: its hosts and clusters, and the members (host
/// or cluster ids, in the rollout or not) of each fleet group a `needs`
/// names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fleet {
    pub hosts: Vec<HostInput>,
    pub clusters: Vec<ClusterInput>,
    pub groups: BTreeMap<String, Vec<String>>,
}

/// A row a step waits for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Pred {
    /// The row's key: a host, a cluster, or `cluster/workload`.
    pub key: String,
    /// The step of that row that must be done (a host laid out in steps,
    /// when the need is for a module of an earlier one); `None`: the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<usize>,
}

/// One `needs` entry of a step, resolved to the rows that satisfy it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wait {
    /// The entry in its written form (`group:k3s-server`).
    pub need: String,
    /// The module declaring it, on a host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub on: Vec<Pred>,
}

impl Wait {
    /// `k3s-agent needs group:k3s-server`, or `needs module:nomad`.
    pub fn label(&self) -> String {
        match &self.via {
            Some(via) => format!("{via} needs {}", self.need),
            None => format!("needs {}", self.need),
        }
    }
}

/// One step of a target: the modules it applies (`None` for the whole
/// target: every module, or every workload), what it waits for, and where
/// it sits in its phase's dependency order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// The length of the longest chain of needs before the step, inside its
    /// phase: 0 for one that waits for nothing there.
    #[serde(default)]
    pub stage: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modules: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<Wait>,
    /// The length of the longest chain of needs behind the step: what
    /// starting it early unblocks.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rank: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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

/// The schedule of a rollout.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Staged {
    /// Each target's steps in order (at least one per target), keyed by
    /// host, cluster, or `cluster/workload` for a placement.
    pub steps: BTreeMap<String, Vec<Step>>,
    /// How deep each phase's dependency order goes (at least 1).
    pub stages: BTreeMap<usize, u32>,
    /// Dependency cycles no split of a host resolves, each as the needs
    /// along it (`timepi: buckle needs group:buckle-server`).
    pub cycles: Vec<Vec<String>>,
}

/// A unit of the configuration on a target.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Unit {
    Module(String, String),
    Workload(String, String),
    Cluster(String),
}

impl Unit {
    /// The row the unit belongs to.
    fn key(&self) -> String {
        match self {
            Unit::Module(h, _) | Unit::Cluster(h) => h.clone(),
            Unit::Workload(c, w) => buckle_api::placement_key(c, Some(w)),
        }
    }
}

/// What one `needs` entry points at.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Unit(Unit),
    /// Every module of a host.
    Whole(String),
    /// A row the rollout does not have.
    Absent(String),
}

/// One `needs` entry of a unit, resolved.
#[derive(Debug, Clone)]
struct Edge {
    need: String,
    targets: Vec<Target>,
}

/// A node of the dependency graph: a host is one node unless it is laid
/// out module by module (`free`), then one per module and one for the host
/// being done.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Node {
    Host(String),
    Module(String, String),
    Done(String),
    Workload(String, String),
    Cluster(String),
}

impl Node {
    fn key(&self) -> String {
        match self {
            Node::Host(h) | Node::Module(h, _) | Node::Done(h) | Node::Cluster(h) => h.clone(),
            Node::Workload(c, w) => buckle_api::placement_key(c, Some(w)),
        }
    }
}

struct Graph {
    nodes: Vec<Node>,
    index: HashMap<Node, usize>,
    /// Per node: (predecessor, weight, what makes it wait).
    preds: Vec<Vec<(usize, u32, Option<String>)>>,
    succs: Vec<Vec<(usize, u32)>>,
}

impl Graph {
    fn node(&mut self, node: Node) -> usize {
        if let Some(&i) = self.index.get(&node) {
            return i;
        }
        let i = self.nodes.len();
        self.index.insert(node.clone(), i);
        self.nodes.push(node);
        self.preds.push(Vec::new());
        self.succs.push(Vec::new());
        i
    }

    fn edge(&mut self, from: usize, to: usize, weight: u32, why: Option<String>) {
        if from == to || self.preds[to].iter().any(|(p, ..)| *p == from) {
            return;
        }
        self.preds[to].push((from, weight, why));
        self.succs[from].push((to, weight));
    }

    /// The nodes in dependency order with the length of the longest chain
    /// before each, and those left out: on a cycle, or after one.
    fn levels(&self) -> (Vec<usize>, Vec<u32>, BTreeSet<usize>) {
        let n = self.nodes.len();
        let mut indegree: Vec<usize> = self.preds.iter().map(Vec::len).collect();
        let mut level = vec![0u32; n];
        let mut ready: Vec<usize> = (0..n).filter(|&i| indegree[i] == 0).collect();
        let mut order = Vec::with_capacity(n);
        while let Some(u) = ready.pop() {
            order.push(u);
            for &(v, w) in &self.succs[u] {
                level[v] = level[v].max(level[u] + w);
                indegree[v] -= 1;
                if indegree[v] == 0 {
                    ready.push(v);
                }
            }
        }
        let left = (0..n).filter(|&i| indegree[i] > 0).collect();
        (order, level, left)
    }

    /// The nodes on a cycle or between two: what [`Graph::levels`] leaves
    /// out, less what only follows a cycle.
    fn cyclic(&self) -> BTreeSet<usize> {
        let (_, _, mut left) = self.levels();
        loop {
            let leaf = left
                .iter()
                .copied()
                .find(|&i| !self.succs[i].iter().any(|(s, _)| left.contains(s)));
            match leaf {
                Some(i) => left.remove(&i),
                None => return left,
            };
        }
    }

    /// One cycle among `stuck`, as the needs along it.
    fn describe(&self, stuck: &BTreeSet<usize>) -> Vec<String> {
        let Some(&start) = stuck.iter().next() else {
            return Vec::new();
        };
        // Walk predecessors until a node repeats: the tail is a cycle.
        let mut path: Vec<(usize, Option<String>)> = Vec::new();
        let mut at = start;
        let from = loop {
            if let Some(i) = path.iter().position(|(n, _)| *n == at) {
                break i;
            }
            let Some((pred, _, why)) = self.preds[at].iter().find(|(p, ..)| stuck.contains(p))
            else {
                return Vec::new();
            };
            path.push((at, why.clone()));
            at = *pred;
        };
        let mut out: Vec<String> = path[from..]
            .iter()
            .rev()
            .filter_map(|(n, why)| {
                why.as_ref()
                    .map(|why| format!("{}: {why}", self.nodes[*n].key()))
            })
            .collect();
        out.dedup();
        out
    }
}

struct Solver<'a> {
    fleet: &'a Fleet,
    host_phase: HashMap<&'a str, usize>,
    host_modules: HashMap<&'a str, BTreeSet<&'a str>>,
    clusters: HashMap<&'a str, &'a ClusterInput>,
    /// Every unit with its phase and resolved needs, hosts first.
    units: Vec<(Unit, usize, Vec<Edge>)>,
}

impl<'a> Solver<'a> {
    fn new(fleet: &'a Fleet) -> Self {
        let mut solver = Solver {
            fleet,
            host_phase: fleet
                .hosts
                .iter()
                .map(|h| (h.id.as_str(), h.phase))
                .collect(),
            host_modules: fleet
                .hosts
                .iter()
                .map(|h| {
                    (
                        h.id.as_str(),
                        h.modules.iter().map(|m| m.name.as_str()).collect(),
                    )
                })
                .collect(),
            clusters: fleet.clusters.iter().map(|c| (c.id.as_str(), c)).collect(),
            units: Vec::new(),
        };
        solver.resolve();
        solver
    }

    /// The unit a workload of a cluster is applied by: its own placement,
    /// or the whole cluster.
    fn workload_unit(c: &ClusterInput, workload: &str) -> Option<Unit> {
        c.workloads.iter().any(|w| w.name == workload).then(|| {
            if c.placements {
                Unit::Workload(c.id.clone(), workload.to_owned())
            } else {
                Unit::Cluster(c.id.clone())
            }
        })
    }

    /// A group's members as targets; `None` when `owner` (a host or a
    /// cluster) is one of them: a member never waits for its own group.
    fn group(&self, name: &str, owner: &str) -> Option<Vec<Target>> {
        let members = self.fleet.groups.get(name).map_or(&[][..], Vec::as_slice);
        if members.iter().any(|m| m == owner) {
            return None;
        }
        let mut out = Vec::new();
        for member in members {
            if self.host_phase.contains_key(member.as_str()) {
                out.push(Target::Whole(member.clone()));
            } else if let Some(c) = self.clusters.get(member.as_str()) {
                if c.placements {
                    out.extend(
                        c.workloads
                            .iter()
                            .map(|w| Target::Unit(Unit::Workload(c.id.clone(), w.name.clone()))),
                    );
                } else {
                    out.push(Target::Unit(Unit::Cluster(c.id.clone())));
                }
            } else {
                out.push(Target::Absent(member.clone()));
            }
        }
        Some(out)
    }

    /// `workload:NAME:CLUSTER` as a target: the placement, in the rollout
    /// or not.
    fn placement(&self, workload: &str, cluster: &str) -> Target {
        match self
            .clusters
            .get(cluster)
            .and_then(|c| Self::workload_unit(c, workload))
        {
            Some(unit) => Target::Unit(unit),
            None => Target::Absent(buckle_api::placement_key(cluster, Some(workload))),
        }
    }

    fn resolve(&mut self) {
        let fleet = self.fleet;
        let mut units = Vec::new();
        for h in &fleet.hosts {
            for m in &h.modules {
                let mut edges = Vec::new();
                for need in &m.needs {
                    let targets = match need {
                        // On this host; one the host does not run: on every
                        // host that does.
                        Need::Module(n)
                            if self.host_modules[h.id.as_str()].contains(n.as_str()) =>
                        {
                            vec![Target::Unit(Unit::Module(h.id.clone(), n.clone()))]
                        }
                        Need::Module(n) => fleet
                            .hosts
                            .iter()
                            .filter(|t| t.modules.iter().any(|x| x.name == *n))
                            .map(|t| Target::Unit(Unit::Module(t.id.clone(), n.clone())))
                            .collect(),
                        // On the clusters the host is a node of; a host
                        // outside them waits for every placement.
                        Need::Workload(w) => {
                            let running: Vec<&ClusterInput> = fleet
                                .clusters
                                .iter()
                                .filter(|c| c.workloads.iter().any(|x| x.name == *w))
                                .collect();
                            let mine: Vec<&ClusterInput> = running
                                .iter()
                                .copied()
                                .filter(|c| c.nodes.iter().any(|n| n == &h.id))
                                .collect();
                            (if mine.is_empty() { running } else { mine })
                                .into_iter()
                                .filter_map(|c| Self::workload_unit(c, w))
                                .map(Target::Unit)
                                .collect()
                        }
                        Need::Placement { workload, cluster } => {
                            vec![self.placement(workload, cluster)]
                        }
                        Need::Group(g) => match self.group(g, &h.id) {
                            Some(targets) => targets,
                            None => continue,
                        },
                    };
                    edges.push(Edge {
                        need: need.qualified(),
                        targets,
                    });
                }
                units.push((Unit::Module(h.id.clone(), m.name.clone()), h.phase, edges));
            }
        }
        for c in &fleet.clusters {
            for w in &c.workloads {
                let unit = Self::workload_unit(c, &w.name).expect("its own workload");
                let mut edges = Vec::new();
                for need in &w.needs {
                    let targets = match need {
                        Need::Module(m) => c
                            .nodes
                            .iter()
                            .filter(|n| {
                                self.host_modules
                                    .get(n.as_str())
                                    .is_some_and(|ms| ms.contains(m.as_str()))
                            })
                            .map(|n| Target::Unit(Unit::Module(n.clone(), m.clone())))
                            .collect(),
                        // Between two placements of the cluster; inside one
                        // cluster unit the apply orders them itself. A
                        // workload the target does not run is looked for on
                        // the other targets of its cluster (a scoped
                        // target's workload needing one of the cluster's).
                        Need::Workload(x) => match Self::workload_unit(c, x) {
                            Some(unit) => (c.placements && *x != w.name)
                                .then_some(Target::Unit(unit))
                                .into_iter()
                                .collect(),
                            None => fleet
                                .clusters
                                .iter()
                                .filter(|o| o.id != c.id && o.family == c.family)
                                .filter_map(|o| Self::workload_unit(o, x))
                                .map(Target::Unit)
                                .collect(),
                        },
                        Need::Placement { workload, cluster } => {
                            if *cluster == c.id && *workload == w.name {
                                continue;
                            }
                            vec![self.placement(workload, cluster)]
                        }
                        Need::Group(g) => match self.group(g, &c.id) {
                            Some(targets) => targets,
                            None => continue,
                        },
                    };
                    edges.push(Edge {
                        need: need.qualified(),
                        targets,
                    });
                }
                let phase = if c.placements { w.phase } else { c.phase };
                units.push((unit, phase, edges));
            }
        }
        self.units = units;
    }

    fn phase_of(&self, node: &Node) -> Option<usize> {
        match node {
            Node::Host(h) | Node::Module(h, _) | Node::Done(h) => {
                self.host_phase.get(h.as_str()).copied()
            }
            Node::Workload(c, w) => self
                .clusters
                .get(c.as_str())?
                .workloads
                .iter()
                .find(|x| x.name == *w)
                .map(|x| x.phase),
            Node::Cluster(c) => self.clusters.get(c.as_str()).map(|c| c.phase),
        }
    }

    fn unit_node(unit: &Unit, free: &BTreeSet<String>) -> Node {
        match unit {
            Unit::Module(h, m) if free.contains(h) => Node::Module(h.clone(), m.clone()),
            Unit::Module(h, _) => Node::Host(h.clone()),
            Unit::Workload(c, w) => Node::Workload(c.clone(), w.clone()),
            Unit::Cluster(c) => Node::Cluster(c.clone()),
        }
    }

    /// The dependency graph with the hosts in `free` laid out module by
    /// module and every other host as one node. Only needs inside a phase
    /// are edges.
    fn graph(&self, free: &BTreeSet<String>) -> Graph {
        let mut g = Graph {
            nodes: Vec::new(),
            index: HashMap::new(),
            preds: Vec::new(),
            succs: Vec::new(),
        };
        for h in &self.fleet.hosts {
            if free.contains(&h.id) {
                let done = g.node(Node::Done(h.id.clone()));
                for m in &h.modules {
                    let module = g.node(Node::Module(h.id.clone(), m.name.clone()));
                    g.edge(module, done, 0, None);
                }
            } else {
                g.node(Node::Host(h.id.clone()));
            }
        }
        for c in self.fleet.clusters.iter().filter(|c| !c.placements) {
            g.node(Node::Cluster(c.id.clone()));
        }
        for (unit, phase, edges) in &self.units {
            let to = g.node(Self::unit_node(unit, free));
            for edge in edges {
                for target in &edge.targets {
                    let (from, weight) = match target {
                        Target::Unit(t) => {
                            let inside = matches!(
                                (t, unit),
                                (Unit::Module(a, _), Unit::Module(b, _)) if a == b
                            );
                            (Self::unit_node(t, free), u32::from(!inside))
                        }
                        Target::Whole(h) if free.contains(h) => (Node::Done(h.clone()), 1),
                        Target::Whole(h) => (Node::Host(h.clone()), 1),
                        Target::Absent(_) => continue,
                    };
                    if self.phase_of(&from) != Some(*phase) {
                        continue;
                    }
                    let why = match unit {
                        Unit::Module(_, m) => format!("{m} needs {}", edge.need),
                        _ => format!("needs {}", edge.need),
                    };
                    let from = g.node(from);
                    g.edge(from, to, weight, Some(why));
                }
            }
        }
        g
    }

    /// The hosts to lay out module by module (those a cycle runs through,
    /// and no more than it takes to break every cycle), the graph that
    /// gives, and what is still on a cycle.
    fn split(&self) -> (BTreeSet<String>, Graph, BTreeSet<usize>) {
        let mut free = BTreeSet::new();
        let (graph, stuck) = loop {
            let g = self.graph(&free);
            let stuck = g.cyclic();
            let hosts: Vec<String> = stuck
                .iter()
                .filter_map(|&i| match &g.nodes[i] {
                    Node::Host(h) => Some(h.clone()),
                    _ => None,
                })
                .collect();
            if hosts.is_empty() {
                break (g, stuck);
            }
            free.extend(hosts);
        };
        if !stuck.is_empty() {
            return (free, graph, stuck);
        }
        // Hosts wait whole wherever that leaves no cycle.
        for h in free.clone() {
            free.remove(&h);
            if !self.graph(&free).cyclic().is_empty() {
                free.insert(h);
            }
        }
        let graph = self.graph(&free);
        (free, graph, BTreeSet::new())
    }
}

/// Lays the rollout's targets out: each target's steps, what each step
/// waits for, and any dependency cycle no split of a host resolves.
pub fn layer(fleet: &Fleet) -> Staged {
    let solver = Solver::new(fleet);
    let (free, graph, stuck) = solver.split();
    let cycles = if stuck.is_empty() {
        Vec::new()
    } else {
        vec![graph.describe(&stuck)]
    };
    let (order, level, _) = graph.levels();
    let mut rank = vec![0u32; graph.nodes.len()];
    for &u in order.iter().rev() {
        rank[u] = graph.succs[u]
            .iter()
            .map(|&(v, w)| rank[v] + w)
            .max()
            .unwrap_or(0);
    }
    let at = |node: &Node| graph.index.get(node).map(|&i| (level[i], rank[i]));

    // The module groups of each host laid out in steps, earliest first.
    /// A host's modules of one level: the level, its rank, their names.
    type Group<'a> = (u32, u32, Vec<&'a str>);
    let mut groups: HashMap<&str, Vec<Group>> = HashMap::new();
    for h in fleet.hosts.iter().filter(|h| free.contains(&h.id)) {
        let mut by_level: BTreeMap<u32, (u32, Vec<&str>)> = BTreeMap::new();
        for m in &h.modules {
            let (l, r) = at(&Node::Module(h.id.clone(), m.name.clone())).unwrap_or((0, 0));
            let slot = by_level.entry(l).or_default();
            slot.0 = slot.0.max(r);
            slot.1.push(&m.name);
        }
        let list = by_level
            .into_iter()
            .map(|(l, (r, mut ms))| {
                ms.sort_unstable();
                (l, r, ms)
            })
            .collect();
        groups.insert(&h.id, list);
    }
    // The step of a host that applies one of its modules; `None`: the last.
    let step_of = |host: &str, module: &str| -> Option<usize> {
        let list = groups.get(host)?;
        let i = list.iter().position(|(_, _, ms)| ms.contains(&module))?;
        (i + 1 < list.len()).then_some(i)
    };
    let pred = |target: &Target, own: &str| -> Option<Pred> {
        let (key, step) = match target {
            Target::Unit(Unit::Module(h, m)) => (h.clone(), step_of(h, m)),
            Target::Unit(u) => (u.key(), None),
            Target::Whole(h) => (h.clone(), None),
            Target::Absent(k) => (k.clone(), None),
        };
        (key != own).then_some(Pred { key, step })
    };
    // The waits of the units `picked` of a row: one per written need.
    let waits = |own: &str, picked: &dyn Fn(&Unit) -> bool| -> Vec<Wait> {
        let mut out: Vec<Wait> = Vec::new();
        for (unit, _, edges) in solver.units.iter().filter(|(u, ..)| picked(u)) {
            for edge in edges {
                let on: BTreeSet<Pred> = edge.targets.iter().filter_map(|t| pred(t, own)).collect();
                if on.is_empty() {
                    continue;
                }
                match out.iter_mut().find(|w| w.need == edge.need) {
                    Some(wait) => {
                        let merged: BTreeSet<Pred> = wait.on.iter().cloned().chain(on).collect();
                        wait.on = merged.into_iter().collect();
                    }
                    None => out.push(Wait {
                        need: edge.need.clone(),
                        via: match unit {
                            Unit::Module(_, m) => Some(m.clone()),
                            _ => None,
                        },
                        on: on.into_iter().collect(),
                    }),
                }
            }
        }
        out
    };

    let mut steps: BTreeMap<String, Vec<Step>> = BTreeMap::new();
    let mut stages: BTreeMap<usize, u32> = BTreeMap::new();
    let mut seen = |phase: usize, stage: u32| {
        let depth = stages.entry(phase).or_insert(1);
        *depth = (*depth).max(stage + 1);
    };
    for h in &fleet.hosts {
        let list = match groups.get(h.id.as_str()) {
            Some(list) if list.len() > 1 => {
                let last = list.len() - 1;
                list.iter()
                    .enumerate()
                    .map(|(i, (stage, rank, modules))| Step {
                        stage: *stage,
                        modules: (i < last)
                            .then(|| modules.iter().map(|m| (*m).to_owned()).collect()),
                        needs: waits(&h.id, &|u| {
                            matches!(u, Unit::Module(x, m) if *x == h.id && modules.contains(&m.as_str()))
                        }),
                        // An earlier step also unblocks the host's later ones.
                        rank: list[i..].iter().map(|(_, r, _)| *r).max().unwrap_or(*rank),
                    })
                    .collect()
            }
            _ => {
                let (stage, rank) = at(&Node::Host(h.id.clone()))
                    .or_else(|| at(&Node::Done(h.id.clone())))
                    .unwrap_or((0, 0));
                vec![Step {
                    stage,
                    modules: None,
                    needs: waits(&h.id, &|u| matches!(u, Unit::Module(x, _) if *x == h.id)),
                    rank,
                }]
            }
        };
        for step in &list {
            seen(h.phase, step.stage);
        }
        steps.insert(h.id.clone(), list);
    }
    for c in &fleet.clusters {
        if c.placements {
            for w in &c.workloads {
                let unit = Unit::Workload(c.id.clone(), w.name.clone());
                let (stage, rank) =
                    at(&Node::Workload(c.id.clone(), w.name.clone())).unwrap_or((0, 0));
                seen(w.phase, stage);
                let key = unit.key();
                let needs = waits(&key, &|u| *u == unit);
                steps.insert(
                    key,
                    vec![Step {
                        stage,
                        modules: None,
                        needs,
                        rank,
                    }],
                );
            }
        } else {
            let unit = Unit::Cluster(c.id.clone());
            let (stage, rank) = at(&Node::Cluster(c.id.clone())).unwrap_or((0, 0));
            seen(c.phase, stage);
            steps.insert(
                c.id.clone(),
                vec![Step {
                    stage,
                    modules: None,
                    needs: waits(&c.id, &|u| *u == unit),
                    rank,
                }],
            );
        }
    }
    Staged {
        steps,
        stages,
        cycles,
    }
}

/// The one step of a target that waits for nothing.
pub fn whole() -> Step {
    Step::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_core::model::Layer;

    fn module(name: &str, needs: &[&str]) -> ModuleUnit {
        ModuleUnit {
            name: name.into(),
            needs: needs
                .iter()
                .map(|n| Need::parse(n, Layer::Module))
                .collect(),
        }
    }

    fn workload(name: &str, needs: &[&str]) -> WorkloadUnit {
        WorkloadUnit {
            name: name.into(),
            phase: 0,
            needs: needs
                .iter()
                .map(|n| Need::parse(n, Layer::Workload))
                .collect(),
        }
    }

    fn host(id: &str, modules: Vec<ModuleUnit>) -> HostInput {
        HostInput {
            id: id.into(),
            phase: 0,
            modules,
        }
    }

    /// A cluster applied as one unit.
    fn cluster(id: &str, nodes: &[&str], workloads: Vec<WorkloadUnit>) -> ClusterInput {
        ClusterInput {
            id: id.into(),
            family: id.into(),
            phase: 0,
            nodes: nodes.iter().map(|n| (*n).to_owned()).collect(),
            workloads,
            placements: false,
        }
    }

    /// A cluster rolled by placements.
    fn placed(id: &str, nodes: &[&str], workloads: Vec<WorkloadUnit>) -> ClusterInput {
        ClusterInput {
            placements: true,
            ..cluster(id, nodes, workloads)
        }
    }

    fn fleet(hosts: Vec<HostInput>, clusters: Vec<ClusterInput>) -> Fleet {
        Fleet {
            hosts,
            clusters,
            groups: BTreeMap::new(),
        }
    }

    fn grouped(mut fleet: Fleet, groups: &[(&str, &[&str])]) -> Fleet {
        fleet.groups = groups
            .iter()
            .map(|(g, members)| {
                (
                    (*g).to_owned(),
                    members.iter().map(|m| (*m).to_owned()).collect(),
                )
            })
            .collect();
        fleet
    }

    /// The rows `key`'s step `step` waits for, as `row` or `row#step`.
    fn waits_for(staged: &Staged, key: &str, step: usize) -> Vec<String> {
        let mut out: Vec<String> = staged.steps[key][step]
            .needs
            .iter()
            .flat_map(|w| &w.on)
            .map(|p| match p.step {
                Some(s) => format!("{}#{s}", p.key),
                None => p.key.clone(),
            })
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn stage(staged: &Staged, key: &str) -> u32 {
        staged.steps[key].last().expect("a step").stage
    }

    #[test]
    fn hosts_alone_are_whole_steps_that_wait_for_nothing() {
        let staged = layer(&fleet(
            vec![
                host("a", vec![module("base", &[]), module("web", &["base"])]),
                host("b", vec![]),
            ],
            vec![],
        ));
        assert_eq!(staged.stages[&0], 1);
        assert_eq!(staged.steps["a"], vec![whole()]);
        assert_eq!(staged.steps["b"], vec![whole()]);
        assert!(staged.cycles.is_empty());
    }

    #[test]
    fn a_workload_needing_a_module_waits_for_its_nodes() {
        let staged = layer(&fleet(
            vec![
                host("pi-1", vec![module("nomad", &[])]),
                host("pi-2", vec![module("nomad", &[])]),
                host("laptop", vec![module("nomad", &[])]),
            ],
            vec![cluster(
                "home",
                &["pi-1", "pi-2"],
                vec![workload("postgresql", &["module:nomad"])],
            )],
        ));
        assert_eq!(staged.stages[&0], 2);
        assert_eq!(waits_for(&staged, "home", 0), ["pi-1", "pi-2"]);
        assert_eq!(stage(&staged, "home"), 1);
        assert_eq!(staged.steps["laptop"], vec![whole()]);
        assert!(
            staged.steps["pi-1"][0].rank > staged.steps["laptop"][0].rank,
            "what unblocks others ranks first: {:?}",
            staged.steps
        );
    }

    /// A cycle through a host (a workload needs one of its modules, another
    /// of its modules needs the workload) lays it out in steps.
    #[test]
    fn a_host_on_both_sides_of_a_workload_gets_two_steps() {
        let staged = layer(&fleet(
            vec![host(
                "pi-1",
                vec![
                    module("tailscale", &[]),
                    module("nomad", &["tailscale"]),
                    module("backup-agent", &["workload:postgresql"]),
                    module("backup-cron", &["backup-agent"]),
                ],
            )],
            vec![cluster(
                "home",
                &["pi-1"],
                vec![workload("postgresql", &["module:nomad"])],
            )],
        ));
        assert!(staged.cycles.is_empty(), "{:?}", staged.cycles);
        assert_eq!(staged.stages[&0], 3);
        let steps = &staged.steps["pi-1"];
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[0].modules.as_deref(),
            Some(&["nomad".to_owned(), "tailscale".to_owned()][..])
        );
        assert_eq!(
            steps[0].select(),
            r#"module.name == "nomad" || module.name == "tailscale""#
        );
        assert!(steps[0].needs.is_empty());
        assert_eq!((steps[1].stage, steps[1].modules.as_ref()), (2, None));
        assert_eq!(waits_for(&staged, "pi-1", 1), ["home"]);
        assert_eq!(waits_for(&staged, "home", 0), ["pi-1#0"]);
    }

    /// Without a cycle a host waits whole, whatever its modules need.
    #[test]
    fn a_host_waits_whole_for_what_any_of_its_modules_needs() {
        let staged = layer(&fleet(
            vec![
                host("node", vec![module("nomad", &[])]),
                host(
                    "app",
                    vec![
                        module("base", &[]),
                        module("vault-agent", &["workload:vault"]),
                    ],
                ),
            ],
            vec![placed(
                "home",
                &["node"],
                vec![workload("vault", &["module:nomad"])],
            )],
        ));
        assert_eq!(staged.steps["app"].len(), 1);
        assert_eq!(waits_for(&staged, "app", 0), ["home/vault"]);
        assert_eq!(
            staged.steps["app"][0].needs[0].label(),
            "vault-agent needs workload:vault"
        );
        assert_eq!(stage(&staged, "app"), 2);
    }

    /// A host outside the clusters that run a workload waits for every
    /// placement of it; `workload:NAME:CLUSTER` for that one alone, in the
    /// rollout or not.
    #[test]
    fn a_workload_need_binds_to_the_hosts_cluster_else_to_every_placement() {
        let staged = layer(&fleet(
            vec![
                host("east-1", vec![module("agent", &["workload:vault"])]),
                host("other", vec![module("agent", &["workload:vault"])]),
                host("pinned", vec![module("pin", &["workload:vault:west"])]),
                host("lost", vec![module("far", &["workload:vault:gone"])]),
            ],
            vec![
                placed("east", &["east-1"], vec![workload("vault", &[])]),
                placed("west", &[], vec![workload("vault", &[])]),
            ],
        ));
        assert_eq!(waits_for(&staged, "east-1", 0), ["east/vault"]);
        assert_eq!(waits_for(&staged, "other", 0), ["east/vault", "west/vault"]);
        assert_eq!(waits_for(&staged, "pinned", 0), ["west/vault"]);
        assert_eq!(waits_for(&staged, "lost", 0), ["gone/vault"]);
    }

    #[test]
    fn a_group_need_waits_for_every_member_but_never_for_its_own_group() {
        let agent = || module("k3s-agent", &["group:servers"]);
        let staged = layer(&grouped(
            fleet(
                vec![
                    host("server-1", vec![module("k3s-server", &[]), agent()]),
                    host("server-2", vec![module("k3s-server", &[])]),
                    host("agent-1", vec![agent()]),
                ],
                vec![placed(
                    "home",
                    &[],
                    vec![workload("app", &["group:servers"])],
                )],
            ),
            &[("servers", &["server-1", "server-2", "elsewhere"])],
        ));
        assert_eq!(staged.steps["server-1"], vec![Step { rank: 1, ..whole() }]);
        assert_eq!(
            waits_for(&staged, "agent-1", 0),
            ["elsewhere", "server-1", "server-2"]
        );
        assert_eq!(
            waits_for(&staged, "home/app", 0),
            ["elsewhere", "server-1", "server-2"]
        );
        assert_eq!(stage(&staged, "agent-1"), 1);
    }

    /// A module needing one its host does not run waits for it on the hosts
    /// that do, which breaks the cycle two hosts serving each other would
    /// make with whole-host needs. Only one of them is split, and its first
    /// step takes every module whose needs are met by then, not just the
    /// one the other host waits for.
    #[test]
    fn hosts_that_serve_each_other_split_once_and_front_load_what_is_free() {
        let staged = layer(&grouped(
            fleet(
                vec![
                    host(
                        "nuc",
                        vec![
                            module("base", &[]),
                            module("buckle", &["group:server"]),
                            module("nut-server", &["base"]),
                            module("time-sync", &["ptp-server"]),
                        ],
                    ),
                    host(
                        "timepi",
                        vec![
                            module("base", &[]),
                            module("alloy", &["base"]),
                            module("buckle", &["group:server"]),
                            module("nut-client", &["nut-server"]),
                            module("ptp-server", &["alloy"]),
                            module("ups-alerts", &["nut-client"]),
                        ],
                    ),
                ],
                vec![],
            ),
            &[("server", &["nuc"])],
        ));
        assert!(staged.cycles.is_empty(), "{:?}", staged.cycles);
        assert_eq!(staged.steps["nuc"].len(), 1, "{:?}", staged.steps);
        assert_eq!(waits_for(&staged, "nuc", 0), ["timepi#0"]);
        let timepi = &staged.steps["timepi"];
        assert_eq!(timepi.len(), 2);
        assert_eq!(
            timepi[0].modules.as_deref(),
            Some(
                &[
                    "alloy".to_owned(),
                    "base".to_owned(),
                    "ptp-server".to_owned()
                ][..]
            ),
            "everything that does not wait for the nuc goes first"
        );
        assert!(timepi[0].needs.is_empty());
        assert_eq!(waits_for(&staged, "timepi", 1), ["nuc"]);
    }

    #[test]
    fn a_cycle_no_split_resolves_is_reported() {
        let staged = layer(&grouped(
            fleet(
                vec![
                    host("blue", vec![module("a", &["group:green"])]),
                    host("green", vec![module("b", &["group:blue"])]),
                    host("other", vec![module("c", &[])]),
                ],
                vec![],
            ),
            &[("blue", &["blue"]), ("green", &["green"])],
        ));
        assert_eq!(staged.cycles.len(), 1);
        let cycle = staged.cycles[0].join(" -> ");
        assert!(
            cycle.contains("blue: a needs group:green")
                && cycle.contains("green: b needs group:blue"),
            "{cycle}"
        );
    }

    /// Only needs inside a phase order it; one across phases is still a
    /// wait for the controller to decide.
    #[test]
    fn needs_across_phases_are_waits_but_not_a_cycle() {
        let mut later = host("server", vec![module("s", &["group:canaries"])]);
        later.phase = 1;
        let staged = layer(&grouped(
            fleet(
                vec![
                    host("canary", vec![module("agent", &["group:servers"])]),
                    later,
                ],
                vec![],
            ),
            &[("servers", &["server"]), ("canaries", &["canary"])],
        ));
        assert!(staged.cycles.is_empty());
        assert_eq!(waits_for(&staged, "canary", 0), ["server"]);
        assert_eq!(waits_for(&staged, "server", 0), ["canary"]);
        assert_eq!((stage(&staged, "canary"), stage(&staged, "server")), (0, 0));
    }

    #[test]
    fn needs_between_workloads_of_one_cluster_unit_do_not_stage() {
        let staged = layer(&fleet(
            vec![host("pi-1", vec![module("nomad", &[])])],
            vec![cluster(
                "home",
                &["pi-1"],
                vec![
                    workload("platform", &["module:nomad"]),
                    workload("paste", &["platform"]),
                ],
            )],
        ));
        assert_eq!(staged.stages[&0], 2);
        assert_eq!(stage(&staged, "home"), 1);
        assert_eq!(waits_for(&staged, "home", 0), ["pi-1"]);
    }

    /// Placements are units of their own: a workload needing another on the
    /// cluster waits for it, and a module needing a workload waits for that
    /// placement alone.
    #[test]
    fn placements_wait_on_every_needs_edge() {
        let staged = layer(&fleet(
            vec![host(
                "pi-1",
                vec![module("nomad", &[]), module("backup", &["workload:paste"])],
            )],
            vec![placed(
                "home",
                &["pi-1"],
                vec![
                    workload("platform", &["module:nomad"]),
                    workload("paste", &["platform"]),
                    workload("monitoring", &[]),
                ],
            )],
        ));
        assert_eq!(staged.stages[&0], 4);
        assert_eq!(stage(&staged, "home/platform"), 1);
        assert_eq!(waits_for(&staged, "home/paste", 0), ["home/platform"]);
        assert_eq!(staged.steps["home/monitoring"], vec![whole()]);
        assert!(!staged.steps.contains_key("home"), "{:?}", staged.steps);
        let steps = &staged.steps["pi-1"];
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].modules.as_deref(), Some(&["nomad".to_owned()][..]));
        assert_eq!(steps[1].stage, 3);
        assert!(
            steps[0].rank > staged.steps["home/monitoring"][0].rank,
            "{steps:?}"
        );
    }

    /// A scoped target's workload may need one its cluster runs (the
    /// namespace it lands in, say): the need reaches across the targets of
    /// one cluster, and no further.
    #[test]
    fn a_need_reaches_the_other_targets_of_the_same_cluster() {
        let scoped = |id: &str, family: &str| ClusterInput {
            family: family.into(),
            ..placed(id, &[], vec![workload("site", &["platform"])])
        };
        let staged = layer(&fleet(
            vec![],
            vec![
                placed("nomad", &[], vec![workload("platform", &[])]),
                scoped("nomad-public", "nomad"),
                scoped("elsewhere", "elsewhere"),
            ],
        ));
        assert_eq!(
            waits_for(&staged, "nomad-public/site", 0),
            ["nomad/platform"]
        );
        assert!(waits_for(&staged, "elsewhere/site", 0).is_empty());
    }

    /// A removal placement names the placements that needed its workload as
    /// its needs, so it goes after them.
    #[test]
    fn a_removal_goes_after_the_placements_that_needed_it() {
        let staged = layer(&fleet(
            vec![],
            vec![placed(
                "home",
                &[],
                vec![workload("paste", &[]), workload("platform", &["paste"])],
            )],
        ));
        assert_eq!(staged.stages[&0], 2);
        assert_eq!(
            staged.steps["home/paste"],
            vec![Step { rank: 1, ..whole() }]
        );
        assert_eq!(stage(&staged, "home/platform"), 1);
    }
}
