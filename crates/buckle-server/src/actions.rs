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
        TargetKind::Cluster => {
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
