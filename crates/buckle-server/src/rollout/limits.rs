//! What a rollout's modules and workloads have in flight, queued and failed,
//! against their own `parallel` and `max_failures`: one account for the
//! controller (which starts targets by it) and the API (which shows it).

use std::collections::{BTreeSet, HashMap};

use buckle_core::engine::Revision;
use buckle_core::model::{FailureBudget, Parallelism};

use super::model::{HostPhase, RolloutHostRecord};
use super::{bake, view};

/// How many targets of a rollout run a module or workload, which of them
/// hold a place of its `parallel` (in flight, or failed), and which failed
/// on it.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub total: usize,
    pub in_flight: usize,
    /// Targets still to start.
    pub queued: usize,
    /// Failed targets that run it: each keeps its place.
    pub failed_holders: Vec<String>,
    /// Failed targets whose failure is this module's or workload's.
    pub failures: Vec<String>,
}

/// Row `h`'s plan changes nothing (no changes, removals or deferred
/// modules): it takes no slot and counts against no limit.
pub fn unchanged(h: &RolloutHostRecord) -> bool {
    let r = &h.runtime;
    r.plan_changes == Some(0) && r.plan_removals == Some(0) && r.deferred_modules.is_empty()
}

/// The modules row `h`'s plan expects to change, when it has one.
pub fn plan_changes(h: &RolloutHostRecord) -> Option<BTreeSet<String>> {
    let plan = h.plan.as_ref()?;
    serde_json::from_value::<buckle_core::plan::Plan>(plan.clone())
        .ok()
        .map(|p| bake::planned(&p))
}

/// The modules or workloads row `h`'s failure is charged to: the module
/// whose task failed and those of its failed checks, else what its plan
/// changes, else everything it runs.
pub fn attributed(h: &RolloutHostRecord) -> Vec<String> {
    let layer = if h.runtime.kind.on_cluster() {
        "workload"
    } else {
        "module"
    };
    let mut named: BTreeSet<String> = h.runtime.failed_module.iter().cloned().collect();
    named.extend(
        h.runtime
            .failed_checks
            .iter()
            .filter_map(|c| c.split_once('/').map(|(m, _)| m.to_owned())),
    );
    if named.is_empty() && !h.runtime.kind.on_cluster() {
        named = plan_changes(h).unwrap_or_default();
    }
    let named: Vec<String> = named
        .into_iter()
        .map(|m| format!("{layer}:{m}"))
        .filter(|k| h.runtime.runs.contains(k))
        .collect();
    if named.is_empty() {
        h.runtime.runs.clone()
    } else {
        named
    }
}

/// What every module and workload of a rollout has in flight and failed,
/// across its phases: a `parallel` and a `max_failures` on a module or
/// workload count fleet-wide.
pub fn usage(hosts: &[RolloutHostRecord]) -> HashMap<String, Usage> {
    let mut usage: HashMap<String, Usage> = HashMap::new();
    for h in hosts {
        let in_flight = h.state.in_flight() && !unchanged(h);
        let failed = h.state == HostPhase::Failed;
        let queued = matches!(h.state, HostPhase::PlanPending | HostPhase::Planned);
        for key in &h.runtime.runs {
            let u = usage.entry(key.clone()).or_default();
            u.total += 1;
            u.in_flight += usize::from(in_flight);
            u.queued += usize::from(queued);
            if failed {
                u.failed_holders.push(view::row_key(h));
            }
        }
        if failed {
            for key in attributed(h) {
                usage
                    .entry(key)
                    .or_default()
                    .failures
                    .push(view::row_key(h));
            }
        }
    }
    usage
}

/// The `parallel` and `max_failures` of the module or workload `key`
/// (`module:NAME`, `workload:NAME`) names.
pub fn of(revision: &Revision, key: &str) -> (Option<Parallelism>, Option<FailureBudget>) {
    match key.split_once(':') {
        Some(("module", m)) => revision
            .module(m)
            .map(|m| (m.config.parallel, m.config.max_failures)),
        Some(("workload", w)) => revision
            .workload(w)
            .map(|w| (w.config.parallel, w.config.max_failures)),
        _ => None,
    }
    .unwrap_or((None, None))
}

/// Failures the module or workload `key` tolerates among the `total`
/// targets that run it: none unless it sets `max_failures` (a count as is;
/// a percentage rounds down).
pub fn budget(revision: &Revision, key: &str, total: usize) -> usize {
    match of(revision, key).1 {
        None => 0,
        Some(Parallelism::Count(n)) => n as usize,
        Some(Parallelism::Percent(p)) => total * p as usize / 100,
    }
}

/// The limits worth showing: every module or workload that sets a
/// `parallel` or a `max_failures`, and any other that has a failure charged
/// to it (which stops its other targets), by layer and name.
pub fn view(revision: &Revision, hosts: &[RolloutHostRecord]) -> Vec<buckle_api::RolloutLimit> {
    let usage = usage(hosts);
    let mut out: Vec<buckle_api::RolloutLimit> = usage
        .iter()
        .filter_map(|(key, u)| {
            let (parallel, max_failures) = of(revision, key);
            if parallel.is_none() && max_failures.is_none() && u.failures.is_empty() {
                return None;
            }
            let (kind, name) = key.split_once(':')?;
            Some(buckle_api::RolloutLimit {
                kind: match kind {
                    "module" => buckle_api::RolloutLimitKind::Module,
                    _ => buckle_api::RolloutLimitKind::Workload,
                },
                name: name.to_owned(),
                parallel: parallel.map(|p| p.resolve(u.total) as u32),
                max_failures: budget(revision, key, u.total) as u32,
                total: u.total as u32,
                in_flight: u.in_flight as u32,
                queued: u.queued as u32,
                holding: u.failed_holders.clone(),
                failures: u.failures.clone(),
            })
        })
        .collect();
    out.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    out
}
