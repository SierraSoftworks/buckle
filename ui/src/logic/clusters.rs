//! Cluster targets as the UI reads them: a cluster's workloads row by row
//! (what the revision declares beside what was applied, how it reads and
//! what the run in progress is doing to it), its tally for a glance, what a
//! cluster in a rollout is waiting for, and keeping the tally current from
//! run updates.

use buckle_api::{
    ClusterSpec, ClusterStatus, Deploying, HostCheck, HostDetail, HostRolloutState, ModuleState,
    RolloutHost, RolloutPhase, RunProgress, RunReport, StepState, TargetKind,
};
use chrono::{DateTime, Utc};

use super::format::count;
use super::run::run_finished;

/// How a workload stands, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkloadState {
    /// A required check fails now.
    Failing,
    /// The run in progress is applying it.
    Deploying,
    /// Applied, and nothing reads otherwise.
    Running,
    /// Declared, but nothing of it has been applied yet.
    NotApplied,
}

impl WorkloadState {
    pub fn label(self) -> &'static str {
        match self {
            WorkloadState::Failing => "failing",
            WorkloadState::Deploying => "deploying",
            WorkloadState::Running => "running",
            WorkloadState::NotApplied => "not applied yet",
        }
    }
}

/// A row of the cluster page's Workloads panel.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkloadRow {
    pub name: String,
    pub description: String,
    pub needs: Vec<String>,
    pub state: WorkloadState,
    /// The latest progress of the step being applied, while it deploys.
    pub progress: Option<String>,
    /// Its checks: how many read `pass`, of how many.
    pub checks: (usize, usize),
    /// Why a failing check fails, for the first one.
    pub reason: Option<String>,
    /// When its resources were last applied, and in which revision.
    pub applied: Option<(DateTime<Utc>, String)>,
}

/// Where the cluster's own UI shows a resource of `kind` and `key`: its
/// jobs, when the cluster has a UI Buckle links into.
pub fn resource_url(spec: Option<&ClusterSpec>, kind: &str, key: &str) -> Option<String> {
    match kind {
        "nomad.job" => spec?.job_url(key),
        _ => None,
    }
}

/// One row from what the detail's resources, checks and the run in
/// progress (`live`) say of module or workload `name`.
fn row_of(
    d: &HostDetail,
    live: Option<&RunReport>,
    name: &str,
    description: &str,
    needs: &[String],
) -> WorkloadRow {
    let own: Vec<_> = d.resources.iter().filter(|r| r.module == name).collect();
    let applied = own
        .iter()
        .max_by_key(|r| r.applied_at)
        .map(|r| (r.applied_at, r.revision.clone()));
    let list: Vec<&HostCheck> = d
        .health_checks
        .iter()
        .filter(|c| c.module == name)
        .collect();
    let failing = list
        .iter()
        .find(|c| c.required && c.status.eq_ignore_ascii_case("fail"));
    let passing = list
        .iter()
        .filter(|c| c.status.eq_ignore_ascii_case("pass"))
        .count();
    let progress: Option<Option<String>> = match live {
        Some(r) => r
            .modules
            .iter()
            .find(|m| m.name == name && m.state == ModuleState::Running)
            .map(|m| {
                m.steps
                    .iter()
                    .find(|s| s.state == StepState::Running)
                    .and_then(|s| s.result.clone())
            }),
        None => d
            .summary
            .cluster_status
            .as_ref()
            .and_then(|s| s.deploying.iter().find(|x| x.workload == name))
            .map(|x| x.progress.clone()),
    };
    let state = if failing.is_some() {
        WorkloadState::Failing
    } else if progress.is_some() {
        WorkloadState::Deploying
    } else if own.is_empty() && list.is_empty() {
        WorkloadState::NotApplied
    } else {
        WorkloadState::Running
    };
    WorkloadRow {
        name: name.to_owned(),
        description: description.to_owned(),
        needs: needs.to_vec(),
        state,
        progress: progress
            .flatten()
            .map(|p| p.trim_start_matches("deploying: ").to_owned()),
        checks: (passing, list.len()),
        reason: failing.and_then(|c| c.reason.clone()),
        applied,
    }
}

/// The cluster's workloads, as the revision declares them, each with what
/// the detail's resources, checks and the run in progress say of it.
/// Workloads the cluster still runs that the revision no longer selects
/// follow, so nothing applied goes unlisted.
pub fn workload_rows(d: &HostDetail, run: Option<&RunReport>) -> Vec<WorkloadRow> {
    let live = run.filter(|r| !run_finished(r.state));
    let mut rows: Vec<WorkloadRow> = d
        .workloads
        .iter()
        .map(|w| row_of(d, live, &w.name, &w.description, &w.needs))
        .collect();
    let mut leftover: Vec<&str> = d
        .resources
        .iter()
        .map(|r| r.module.as_str())
        .filter(|m| !d.workloads.iter().any(|w| w.name == *m))
        .collect();
    leftover.dedup();
    for name in leftover {
        let mut r = row_of(d, live, name, "", &[]);
        r.description = "no longer selected by the revision".into();
        rows.push(r);
    }
    rows
}

/// A host's modules, the same way: those of `run` in plan order, then any
/// other it has resources or checks of.
pub fn module_rows(d: &HostDetail, run: Option<&RunReport>) -> Vec<WorkloadRow> {
    let live = run.filter(|r| !run_finished(r.state));
    let mut names: Vec<&str> = Vec::new();
    let planned = run
        .into_iter()
        .flat_map(|r| r.modules.iter().map(|m| m.name.as_str()));
    let applied = d.resources.iter().map(|r| r.module.as_str());
    let checked = d.health_checks.iter().map(|c| c.module.as_str());
    for name in planned.chain(applied).chain(checked) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
        .into_iter()
        .map(|name| row_of(d, live, name, "", &[]))
        .collect()
}

/// What a cluster row says at a glance: its workload count, then only the
/// unusual (failing, deploying).
pub fn tally(status: &ClusterStatus) -> Vec<String> {
    let mut parts = vec![count(status.workloads as usize, "workload", "workloads")];
    if !status.failing.is_empty() {
        parts.push(format!("{} failing", status.failing.join(", ")));
    }
    if !status.deploying.is_empty() {
        parts.push(format!("deploying {}", deploying_text(&status.deploying)));
    }
    parts
}

/// `atuin (server 1/2 healthy), paste`.
pub fn deploying_text(list: &[Deploying]) -> String {
    list.iter()
        .map(|d| match &d.progress {
            Some(p) => format!("{} ({})", d.workload, p.trim_start_matches("deploying: ")),
            None => d.workload.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Keeps a cluster's tally of workloads in deployment current from a run
/// update: a workload starts deploying when its module runs, says how far
/// it got with each step's progress, and stops when its module or the run
/// ends.
pub fn track_deploying(status: &mut ClusterStatus, p: &RunProgress) {
    if run_finished(p.run_state) {
        status.deploying.clear();
        return;
    }
    let Some(module) = &p.module else { return };
    match p.module_state {
        Some(ModuleState::Running) => {
            if !status.deploying.iter().any(|d| &d.workload == module) {
                status.deploying.push(Deploying {
                    workload: module.clone(),
                    progress: None,
                });
            }
            return;
        }
        Some(_) => {
            status.deploying.retain(|d| &d.workload != module);
            return;
        }
        None => {}
    }
    // A step's progress (no state of its own) while its workload deploys.
    if p.step.is_some()
        && p.step_state.is_none()
        && let Some(d) = status.deploying.iter_mut().find(|d| &d.workload == module)
    {
        d.progress = p.result.clone();
    }
}

/// What a target in a rollout waits for before it is planned and applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiting {
    /// The targets of an earlier dependency stage of its phase that have
    /// not finished (a cluster waits for the hosts its workloads need).
    Stage { stage: u32, on: Vec<String> },
    /// Its stage is open, but no connected host can speak for the cluster.
    Executor,
}

impl Waiting {
    pub fn describe(&self) -> String {
        match self {
            Waiting::Stage { on, .. } => {
                const SHOWN: usize = 3;
                let mut names: Vec<&str> = on.iter().take(SHOWN).map(String::as_str).collect();
                let more = on.len().saturating_sub(SHOWN);
                let rest = (more > 0).then(|| format!("{more} more"));
                names.extend(rest.as_deref());
                format!("waits for {}", names.join(", "))
            }
            Waiting::Executor => "awaiting an executor".into(),
        }
    }
}

/// Whether a target is through its step, so later stages may go on.
fn through(state: HostRolloutState) -> bool {
    matches!(state, HostRolloutState::Healthy | HostRolloutState::Skipped)
}

/// What `entry` waits for in `phase`, when it has not started: the
/// unfinished targets of the stages before its own, else (a cluster) an
/// executor.
pub fn waiting(phase: &RolloutPhase, entry: &RolloutHost) -> Option<Waiting> {
    if !matches!(
        entry.state,
        HostRolloutState::Pending | HostRolloutState::PlanPending | HostRolloutState::Planned
    ) {
        return None;
    }
    let on: Vec<String> = phase
        .hosts
        .iter()
        .filter(|h| h.step < entry.step && !through(h.state))
        .map(|h| h.host.clone())
        .collect();
    if !on.is_empty() {
        return Some(Waiting::Stage {
            stage: entry.step,
            on,
        });
    }
    (entry.kind == TargetKind::Cluster && entry.executor.is_none()).then_some(Waiting::Executor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{ResourceSummary, RunState, WorkloadView};
    use chrono::TimeZone;

    fn t(m: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 4, 12, 0, 0).unwrap() + chrono::Duration::minutes(m)
    }

    fn detail() -> HostDetail {
        let mut summary: buckle_api::HostSummary = serde_json::from_value(serde_json::json!({
            "id": "home-nomad", "kind": "cluster", "status": "active", "connection": "offline",
            "agent_labels": {}, "labels": {}, "effective_labels": {}, "groups": [],
            "platform": null, "arch": null, "agent_version": null, "applied_revision": "r1",
            "health": "failing", "health_at": null, "drifted": false, "first_seen": t(0),
            "last_seen": null,
        }))
        .unwrap();
        summary.cluster = Some(buckle_api::ClusterInfo {
            spec: buckle_api::ClusterSpec {
                kind: buckle_api::ClusterKind::Nomad,
                description: String::new(),
                api: Some("http://nomad.home:4646".into()),
                ui: None,
                nodes: None,
                executors: None,
                identity: "buckle_jwt".into(),
                cluster: None,
                scope: Default::default(),
                trust: None,
            },
            declared: true,
        });
        let w = |name: &str, needs: &[&str]| WorkloadView {
            name: name.into(),
            needs: needs.iter().map(|n| n.to_string()).collect(),
            resources: vec!["job".into()],
            ..Default::default()
        };
        let res = |module: &str, kind: &str, key: &str, at: i64| ResourceSummary {
            module: module.into(),
            kind: kind.into(),
            key: key.into(),
            revision: "r1".into(),
            applied_at: t(at),
            removal: "cleanup".into(),
        };
        let check = |module: &str, status: &str| HostCheck {
            module: module.into(),
            check: "job".into(),
            kind: "nomad.job".into(),
            required: true,
            status: status.into(),
            reason: (status == "fail").then(|| "0/1 running".into()),
            scope: "drift".into(),
            at: t(5),
            history: vec![],
        };
        HostDetail {
            summary,
            facts: serde_json::Value::Null,
            facts_at: None,
            resources: vec![
                res("postgresql", "nomad.job", "default/postgresql", 1),
                res("atuin", "nomad.variable", "nomad/jobs/atuin", 2),
                res("atuin", "nomad.job", "apps/atuin", 3),
                res("retired", "nomad.job", "default/retired", 0),
            ],
            recent_runs: vec![],
            health_results: vec![],
            health_checks: vec![check("postgresql", "fail"), check("atuin", "pass")],
            drift: vec![],
            latest_run: None,
            actions: vec![],
            executors: vec![],
            workloads: vec![
                w("postgresql", &[]),
                w("atuin", &["postgresql"]),
                w("paste", &[]),
            ],
        }
    }

    fn row<'a>(rows: &'a [WorkloadRow], name: &str) -> &'a WorkloadRow {
        rows.iter().find(|r| r.name == name).unwrap()
    }

    #[test]
    fn workloads_read_their_checks_resources_and_jobs() {
        let rows = workload_rows(&detail(), None);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["postgresql", "atuin", "paste", "retired"]);
        let pg = row(&rows, "postgresql");
        assert_eq!(pg.state, WorkloadState::Failing);
        assert_eq!(pg.reason.as_deref(), Some("0/1 running"));
        let atuin = row(&rows, "atuin");
        assert_eq!(atuin.state, WorkloadState::Running);
        assert_eq!(atuin.checks, (1, 1));
        assert_eq!(atuin.applied.as_ref().map(|a| a.0), Some(t(3)));
        assert_eq!(row(&rows, "paste").state, WorkloadState::NotApplied);
        assert!(row(&rows, "retired").description.contains("no longer"));
    }

    #[test]
    fn only_jobs_link_into_the_clusters_ui() {
        let d = detail();
        let spec = d.summary.cluster.as_ref().map(|c| &c.spec);
        assert_eq!(
            resource_url(spec, "nomad.job", "apps/atuin").as_deref(),
            Some("http://nomad.home:4646/ui/jobs/atuin@apps")
        );
        assert_eq!(
            resource_url(spec, "nomad.variable", "nomad/jobs/atuin"),
            None
        );
        assert_eq!(resource_url(None, "nomad.job", "apps/atuin"), None);
    }

    #[test]
    fn a_workload_deploys_while_its_module_runs_and_shows_its_progress() {
        let mut d = detail();
        d.summary.cluster_status = Some(ClusterStatus {
            deploying: vec![Deploying {
                workload: "paste".into(),
                progress: None,
            }],
            ..Default::default()
        });
        // Without a report, the summary's tally says what deploys.
        let rows = workload_rows(&d, None);
        assert_eq!(row(&rows, "paste").state, WorkloadState::Deploying);

        // With one, the report does, and the running step's result is its
        // progress.
        let report: RunReport = serde_json::from_value(serde_json::json!({
            "id": "run-1", "host": "home-nomad", "revision": "r2", "plan_digest": "d",
            "state": "running", "updated_at": t(6),
            "modules": [{
                "name": "atuin", "state": "running",
                "steps": [{"id": "nomad.job[apps/atuin]", "kind": "nomad.job", "summary": "update",
                           "state": "running", "result": "deploying: server 1/2 healthy"}]
            }],
            "removals": [], "counts": buckle_api::RunCounts::default()
        }))
        .unwrap();
        let rows = workload_rows(&d, Some(&report));
        let atuin = row(&rows, "atuin");
        assert_eq!(atuin.state, WorkloadState::Deploying);
        assert_eq!(atuin.progress.as_deref(), Some("server 1/2 healthy"));
        assert_eq!(row(&rows, "paste").state, WorkloadState::NotApplied);
    }

    #[test]
    fn a_host_lists_its_run_modules_first_then_what_it_holds_or_checks() {
        let mut d = detail();
        d.workloads.clear();
        let report: RunReport = serde_json::from_value(serde_json::json!({
            "id": "run-1", "host": "pi-1", "revision": "r2", "plan_digest": "d",
            "state": "running", "updated_at": t(6),
            "modules": [
                {"name": "paste", "state": "running", "steps": []},
                {"name": "atuin", "state": "unchanged", "steps": []}
            ],
            "removals": [], "counts": buckle_api::RunCounts::default()
        }))
        .unwrap();
        let rows = module_rows(&d, Some(&report));
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["paste", "atuin", "postgresql", "retired"]);
        assert_eq!(row(&rows, "paste").state, WorkloadState::Deploying);
        assert_eq!(row(&rows, "postgresql").state, WorkloadState::Failing);
        assert!(
            rows.iter().all(|r| r.description.is_empty()),
            "nothing reads as retired"
        );
    }

    fn progress(module: Option<ModuleState>, step: bool, result: Option<&str>) -> RunProgress {
        RunProgress {
            host: "home-nomad".into(),
            run_id: "run-1".into(),
            rollout_id: None,
            at: t(0),
            run_state: RunState::Running,
            module: Some("atuin".into()),
            module_state: module,
            step: step.then(|| "nomad.job[apps/atuin]".into()),
            step_state: None,
            result: result.map(Into::into),
            error: None,
        }
    }

    #[test]
    fn the_tally_follows_run_updates() {
        let mut s = ClusterStatus {
            workloads: 3,
            ..Default::default()
        };
        track_deploying(&mut s, &progress(Some(ModuleState::Running), false, None));
        track_deploying(
            &mut s,
            &progress(None, true, Some("deploying: server 1/2 healthy")),
        );
        assert_eq!(
            tally(&s),
            vec!["3 workloads", "deploying atuin (server 1/2 healthy)"]
        );
        track_deploying(&mut s, &progress(Some(ModuleState::Applied), false, None));
        assert!(s.deploying.is_empty());
        track_deploying(&mut s, &progress(Some(ModuleState::Running), false, None));
        let mut done = progress(None, false, None);
        done.run_state = RunState::Succeeded;
        track_deploying(&mut s, &done);
        assert!(s.deploying.is_empty(), "a finished run deploys nothing");
    }

    fn entry(host: &str, stage: u32, state: HostRolloutState, kind: TargetKind) -> RolloutHost {
        let mut e: RolloutHost = serde_json::from_value(serde_json::json!({
            "host": host, "phase": "home", "state": state, "updated_at": t(0),
        }))
        .unwrap();
        e.step = stage;
        e.kind = kind;
        e
    }

    #[test]
    fn a_cluster_says_what_it_waits_for() {
        let cases = [
            // An earlier stage's host is still applying.
            (
                HostRolloutState::Applying,
                None,
                Some(Waiting::Stage {
                    stage: 1,
                    on: vec!["pi-1".into()],
                }),
            ),
            // Its stage is open, but nobody speaks for it.
            (HostRolloutState::Healthy, None, Some(Waiting::Executor)),
            (HostRolloutState::Healthy, Some("pi-1"), None),
        ];
        for (node, executor, expected) in cases {
            let mut cluster = entry(
                "home-nomad",
                1,
                HostRolloutState::Planned,
                TargetKind::Cluster,
            );
            cluster.executor = executor.map(Into::into);
            let mut phase: RolloutPhase = serde_json::from_value(serde_json::json!({
                "name": "home", "state": "running", "parallel": "1", "approval": "auto",
                "bake_seconds": 0, "max_failures": "0", "virtual_stage": false, "hosts": [],
            }))
            .unwrap();
            phase.hosts = vec![
                entry("pi-1", 0, node, TargetKind::Host),
                entry("pi-2", 0, HostRolloutState::Skipped, TargetKind::Host),
                cluster.clone(),
            ];
            assert_eq!(waiting(&phase, &cluster), expected, "{node:?} {executor:?}");
            let applying = RolloutHost {
                state: HostRolloutState::Applying,
                ..cluster
            };
            assert_eq!(
                waiting(&phase, &applying),
                None,
                "a target under way waits for nothing"
            );
        }
    }

    #[test]
    fn a_long_wait_names_a_few_and_counts_the_rest() {
        let w = Waiting::Stage {
            stage: 1,
            on: (1..=5).map(|i| format!("pi-{i}")).collect(),
        };
        assert_eq!(w.describe(), "waits for pi-1, pi-2, pi-3, 2 more");
    }
}
