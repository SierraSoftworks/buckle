//! Operator actions a target offers, from the revision it runs: a host's
//! selected modules' `actions:`, a cluster's selected workloads' implicit
//! actions and their own. The controller runs one ([`crate::rollout::Command::RunAction`]).

use crate::store::HostRecord;
use buckle_api::{ActionInfo, TargetKind};
use buckle_core::engine::Revision;

/// The actions `record` offers at `revision`, in module order.
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
                    m.config.actions.iter().map(|(name, decl)| ActionInfo {
                        module: m.config.name.clone(),
                        name: name.clone(),
                        label: decl.label(name).to_owned(),
                        kind: decl.task.kind.clone(),
                        description: decl.description.clone(),
                        confirm: decl.confirm,
                        implicit: false,
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
                    });
                    implicit.chain(extra).collect::<Vec<_>>()
                })
                .collect()
        }
    }
}

/// Every action the fleet's active targets offer now (each from the
/// revision it runs), with the targets offering it that `selector` matches:
/// hosts' module actions apart from clusters' workload actions, even where
/// a module and a workload share a name.
/// The selector sees a target as the stable revision's phases do: a host as
/// itself, a cluster's workload action as that workload's placement
/// (`workload.*` beside `cluster.*`), so `workload.labels.tier == "web"`
/// picks the clusters running such a workload, offering its actions only.
pub async fn fleet(
    store: &crate::store::Store,
    revisions: &crate::source::cache::RevisionCache,
    selector: Option<&buckle_core::Filter>,
) -> buckle_core::Result<Vec<buckle_api::FleetAction>> {
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
    let mut loaded: std::collections::HashMap<String, Option<std::sync::Arc<Revision>>> =
        std::collections::HashMap::new();
    let mut out: Vec<buckle_api::FleetAction> = Vec::new();
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
            match out.iter_mut().find(|f| {
                f.workload == workload && f.action.module == a.module && f.action.name == a.name
            }) {
                Some(f) => f.targets.push(record.id.clone()),
                None => out.push(buckle_api::FleetAction {
                    action: a,
                    workload,
                    targets: vec![record.id.clone()],
                }),
            }
        }
    }
    out.sort_by(|a, b| {
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
