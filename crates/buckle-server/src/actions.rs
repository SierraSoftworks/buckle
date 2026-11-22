//! Operator actions a target offers, from the revision it runs: a host's
//! selected modules' `actions:`, a cluster's selected workloads' implicit
//! actions (with their Nomad jobs' task `action` blocks) and their own.
//! The controller runs one ([`crate::rollout::Command::RunAction`]).

use crate::store::HostRecord;
use buckle_api::{ActionInfo, TargetKind};
use buckle_core::engine::Revision;

/// The actions `record` offers at `revision`, in module order: a
/// workload's implicit actions, then its jobs' task actions, then its own.
pub fn list(
    revision: &Revision,
    record: &HostRecord,
    facts: &serde_json::Value,
) -> Vec<ActionInfo> {
    match record.kind {
        TargetKind::Host => {
            let selected = crate::views::selected_modules(revision, None, Some((record, facts)));
            revision
                .modules
                .iter()
                .filter(|m| selected.contains(&m.config.name))
                .flat_map(|m| {
                    // `@suspend` and `@resume` run only through Suspend
                    // and Resume.
                    let own = m
                        .config
                        .actions
                        .iter()
                        .filter(|(name, _)| !buckle_core::model::reserved_action(name));
                    own.map(|(name, decl)| ActionInfo {
                        module: m.config.name.clone(),
                        name: name.clone(),
                        label: decl.label(name).to_owned(),
                        kind: decl.task.kind.clone(),
                        description: decl.description.clone(),
                        confirm: decl.confirm,
                        implicit: false,
                        instances: None,
                    })
                })
                .collect()
        }
        TargetKind::Cluster | TargetKind::Workload => {
            let ctx = crate::fleet::record_context(record, facts, &revision.fleet);
            buckle_core::engine::selected_workloads(revision, &ctx)
                .into_iter()
                .flat_map(|w| {
                    let module = w.config.name.clone();
                    let implicit = w.config.implicit_actions().iter().map({
                        let module = module.clone();
                        move |i| ActionInfo {
                            module: module.clone(),
                            name: i.name.to_owned(),
                            label: i.label.to_owned(),
                            kind: i.name.to_owned(),
                            description: Some(i.description.to_owned()),
                            confirm: i.confirm,
                            implicit: true,
                            instances: None,
                        }
                    });
                    let task = w.task_actions.iter().map({
                        let module = module.clone();
                        move |t| ActionInfo {
                            module: module.clone(),
                            name: t.name.clone(),
                            label: t.name.clone(),
                            kind: buckle_core::engine::task_actions::KIND.to_owned(),
                            description: Some(t.description()),
                            confirm: false,
                            implicit: true,
                            instances: t.choice.then_some(match t.instances {
                                buckle_core::engine::task_actions::Instances::Any => {
                                    buckle_api::ActionInstances::Any
                                }
                                buckle_core::engine::task_actions::Instances::All => {
                                    buckle_api::ActionInstances::All
                                }
                            }),
                        }
                    });
                    let extra = w.config.actions.iter().map(move |(name, decl)| ActionInfo {
                        module: module.clone(),
                        name: name.clone(),
                        label: decl.label(name).to_owned(),
                        kind: decl.task.kind.clone(),
                        description: decl.description.clone(),
                        confirm: decl.confirm,
                        implicit: false,
                        instances: None,
                    });
                    implicit.chain(task).chain(extra).collect::<Vec<_>>()
                })
                .collect()
        }
    }
}

/// What suspending and resuming a unit runs besides pausing it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnitHooks {
    /// Suspending it runs `@suspend` (a workload: stops its jobs).
    pub suspend: bool,
    /// Resuming it runs `@resume` before the pause is lifted.
    pub resume: bool,
}

/// The hooks of `unit` on `record` at `revision`: a module the host
/// selects, or a workload placed on the cluster. `None` when `record` has
/// no such unit there.
pub fn unit(
    revision: &Revision,
    record: &HostRecord,
    facts: &serde_json::Value,
    unit: &str,
) -> Option<UnitHooks> {
    use buckle_core::model::{RESUME_ACTION, SUSPEND_ACTION};
    match record.kind {
        TargetKind::Host => {
            let selected = crate::views::selected_modules(revision, None, Some((record, facts)));
            let module = revision
                .modules
                .iter()
                .find(|m| m.config.name == unit && selected.contains(&m.config.name))?;
            Some(UnitHooks {
                suspend: module.config.actions.contains_key(SUSPEND_ACTION),
                resume: module.config.actions.contains_key(RESUME_ACTION),
            })
        }
        TargetKind::Cluster | TargetKind::Workload => {
            let ctx = crate::fleet::record_context(record, facts, &revision.fleet);
            let workload = buckle_core::engine::selected_workloads(revision, &ctx)
                .into_iter()
                .find(|w| w.config.name == unit)?;
            Some(UnitHooks {
                // Its jobs are stopped as the implicit `stop` stops them.
                suspend: workload
                    .config
                    .implicit_actions()
                    .iter()
                    .any(|i| i.name == "stop"),
                resume: false,
            })
        }
    }
}

/// The units (target, name) of the fleet's active targets, each from the
/// revision it runs: the modules hosts select, or with `workload` the
/// workloads placed on clusters; of `target` alone when one is named, and
/// only those called `unit` when that is. `selector` reads a host as
/// itself and a cluster as the workload's placement, as [`offered`] does.
pub async fn units(
    store: &crate::store::Store,
    revisions: &crate::source::cache::RevisionCache,
    target: Option<&str>,
    selector: Option<&buckle_core::Filter>,
    workload: bool,
    unit: Option<&str>,
) -> buckle_core::Result<Vec<(String, String)>> {
    let targets = store
        .call(|db| {
            let mut out = Vec::new();
            for h in db.hosts().targets()? {
                let facts = db.hosts().latest_facts(&h.id)?.map(|(f, _)| f);
                out.push((h, facts.unwrap_or(serde_json::Value::Null)));
            }
            Ok(out)
        })
        .await?;
    let stable = crate::views::stable_revision(store, revisions).await;
    let mut out = Vec::new();
    for (record, facts) in &targets {
        if record.status != buckle_api::HostStatus::Active
            || record.executor_of().is_some()
            || (record.kind != TargetKind::Host) != workload
            || target.is_some_and(|t| t != record.id)
        {
            continue;
        }
        let Some(applied) = record.applied_revision.as_deref() else {
            continue;
        };
        let Ok(rev) = revisions.load(applied).await else {
            continue;
        };
        let seen_by = stable.as_deref().unwrap_or(&rev);
        let ctx = crate::fleet::record_context(record, facts, &seen_by.fleet);
        let names: Vec<String> = if workload {
            let own = crate::fleet::record_context(record, facts, &rev.fleet);
            buckle_core::engine::selected_workloads(&rev, &own)
                .into_iter()
                .map(|w| w.config.name.clone())
                .collect()
        } else {
            let selected = crate::views::selected_modules(&rev, None, Some((record, facts)));
            rev.modules
                .iter()
                .map(|m| m.config.name.clone())
                .filter(|name| selected.contains(name))
                .collect()
        };
        for name in names {
            if unit.is_some_and(|u| u != name)
                || selector.is_some_and(|s| !offers_to(s, record.kind, &ctx, seen_by, &rev, &name))
            {
                continue;
            }
            out.push((record.id.clone(), name));
        }
    }
    Ok(out)
}

/// An action the fleet offers, with the targets it is suspended on.
pub struct Offered {
    pub action: buckle_api::FleetAction,
    /// Targets that would offer it, were its module (or workload) not
    /// suspended there.
    pub suspended: Vec<String>,
}

/// [`offered`] without the suspended targets: what can run now.
pub async fn fleet(
    store: &crate::store::Store,
    revisions: &crate::source::cache::RevisionCache,
    selector: Option<&buckle_core::Filter>,
) -> buckle_core::Result<Vec<buckle_api::FleetAction>> {
    Ok(offered(store, revisions, selector)
        .await?
        .into_iter()
        .filter(|o| !o.action.targets.is_empty())
        .map(|o| o.action)
        .collect())
}

/// Every action the fleet's active targets offer now (each from the
/// revision it runs), with the targets offering it that `selector` matches:
/// hosts' module actions apart from clusters' workload actions, even where
/// a module and a workload share a name.
/// The selector sees a target as the stable revision's phases do: a host as
/// itself, a cluster's workload action as that workload's placement
/// (`workload.*` beside `cluster.*`), so `workload.labels.tier == "web"`
/// picks the clusters running such a workload, offering its actions only.
/// A target whose module (or workload) is suspended is set apart: actions
/// leave it alone until it is resumed.
pub async fn offered(
    store: &crate::store::Store,
    revisions: &crate::source::cache::RevisionCache,
    selector: Option<&buckle_core::Filter>,
) -> buckle_core::Result<Vec<Offered>> {
    let (targets, paused) = store
        .call(|db| {
            let mut out = Vec::new();
            for h in db.hosts().targets()? {
                let facts = db.hosts().latest_facts(&h.id)?.map(|(f, _)| f);
                out.push((h, facts.unwrap_or(serde_json::Value::Null)));
            }
            let paused: std::collections::HashSet<(String, String)> = db
                .paused()
                .list()?
                .into_iter()
                .map(|u| (u.target, u.unit))
                .collect();
            Ok((out, paused))
        })
        .await?;
    let stable = crate::views::stable_revision(store, revisions).await;
    let mut loaded: std::collections::HashMap<String, Option<std::sync::Arc<Revision>>> =
        std::collections::HashMap::new();
    let mut out: Vec<Offered> = Vec::new();
    for (record, facts) in &targets {
        // An in-cluster executor runs no modules of its own.
        if record.status != buckle_api::HostStatus::Active || record.executor_of().is_some() {
            continue;
        }
        let Some(applied) = record.applied_revision.clone() else {
            continue;
        };
        if !loaded.contains_key(&applied) {
            let rev = revisions.load(&applied).await.ok();
            loaded.insert(applied.clone(), rev);
        }
        let Some(rev) = loaded.get(&applied).cloned().flatten() else {
            continue;
        };
        let seen_by = stable.as_deref().unwrap_or(&rev);
        let ctx = crate::fleet::record_context(record, facts, &seen_by.fleet);
        let workload = record.kind != TargetKind::Host;
        for a in list(&rev, record, facts) {
            if let Some(selector) = selector
                && !offers_to(selector, record.kind, &ctx, seen_by, &rev, &a.module)
            {
                continue;
            }
            let suspended = paused.contains(&(record.id.clone(), a.module.clone()));
            let at = out.iter().position(|o| {
                let f = &o.action;
                f.workload == workload && f.action.module == a.module && f.action.name == a.name
            });
            let at = at.unwrap_or_else(|| {
                out.push(Offered {
                    action: buckle_api::FleetAction {
                        action: a,
                        workload,
                        targets: Vec::new(),
                    },
                    suspended: Vec::new(),
                });
                out.len() - 1
            });
            if suspended {
                out[at].suspended.push(record.id.clone());
            } else {
                out[at].action.targets.push(record.id.clone());
            }
        }
    }
    out.sort_by(|a, b| {
        let (a, b) = (&a.action, &b.action);
        (a.workload, &a.action.module, &a.action.name).cmp(&(
            b.workload,
            &b.action.module,
            &b.action.name,
        ))
    });
    Ok(out)
}

/// Whether `selector` matches the target offering an action of `module`: a
/// host as itself, a cluster as the placement of the workload `module`
/// (labelled as the revision it runs declares it), with the fleet's groups
/// from `seen_by`.
fn offers_to(
    selector: &buckle_core::Filter,
    kind: TargetKind,
    ctx: &buckle_core::selectors::HostContext,
    seen_by: &Revision,
    runs: &Revision,
    module: &str,
) -> bool {
    match (kind, runs.workload(module)) {
        (TargetKind::Cluster | TargetKind::Workload, Some(w)) => {
            buckle_core::engine::validate::placement_context(seen_by, ctx, &w.config)
                .matches(selector)
        }
        _ => ctx.matches(selector),
    }
}

/// Who started an action rollout, and how it follows its phases.
#[derive(Debug, Clone, Default)]
pub struct Origin {
    /// The schedule that started it; none for an operator.
    pub schedule: Option<String>,
    /// Its phases go ahead without the approvals they ask for.
    pub auto_approve: bool,
}

/// Starts an action rollout of `body`'s action over the targets offering it
/// that its selector matches, each running it from its applied revision;
/// returns the rollout's id. Shared by `POST /rollouts` and the scheduler.
pub async fn start_rollout(
    store: &crate::store::Store,
    revisions: &crate::source::cache::RevisionCache,
    controller: &crate::rollout::ControllerHandle,
    actor: &str,
    body: buckle_api::StartActionRollout,
    origin: Origin,
) -> Result<String, crate::rollout::CommandError> {
    use crate::rollout::CommandError;
    let raw = body
        .selector
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let selector = raw
        .as_deref()
        .map(buckle_core::Filter::new)
        .transpose()
        .map_err(|e| {
            CommandError::invalid(
                format!("The selector is not valid: {e}"),
                &["Check the selector's syntax."],
            )
        })?;
    let all = offered(store, revisions, selector.as_ref())
        .await
        .map_err(CommandError::internal)?;
    let found = all
        .into_iter()
        .find(|o| {
            let f = &o.action;
            f.workload == body.workload
                && f.action.module == body.module
                && f.action.name == body.action
        })
        .ok_or_else(|| {
            CommandError::not_found(format!(
                "No {}{} offers the action '{}' of '{}'.",
                if body.workload { "cluster" } else { "host" },
                if raw.is_some() {
                    " the selector matches"
                } else {
                    ""
                },
                body.action,
                body.module
            ))
        })?;
    if found.action.targets.is_empty() {
        // Nothing can run it now; a schedule tries again while its window
        // lasts, and says this when it gives up.
        return Err(CommandError::conflict(
            format!(
                "'{}' is suspended on every target offering its action '{}' ({}).",
                body.module,
                body.action,
                found.suspended.join(", ")
            ),
            &["Resume it where the action should run."],
        ));
    }
    if !found.suspended.is_empty() {
        tracing::info!(module = %body.module, action = %body.action, suspended = ?found.suspended, "the action leaves out the targets its module is suspended on");
    }
    let found = found.action;
    if found.action.confirm && body.note.trim().is_empty() {
        return Err(CommandError::invalid(
            format!(
                "The action '{}' asks for confirmation: send a note for the audit log.",
                found.action.label
            ),
            &["Describe why it runs; the note is kept in the audit log."],
        ));
    }
    let names = found.targets.clone();
    let targets: std::collections::BTreeMap<String, String> = store
        .call(move |db| {
            let mut out = std::collections::BTreeMap::new();
            for t in &names {
                if let Some(rev) = db.hosts().target(t)?.and_then(|r| r.applied_revision) {
                    out.insert(t.clone(), rev);
                }
            }
            Ok(out)
        })
        .await
        .map_err(CommandError::internal)?;
    controller
        .command(
            actor,
            crate::rollout::Command::StartAction {
                action: crate::rollout::RolloutAction {
                    module: found.action.module.clone(),
                    action: found.action.name.clone(),
                    label: found.action.label.clone(),
                    confirm: found.action.confirm,
                    selector: raw,
                    instances: body.instances.filter(|_| found.action.instances.is_some()),
                    schedule: origin.schedule,
                    auto_approve: origin.auto_approve,
                },
                targets,
                note: body.note,
            },
        )
        .await
}
