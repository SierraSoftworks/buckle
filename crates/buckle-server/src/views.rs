//! Host views shared by the gRPC and HTTP fronts.

mod report;

pub use report::{
    error_view, event_key, finished_progress, outcome_error, pending_report, progress, run_report,
    run_state,
};

use crate::source::cache::RevisionCache;
use crate::store::{HostRecord, Store, kv::keys};
use buckle_api::{
    ClusterStatus, Deploying, DiffKind, HandlerView, HostSummary, ModuleNote, ModuleView, PlanView,
    RehomeView, RemovalView, StepOutcome, StepView, WorkloadView,
};
use buckle_core::engine::Revision;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::Plan;
use serde::Deserialize as _;
use std::sync::Arc;

/// The stable revision, loaded, when there is one.
pub async fn stable_revision(store: &Store, revisions: &RevisionCache) -> Option<Arc<Revision>> {
    let id = store
        .call(|db| db.kv().get(keys::STABLE_REVISION))
        .await
        .ok()
        .flatten()?;
    revisions.load(&id).await.ok()
}

/// The newest valid revision (for evaluating groups before anything is stable).
pub async fn reference_revision(store: &Store, revisions: &RevisionCache) -> Option<Arc<Revision>> {
    if let Some(r) = stable_revision(store, revisions).await {
        return Some(r);
    }
    let list = store.call(|db| db.revisions().list(20, 0)).await.ok()?;
    let valid = list
        .into_iter()
        .find(|r| r.status == buckle_api::RevisionStatus::Valid)?;
    revisions.load(&valid.id).await.ok()
}

/// [`reference_revision`], with the cluster targets it declares brought in
/// line first (see [`crate::targets::sync_declared`]), so a listing made
/// right after reads the repository's clusters.
pub async fn reference_revision_synced(
    store: &Store,
    revisions: &RevisionCache,
) -> Option<Arc<Revision>> {
    let rev = reference_revision(store, revisions).await?;
    let r = rev.clone();
    let now = chrono::Utc::now();
    if let Err(e) = store
        .call(move |db| crate::targets::sync_declared(db, &r, now))
        .await
    {
        tracing::warn!(error = %e, "the declared cluster targets could not be synced");
    }
    Some(rev)
}

/// The summary row of a host, with groups evaluated against `fleet`.
pub fn host_summary(
    host: &HostRecord,
    facts: &serde_json::Value,
    fleet: Option<&Revision>,
) -> HostSummary {
    let groups = fleet
        .map(|r| crate::fleet::record_context(host, facts, &r.fleet).groups)
        .unwrap_or_default();
    HostSummary {
        id: host.id.clone(),
        kind: host.kind,
        cluster: host.cluster.clone(),
        status: host.status,
        connection: host.connection,
        agent_labels: host.agent_labels.clone(),
        labels: host.labels.clone(),
        effective_labels: host.effective_labels(),
        groups,
        platform: host.platform.clone(),
        arch: host.arch.clone(),
        agent_version: host.agent_version.clone(),
        applied_revision: host.applied_revision.clone(),
        health: host.health,
        health_at: host.health_at,
        drifted: host.drifted,
        first_seen: host.first_seen,
        last_seen: host.last_seen,
        notes: host.notes.clone(),
        upgrade: None,
        cluster_status: None,
    }
}

/// The workloads `revision` selects for cluster target `host`, as the
/// cluster page lists them.
pub fn workload_views(
    revision: &Revision,
    host: &HostRecord,
    facts: &serde_json::Value,
) -> Vec<WorkloadView> {
    let ctx = crate::fleet::record_context(host, facts, &revision.fleet);
    buckle_core::engine::selected_workloads(revision, &ctx)
        .into_iter()
        .map(|w| WorkloadView {
            name: w.config.name.clone(),
            description: w.config.description.clone(),
            needs: w.config.needs.clone(),
            resources: w
                .config
                .resources
                .iter()
                .map(|r| r.kind.as_str().to_owned())
                .collect(),
            removal: format!("{:?}", w.config.removal).to_lowercase(),
        })
        .collect()
}

/// How cluster target `host` stands: its last executor (kv
/// `executor/<id>`), how many workloads `fleet` selects for it, which of
/// them fail a required check, and which its run in progress is applying.
/// `None` for a host.
pub fn cluster_status(
    db: &crate::store::Db<'_>,
    host: &HostRecord,
    facts: &serde_json::Value,
    fleet: Option<&Revision>,
) -> buckle_core::Result<Option<ClusterStatus>> {
    use buckle_core::engine::ApplyEvent;
    if host.kind != buckle_api::TargetKind::Cluster {
        return Ok(None);
    }
    let executor = db
        .kv()
        .get(&format!("{}{}", keys::EXECUTOR_PREFIX, host.id))?;
    let workloads = fleet
        .map(|r| {
            let ctx = crate::fleet::record_context(host, facts, &r.fleet);
            buckle_core::engine::selected_workloads(r, &ctx).len() as u32
        })
        .unwrap_or_default();
    let mut failing: Vec<String> = db
        .health()
        .checks(&host.id)?
        .into_iter()
        .filter(|c| c.required && c.status == "fail")
        .map(|c| c.module)
        .collect();
    failing.sort();
    failing.dedup();

    // Modules started and not finished in the run in progress, each with
    // its latest progress message.
    let mut deploying: Vec<Deploying> = Vec::new();
    if let Some(run) = db
        .runs()
        .latest_for_host(&host.id)?
        .filter(|r| r.is_running())
    {
        for e in db.runs().events(&run.id)? {
            let Ok(event) = serde_json::from_str::<ApplyEvent>(&e.event) else {
                continue;
            };
            match event {
                ApplyEvent::ModuleStarted { module } => {
                    if !deploying.iter().any(|d| d.workload == module) {
                        deploying.push(Deploying {
                            workload: module,
                            progress: None,
                        });
                    }
                }
                ApplyEvent::ModuleFinished { module, .. } => {
                    deploying.retain(|d| d.workload != module)
                }
                ApplyEvent::StepProgress {
                    module, message, ..
                } => {
                    if let Some(d) = deploying.iter_mut().find(|d| d.workload == module) {
                        d.progress = Some(message);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(Some(ClusterStatus {
        executor,
        workloads,
        failing,
        deploying,
    }))
}

/// The UI's typed view of an engine plan (design §11).
pub fn plan_view(plan: &Plan) -> PlanView {
    use buckle_core::plan::{Diff, Planned, RehomeKind};
    // Deferred modules previewed ahead of their providers follow the
    // planned ones, marked deferred.
    let previews = plan
        .deferred
        .iter()
        .filter_map(|d| d.preview.as_ref().map(|p| (p, true)));
    let modules = plan
        .modules
        .iter()
        .map(|m| (m, false))
        .chain(previews)
        .map(|(m, deferred)| ModuleView {
            deferred,
            module: m.module.clone(),
            needs: m.needs.clone(),
            steps: m
                .steps
                .iter()
                .map(|s| {
                    let mut step = StepView {
                        kind: s.resource.kind.clone(),
                        key: s.resource.key.clone(),
                        task: s.task.id.clone(),
                        name: s.task.name.clone(),
                        locks: s.locks.clone(),
                        notifies: s.notifies.clone(),
                        always_runs: s.always_runs,
                        facts: s.facts.clone(),
                        display: s.display.clone(),
                        ..Default::default()
                    };
                    match &s.planned {
                        Planned::NoOp => {}
                        Planned::Blocked { reason } => {
                            step.outcome = StepOutcome::Blocked;
                            step.summary = reason.clone();
                        }
                        Planned::Change { diff } => {
                            step.outcome = StepOutcome::Change;
                            step.summary = diff.summary().to_owned();
                            step.destructive = diff.is_destructive();
                            step.diff = Some(match diff {
                                Diff::Create { preview, .. } => {
                                    step.preview = preview.as_ref().map(|p| p.get().clone());
                                    DiffKind::Create
                                }
                                Diff::Update { unified, .. } => {
                                    step.unified = unified.as_ref().map(|u| u.get().clone());
                                    DiffKind::Update
                                }
                                Diff::Delete { .. } => DiffKind::Delete,
                                Diff::Opaque { .. } => DiffKind::Opaque,
                            });
                        }
                    }
                    step
                })
                .collect(),
            handlers: m
                .handlers
                .iter()
                .map(|h| HandlerView {
                    handler: h.handler.clone(),
                    kind: h.kind.clone(),
                    summary: h.summary.clone(),
                })
                .collect(),
        })
        .collect();
    let note = |module: &str, reason: &str| ModuleNote {
        module: module.to_owned(),
        reason: reason.to_owned(),
        ..Default::default()
    };
    PlanView {
        revision: plan.revision.0.clone(),
        host: plan.host.clone(),
        digest: plan.digest.to_string(),
        modules,
        removals: plan
            .removals
            .iter()
            .map(|r| RemovalView {
                module: r.resource.module.clone(),
                kind: r.resource.kind.clone(),
                key: r.resource.key.clone(),
                summary: r.summary.clone(),
                cleanup: r.policy == RemovalPolicy::Cleanup,
                caution: r.caution,
                introduced_in: r.introduced_in.as_ref().map(|i| i.0.clone()),
            })
            .collect(),
        rehomed: plan
            .rehomed
            .iter()
            .map(|r| RehomeView {
                from: r.from.to_string(),
                to: r.to.to_string(),
                how: match r.kind {
                    RehomeKind::Moved => "moved",
                    RehomeKind::Superseded => "superseded",
                }
                .into(),
                summary: r.summary.clone(),
            })
            .collect(),
        deferred: plan
            .deferred
            .iter()
            .map(|d| ModuleNote {
                waiting_on: d.waiting_on.clone(),
                ..note(&d.module, &d.reason)
            })
            .collect(),
        held: plan
            .held
            .iter()
            .map(|h| note(&h.module, &h.reason))
            .collect(),
        indeterminate: plan
            .indeterminate
            .iter()
            .map(|i| ModuleNote {
                plan_error: i.plan_error,
                ..note(&i.module, &i.reason)
            })
            .collect(),
        skipped: plan.skipped.clone(),
        warnings: plan.warnings.clone(),
    }
}

/// [`plan_view`] of a stored plan document (`rollout_hosts.plan`), or
/// `None` when there is none or it is not a plan this server can read.
pub fn stored_plan_view(plan: Option<&serde_json::Value>) -> Option<PlanView> {
    let plan = plan.filter(|p| !p.is_null())?;
    match Plan::deserialize(plan) {
        Ok(plan) => Some(plan_view(&plan)),
        Err(e) => {
            tracing::debug!(error = %e, "a stored plan could not be read");
            None
        }
    }
}

/// The modules a host applies of `revision`: those of its plan (deferred
/// ones included) when it has one, else those whose `when` matches its
/// labels and facts (an approximation of the engine's selection, which can
/// also hold a module back as indeterminate).
pub fn selected_modules(
    revision: &Revision,
    plan: Option<&PlanView>,
    host: Option<(&HostRecord, &serde_json::Value)>,
) -> Vec<String> {
    if let Some(plan) = plan {
        return plan
            .modules
            .iter()
            .map(|m| m.module.clone())
            .chain(plan.deferred.iter().map(|d| d.module.clone()))
            .collect();
    }
    let ctx = host.map(|(h, facts)| crate::fleet::record_context(h, facts, &revision.fleet));
    revision
        .modules
        .iter()
        .filter(|m| match (&m.config.when, &ctx) {
            (None, _) => true,
            (Some(when), Some(ctx)) => ctx.matches(when),
            (Some(_), None) => false,
        })
        .map(|m| m.config.name.clone())
        .collect()
}

/// The required checks (`module/check`) of the `selected` modules, in the
/// revision's module and declaration order.
pub fn required_checks<'a>(
    revision: &Revision,
    selected: impl IntoIterator<Item = &'a str>,
) -> Vec<String> {
    let selected: std::collections::BTreeSet<&str> = selected.into_iter().collect();
    revision
        .modules
        .iter()
        .filter(|m| selected.contains(m.config.name.as_str()))
        .flat_map(|m| {
            m.config
                .health
                .iter()
                .filter(|h| h.required)
                .map(|h| format!("{}/{}", m.config.name, h.display_name()))
        })
        .collect()
}

/// An audit entry about a rollout as its timeline shows it: the host and
/// phase come from the entry's detail. (`rollout::view::detail` maps the
/// stored trail the same way; a test keeps the two in step.)
pub fn rollout_event(e: &buckle_api::AuditEntry) -> buckle_api::RolloutEvent {
    let detail = |key: &str| {
        e.detail
            .as_ref()
            .and_then(|d| d.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    buckle_api::RolloutEvent {
        at: e.at,
        kind: e.action.clone(),
        message: e.message.clone(),
        host: detail("host"),
        workload: detail("workload").filter(|w| !w.is_empty()),
        phase: detail("phase"),
        actor: Some(e.actor.clone()),
    }
}

/// Loads and summarises one host.
pub async fn load_summary(
    store: &Store,
    revisions: &RevisionCache,
    id: &str,
) -> Option<HostSummary> {
    let fleet = reference_revision_synced(store, revisions).await;
    let id = id.to_owned();
    let rev = fleet.clone();
    store
        .call(move |db| {
            let Some(h) = db.hosts().target(&id)? else {
                return Ok(None);
            };
            let facts = db
                .hosts()
                .latest_facts(&id)?
                .map(|(f, _)| f)
                .unwrap_or(serde_json::Value::Null);
            let mut summary = host_summary(&h, &facts, rev.as_deref());
            summary.cluster_status = cluster_status(db, &h, &facts, rev.as_deref())?;
            Ok(Some(summary))
        })
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use buckle_core::plan::{
        Deferred, Diff, HandlerRun, Held, Indeterminate, ModulePlan, Planned, Rehome, RehomeKind,
        Removal, ResourceKey, RevisionId, Step, TaskRef,
    };
    use buckle_core::secrets::Redacted;

    fn step(kind: &str, key: &str, planned: Planned) -> Step {
        Step {
            task: TaskRef {
                module: "nomad".into(),
                id: key.into(),
                kind: kind.into(),
                name: None,
            },
            resource: ResourceKey::new("nomad", kind, key),
            planned,
            locks: if kind.starts_with("apt.") {
                vec!["apt".into()]
            } else {
                vec![]
            },
            notifies: vec![],
            always_runs: false,
            removal: RemovalPolicy::Cleanup,
            facts: None,
            display: None,
        }
    }

    /// A plan touching every part of the view.
    pub(crate) fn review_plan() -> Plan {
        let removal = |kind: &str, key: &str, policy, caution| Removal {
            resource: ResourceKey::new("grey", kind, key),
            policy,
            summary: format!("remove {key}"),
            caution,
            introduced_in: Some(RevisionId::new("0aa1b2c")),
        };
        Plan::new(
            RevisionId::new("7c2e1f0"),
            "pi-1",
            vec![ModulePlan {
                module: "nomad".into(),
                needs: vec!["docker".into()],
                steps: vec![
                    step(
                        "apt.package",
                        "nomad",
                        Planned::change(Diff::Opaque {
                            summary: "1.9.2 → 1.9.3".into(),
                        }),
                    ),
                    step(
                        "file.template",
                        "/etc/nomad.d/nomad.hcl",
                        Planned::change(Diff::unified(
                            "1 line changed",
                            &Redacted::plain(
                                "servers = [\"«secret:nomad_server@3f9a»\"]\n".to_string(),
                            ),
                            &Redacted::plain(
                                "servers = [\"«secret:nomad_server@b21c»\"]\n".to_string(),
                            ),
                        )),
                    ),
                    step(
                        "file.copy",
                        "/etc/nomad.d/new.hcl",
                        Planned::change(Diff::Create {
                            summary: "create".into(),
                            preview: Some(Redacted::plain("app revision 2\n".into())),
                        }),
                    ),
                    step(
                        "file.absent",
                        "/etc/nomad.d/old.hcl",
                        Planned::change(Diff::Delete {
                            summary: "delete /etc/nomad.d/old.hcl".into(),
                        }),
                    ),
                    step("systemd.unit", "nomad", Planned::NoOp),
                    step(
                        "zfs.dataset",
                        "tank",
                        Planned::blocked("not on linux/arm64"),
                    ),
                ],
                handlers: vec![HandlerRun {
                    module: "nomad".into(),
                    handler: "restart".into(),
                    kind: "systemd.restart".into(),
                    summary: "restart nomad.service".into(),
                }],
                provides_facts: vec![],
            }],
            vec![
                removal("apt.package", "grey-agent", RemovalPolicy::Cleanup, true),
                removal("file.copy", "/var/log/grey", RemovalPolicy::Leave, false),
            ],
            vec![Deferred {
                module: "otel".into(),
                waiting_on: vec!["docker.version".into()],
                reason: "waits for facts from docker".into(),
                preview: None,
            }],
        )
        .with_held(vec![Held {
            module: "backup".into(),
            reason: "last run failed".into(),
        }])
        .with_indeterminate(vec![Indeterminate {
            module: "caddy".into(),
            reason: "reads facts.caddy, which failed".into(),
            plan_error: true,
            error_detail: None,
        }])
        .with_rehomed(vec![Rehome {
            from: ResourceKey::new("old", "file.copy", "/etc/x"),
            to: ResourceKey::new("new", "file.copy", "/etc/x"),
            kind: RehomeKind::Moved,
            summary: "moved to new".into(),
        }])
    }

    #[test]
    fn a_plan_view_keeps_what_a_reviewer_reads() {
        let mut plan = review_plan();
        plan.warnings = vec!["Removing apt.package[grey-agent] can remove packages.".into()];
        plan.skipped = vec!["backup: host.groups contains \"storage\"".into()];
        let view = plan_view(&plan);
        assert_eq!(
            (view.revision.as_str(), view.host.as_str()),
            ("7c2e1f0", "pi-1")
        );
        assert_eq!(view.digest, plan.digest.to_string());
        let m = &view.modules[0];
        assert_eq!(m.needs, vec!["docker"]);
        assert_eq!(m.handlers[0].summary, "restart nomad.service");
        let by_key = |k: &str| m.steps.iter().find(|s| s.key == k).unwrap();

        let pkg = by_key("nomad");
        assert_eq!(pkg.kind, "apt.package");
        assert_eq!(
            (pkg.outcome, pkg.diff, pkg.summary.as_str()),
            (StepOutcome::Change, Some(DiffKind::Opaque), "1.9.2 → 1.9.3")
        );
        assert_eq!(pkg.locks, vec!["apt"]);
        let conf = by_key("/etc/nomad.d/nomad.hcl");
        assert_eq!(conf.diff, Some(DiffKind::Update));
        assert!(
            conf.unified
                .as_deref()
                .is_some_and(|u| u.contains("+servers = [\"«secret:nomad_server@b21c»\"]"))
        );
        assert!(!conf.destructive);
        let new = by_key("/etc/nomad.d/new.hcl");
        assert_eq!(new.diff, Some(DiffKind::Create));
        assert_eq!(new.preview.as_deref(), Some("app revision 2\n"));
        let old = by_key("/etc/nomad.d/old.hcl");
        assert!(old.destructive, "a delete is destructive");
        assert_eq!(by_key("nomad").outcome, StepOutcome::Change);
        assert_eq!(by_key("tank").outcome, StepOutcome::Blocked);
        assert_eq!(by_key("tank").summary, "not on linux/arm64");
        assert_eq!(
            m.steps
                .iter()
                .filter(|s| s.outcome == StepOutcome::NoOp)
                .count(),
            1
        );
        assert_eq!(view.changes(), 4);

        let pkg = &view.removals[0];
        assert!(pkg.cleanup && pkg.caution);
        assert_eq!(pkg.resource(), "grey/apt.package[grey-agent]");
        assert_eq!(pkg.introduced_in.as_deref(), Some("0aa1b2c"));
        assert!(!view.removals[1].cleanup, "removal: leave only forgets");

        assert_eq!(view.deferred[0].waiting_on, vec!["docker.version"]);
        assert!(view.modules.iter().all(|m| !m.deferred));
        assert_eq!(view.held[0].module, "backup");
        assert!(view.indeterminate[0].plan_error);
        assert_eq!(view.rehomed[0].how, "moved");
        assert_eq!(view.rehomed[0].from, "old/file.copy[/etc/x]");
        assert_eq!(view.warnings.len(), 1);
        assert_eq!(view.skipped.len(), 1);
    }

    #[test]
    fn a_deferred_modules_preview_is_shown_after_the_planned_modules() {
        let mut plan = review_plan();
        let mut preview = plan.modules[0].clone();
        preview.module = "otel".into();
        plan.deferred[0].preview = Some(preview);
        let view = plan_view(&plan);
        let names: Vec<(&str, bool)> = view
            .modules
            .iter()
            .map(|m| (m.module.as_str(), m.deferred))
            .collect();
        assert_eq!(names, vec![("nomad", false), ("otel", true)]);
    }

    #[test]
    fn a_live_timeline_entry_reads_like_the_stored_trail() {
        let entry = buckle_api::AuditEntry {
            id: 9,
            at: chrono::Utc::now(),
            actor: "controller".into(),
            action: "rollout.host.failed".into(),
            outcome: "ok".into(),
            subject: Some("7".into()),
            message: "pi-5 failed nomad/api".into(),
            detail: Some(serde_json::json!({"host": "pi-5", "phase": "home", "n": 1})),
            trace_id: None,
        };
        let record = crate::store::RolloutRecord {
            id: "7".into(),
            revision: "r1".into(),
            state: buckle_api::RolloutState::Paused,
            policy: Default::default(),
            started_at: chrono::Utc::now(),
            finished_at: None,
            triggered_by: "controller".into(),
            runtime: Default::default(),
        };
        let stored = crate::rollout::view::detail(&record, &[], &[], std::slice::from_ref(&entry));
        assert_eq!(stored.events, vec![rollout_event(&entry)]);
        let live = rollout_event(&entry);
        assert_eq!(
            (
                live.host.as_deref(),
                live.phase.as_deref(),
                live.actor.as_deref()
            ),
            (Some("pi-5"), Some("home"), Some("controller"))
        );
    }

    #[test]
    fn a_stored_plan_that_is_missing_or_unreadable_has_no_view() {
        assert_eq!(stored_plan_view(None), None);
        assert_eq!(stored_plan_view(Some(&serde_json::Value::Null)), None);
        assert_eq!(stored_plan_view(Some(&serde_json::json!({}))), None);
        let stored = serde_json::to_value(review_plan()).unwrap();
        assert_eq!(
            stored_plan_view(Some(&stored)),
            Some(plan_view(&review_plan()))
        );
    }
}
