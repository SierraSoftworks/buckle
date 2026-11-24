//! Whether a host's run disrupts the work a cluster schedules on it: what a
//! rollout reads, with a cluster's `maintenance.drain`, to decide whether
//! the host's node is drained before the run applies.
//!
//! A run disrupts the node when it reboots the host or changes a module
//! marked `disrupts: node`. What the plan cannot know yet counts as
//! disruptive: a reboot handler a changing task notifies may still decide
//! at apply time not to reboot (its `only_if`), and a module planned only
//! at apply time may change.

use super::Revision;
use crate::model::{Disrupts, ModuleConfig, TaskDecl};
use crate::plan::{ModulePlan, Plan};

/// The task kind that reboots the host.
pub const REBOOT_KIND: &str = "system.reboot";

/// Why `plan` disrupts the host's node, one line each; empty when it does
/// not. `revision` is the plan's: it says which modules are marked
/// `disrupts: node` and what a module planned at apply time may do.
pub fn plan_disruptions(plan: &Plan, revision: &Revision) -> Vec<String> {
    let mut out = Vec::new();
    let marked = |module: &str| revision.module(module).is_some_and(|m| disrupts(&m.config));
    for m in &plan.modules {
        module_disruptions(m, marked(&m.module), &mut out);
    }
    for d in &plan.deferred {
        match &d.preview {
            Some(preview) => module_disruptions(preview, marked(&d.module), &mut out),
            None => {
                let Some(config) = revision.module(&d.module).map(|m| &m.config) else {
                    continue;
                };
                if disrupts(config) {
                    push(
                        &mut out,
                        format!(
                            "{} disrupts the node and is planned only when it applies",
                            d.module
                        ),
                    );
                } else if may_reboot(config) {
                    push(
                        &mut out,
                        format!(
                            "{} may reboot the host and is planned only when it applies",
                            d.module
                        ),
                    );
                }
            }
        }
    }
    for r in &plan.removals {
        if marked(&r.resource.module) {
            push(
                &mut out,
                format!(
                    "{} disrupts the node and removes {}",
                    r.resource.module, r.resource.key
                ),
            );
        }
    }
    out
}

/// Why running the action `action` of `module` disrupts the host's node:
/// its task reboots, it notifies a reboot handler, or its module is marked
/// `disrupts: node`. An action is always a change.
pub fn action_disruptions(revision: &Revision, module: &str, action: &str) -> Vec<String> {
    let mut out = Vec::new();
    let Some(loaded) = revision.module(module) else {
        return out;
    };
    let config = &loaded.config;
    if disrupts(config) {
        push(&mut out, format!("{module} disrupts the node"));
    }
    let Some(decl) = config.actions.get(action) else {
        return out;
    };
    if decl.task.kind == REBOOT_KIND {
        push(&mut out, format!("{module}/{action} reboots the host"));
    }
    for (owner, handler) in reboot_handlers(revision, module, &decl.task) {
        push(
            &mut out,
            format!("{module}/{action} notifies the reboot handler {owner}:{handler}"),
        );
    }
    out
}

fn module_disruptions(m: &ModulePlan, marked: bool, out: &mut Vec<String>) {
    let changes = m.steps.iter().any(|s| s.planned.is_change());
    if marked && changes {
        push(out, format!("{} changes and disrupts the node", m.module));
    }
    for step in m.steps.iter().filter(|s| s.planned.is_change()) {
        if step.task.kind == REBOOT_KIND {
            push(out, format!("{} reboots the host", m.module));
        }
    }
    for h in m.handlers.iter().filter(|h| h.kind == REBOOT_KIND) {
        push(
            out,
            format!(
                "a change in {} notifies the reboot handler {}:{}",
                m.module, h.module, h.handler
            ),
        );
    }
}

fn disrupts(config: &ModuleConfig) -> bool {
    config.disrupts == Some(Disrupts::Node)
}

/// Whether a module could reboot the host: a task or handler of its own
/// is a `system.reboot`.
fn may_reboot(config: &ModuleConfig) -> bool {
    config
        .tasks
        .iter()
        .chain(config.handlers.values())
        .any(|t| t.kind == REBOOT_KIND)
}

/// The reboot handlers (`(owner, handler)`) `task` of `module` notifies.
fn reboot_handlers(revision: &Revision, module: &str, task: &TaskDecl) -> Vec<(String, String)> {
    task.notify
        .iter()
        .map(|n| super::planner::qualify_handler(module, n))
        .filter(|(owner, handler)| {
            revision
                .module(owner)
                .and_then(|m| m.config.handlers.get(handler))
                .is_some_and(|h| h.kind == REBOOT_KIND)
        })
        .collect()
}

fn push(out: &mut Vec<String>, line: String) {
    if !out.contains(&line) {
        out.push(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{
        Deferred, Diff, HandlerRun, ModulePlan, Planned, Removal, ResourceKey, RevisionId, Step,
        TaskRef,
    };
    use rstest::rstest;

    fn revision(files: &[(&str, &str)]) -> Revision {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("buckle.yaml"), "version: 2\n").unwrap();
        for (path, content) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        crate::engine::loader::load_dir(dir.path(), RevisionId::new("r1")).unwrap()
    }

    const MODULES: &[(&str, &str)] = &[
        (
            "modules/k3s/module.yaml",
            "name: k3s\ndisrupts: node\ntasks:\n  - file.line: { path: /etc/k3s, line: x }\n",
        ),
        (
            "modules/os/module.yaml",
            "name: os\ntasks:\n  - apt.package: { name: [curl] }\n    notify: [reboot]\nhandlers:\n  reboot: { system.reboot: { only_if: test -f /run/reboot-required } }\nactions:\n  upgrade:\n    apt.upgrade: {}\n    notify: [reboot]\n  status: { command.run: { run: uptime } }\n  restart: { system.reboot: {} }\n",
        ),
        (
            "modules/web/module.yaml",
            "name: web\ntasks:\n  - file.line: { path: /etc/web, line: x }\n",
        ),
    ];

    fn step(module: &str, kind: &str, change: bool) -> Step {
        Step {
            task: TaskRef {
                module: module.into(),
                id: kind.into(),
                kind: kind.into(),
                name: None,
            },
            resource: ResourceKey {
                module: module.into(),
                kind: kind.into(),
                key: kind.into(),
            },
            planned: if change {
                Planned::change(Diff::Opaque {
                    summary: "x".into(),
                })
            } else {
                Planned::NoOp
            },
            locks: vec![],
            notifies: vec![],
            always_runs: false,
            removal: Default::default(),
            facts: None,
            display: None,
        }
    }

    fn module(name: &str, steps: Vec<Step>, handlers: Vec<HandlerRun>) -> ModulePlan {
        ModulePlan {
            module: name.into(),
            needs: vec![],
            steps,
            handlers,
            provides_facts: vec![],
        }
    }

    fn reboot_handler() -> HandlerRun {
        HandlerRun {
            module: "os".into(),
            handler: "reboot".into(),
            kind: REBOOT_KIND.into(),
            summary: "reboot (no change)".into(),
        }
    }

    fn plan(modules: Vec<ModulePlan>) -> Plan {
        Plan::new(RevisionId("r1".into()), "pi", modules, vec![], vec![])
    }

    #[rstest]
    #[case::nothing_changes(plan(vec![module("k3s", vec![step("k3s", "file.line", false)], vec![])]), &[])]
    #[case::an_ordinary_change(plan(vec![module("web", vec![step("web", "file.line", true)], vec![])]), &[])]
    #[case::a_module_that_disrupts_the_node(
        plan(vec![module("k3s", vec![step("k3s", "file.line", true)], vec![])]),
        &["k3s changes and disrupts the node"]
    )]
    #[case::a_reboot_task(
        plan(vec![module("web", vec![step("web", REBOOT_KIND, true)], vec![])]),
        &["web reboots the host"]
    )]
    #[case::a_reboot_task_that_plans_nothing(
        plan(vec![module("web", vec![step("web", REBOOT_KIND, false)], vec![])]),
        &[]
    )]
    // Notified by a change: it may still decide not to reboot when it runs,
    // which the plan cannot know.
    #[case::a_notified_reboot_handler(
        plan(vec![module("os", vec![step("os", "apt.package", true)], vec![reboot_handler()])]),
        &["a change in os notifies the reboot handler os:reboot"]
    )]
    fn a_plan_disrupts_the_node_when_it_reboots_or_changes_a_marked_module(
        #[case] plan: Plan,
        #[case] expected: &[&str],
    ) {
        let rev = revision(MODULES);
        assert_eq!(plan_disruptions(&plan, &rev), expected);
    }

    #[test]
    fn what_is_planned_only_at_apply_time_counts_by_what_it_may_do() {
        let rev = revision(MODULES);
        let deferred = |module: &str, preview: Option<ModulePlan>| Deferred {
            module: module.into(),
            waiting_on: vec!["web".into()],
            reason: "waits for web's facts".into(),
            preview,
        };
        let p = Plan::new(
            RevisionId("r1".into()),
            "pi",
            vec![],
            vec![],
            vec![
                deferred("k3s", None),
                deferred("os", None),
                deferred("web", None),
            ],
        );
        assert_eq!(
            plan_disruptions(&p, &rev),
            [
                "k3s disrupts the node and is planned only when it applies",
                "os may reboot the host and is planned only when it applies",
            ]
        );
        let previewed = Plan::new(
            RevisionId("r1".into()),
            "pi",
            vec![],
            vec![],
            vec![deferred(
                "k3s",
                Some(module("k3s", vec![step("k3s", "file.line", false)], vec![])),
            )],
        );
        assert!(plan_disruptions(&previewed, &rev).is_empty());
    }

    #[test]
    fn removing_what_a_marked_module_owned_disrupts_the_node() {
        let rev = revision(MODULES);
        let p = Plan::new(
            RevisionId("r1".into()),
            "pi",
            vec![],
            vec![Removal {
                resource: ResourceKey {
                    module: "k3s".into(),
                    kind: "file.line".into(),
                    key: "/etc/k3s".into(),
                },
                policy: Default::default(),
                summary: "remove the line".into(),
                caution: false,
                introduced_in: None,
            }],
            vec![],
        );
        assert_eq!(
            plan_disruptions(&p, &rev),
            ["k3s disrupts the node and removes /etc/k3s"]
        );
    }

    #[rstest]
    #[case::notifies_a_reboot("os", "upgrade", &["os/upgrade notifies the reboot handler os:reboot"])]
    #[case::reboots("os", "restart", &["os/restart reboots the host"])]
    #[case::harmless("os", "status", &[])]
    #[case::unknown_module("nope", "x", &[])]
    fn an_action_disrupts_the_node_by_its_declaration(
        #[case] module: &str,
        #[case] action: &str,
        #[case] expected: &[&str],
    ) {
        let rev = revision(MODULES);
        assert_eq!(action_disruptions(&rev, module, action), expected);
    }

    #[rstest]
    #[case::never(crate::model::DrainPolicy::Never, true, true, false)]
    #[case::disruptive(crate::model::DrainPolicy::Disruptive, true, true, true)]
    #[case::disruptive_harmless(crate::model::DrainPolicy::Disruptive, true, false, false)]
    #[case::always(crate::model::DrainPolicy::Always, true, false, true)]
    #[case::always_unchanged(crate::model::DrainPolicy::Always, false, false, false)]
    #[case::disruptive_unchanged(crate::model::DrainPolicy::Disruptive, false, true, false)]
    fn a_policy_drains_only_runs_that_change_something(
        #[case] policy: crate::model::DrainPolicy,
        #[case] changes: bool,
        #[case] disruptive: bool,
        #[case] drains: bool,
    ) {
        let reasons: Vec<String> = disruptive
            .then(|| "reboots".to_owned())
            .into_iter()
            .collect();
        assert_eq!(policy.drains(changes, &reasons), drains);
    }
}
