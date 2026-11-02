//! Host views shared by the gRPC and HTTP fronts.

use crate::source::cache::RevisionCache;
use crate::store::{HostRecord, Store, kv::keys};
use buckle_api::{
    DiffKind, HandlerView, HostSummary, ModuleNote, ModuleView, PlanView, RehomeView, RemovalView,
    StepOutcome, StepView,
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
    }
}

/// The UI's typed view of an engine plan (design §11).
pub fn plan_view(plan: &Plan) -> PlanView {
    use buckle_core::plan::{Diff, Planned, RehomeKind};
    let modules = plan
        .modules
        .iter()
        .map(|m| ModuleView {
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

/// Loads and summarises one host.
pub async fn load_summary(
    store: &Store,
    revisions: &RevisionCache,
    id: &str,
) -> Option<HostSummary> {
    let id = id.to_owned();
    let (host, facts) = store
        .call(move |db| {
            let Some(h) = db.hosts().get(&id)? else {
                return Ok(None);
            };
            let facts = db.hosts().latest_facts(&id)?.map(|(f, _)| f);
            Ok(Some((h, facts.unwrap_or(serde_json::Value::Null))))
        })
        .await
        .ok()
        .flatten()?;
    let fleet = reference_revision(store, revisions).await;
    Some(host_summary(&host, &facts, fleet.as_deref()))
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
        assert_eq!(view.held[0].module, "backup");
        assert!(view.indeterminate[0].plan_error);
        assert_eq!(view.rehomed[0].how, "moved");
        assert_eq!(view.rehomed[0].from, "old/file.copy[/etc/x]");
        assert_eq!(view.warnings.len(), 1);
        assert_eq!(view.skipped.len(), 1);
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
