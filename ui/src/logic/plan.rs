//! Reading a plan for review (design §11).
//!
//! `PlanGroup.plan` is the redacted `buckle_core::plan::Plan` as JSON. The UI
//! mirrors only what it shows, leniently: every field defaults and unknown
//! variants are tolerated, so a newer server's plan still renders.

use std::collections::BTreeSet;

use buckle_api::{PlanGroup, RolloutHost};
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct PlanDoc {
    pub revision: String,
    pub host: String,
    pub digest: String,
    pub modules: Vec<ModuleDoc>,
    pub removals: Vec<RemovalDoc>,
    pub deferred: Vec<DeferredDoc>,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ModuleDoc {
    pub module: String,
    pub needs: Vec<String>,
    pub steps: Vec<StepDoc>,
    pub handlers: Vec<HandlerDoc>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct StepDoc {
    pub task: TaskDoc,
    pub resource: ResourceDoc,
    pub planned: Planned,
    pub locks: Vec<String>,
    pub notifies: Vec<String>,
    pub always_runs: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct TaskDoc {
    pub id: String,
    pub kind: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(default)]
pub struct ResourceDoc {
    pub module: String,
    pub kind: String,
    pub key: String,
}

impl std::fmt::Display for ResourceDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}[{}]", self.module, self.kind, self.key)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Planned {
    #[default]
    NoOp,
    Change {
        diff: Diff,
    },
    Blocked {
        #[serde(default)]
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Diff {
    Create {
        #[serde(default)]
        summary: String,
        #[serde(default)]
        preview: Option<String>,
    },
    Update {
        #[serde(default)]
        summary: String,
        #[serde(default)]
        unified: Option<String>,
    },
    Delete {
        #[serde(default)]
        summary: String,
    },
    Opaque {
        #[serde(default)]
        summary: String,
    },
    #[serde(other)]
    Unknown,
}

impl Diff {
    pub fn summary(&self) -> &str {
        match self {
            Diff::Create { summary, .. }
            | Diff::Update { summary, .. }
            | Diff::Delete { summary }
            | Diff::Opaque { summary } => summary,
            Diff::Unknown => "",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct HandlerDoc {
    pub module: String,
    pub handler: String,
    pub kind: String,
    pub summary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RemovalDoc {
    pub resource: ResourceDoc,
    /// `cleanup` (run the kind's remove) or `leave` (forget only).
    pub policy: String,
    pub summary: String,
    /// The kind's removal can take unrelated things down (packages, users).
    pub caution: bool,
    pub introduced_in: Option<String>,
}

impl RemovalDoc {
    /// Whether the removal touches the host (as opposed to forgetting).
    pub fn runs(&self) -> bool {
        !self.policy.eq_ignore_ascii_case("leave")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DeferredDoc {
    pub module: String,
    pub waiting_on: Vec<String>,
    pub reason: String,
}

impl PlanDoc {
    /// Reads a plan document; `None` when the server sent nothing readable.
    pub fn read(value: &serde_json::Value) -> Option<PlanDoc> {
        if value.is_null() {
            return None;
        }
        serde_json::from_value(value.clone()).ok()
    }
}

/// Kinds whose removal or `absent` state the design asks to be flagged
/// (design §11: removals, `file.absent`, `apt.package` absent and
/// `user.account` absent).
const ABSENT_KINDS: &[&str] = &["apt.package", "user.account", "user.group", "docker.image"];

/// Whether a step deletes something from the host.
pub fn destructive_step(step: &StepDoc) -> bool {
    let Planned::Change { diff } = &step.planned else {
        return false;
    };
    if matches!(diff, Diff::Delete { .. }) || step.task.kind == "file.absent" {
        return true;
    }
    let summary = diff.summary().to_ascii_lowercase();
    ABSENT_KINDS.contains(&step.task.kind.as_str())
        && ["remove", "purge", "delete", "absent"]
            .iter()
            .any(|w| summary.starts_with(w) || summary.contains(&format!(" {w}")))
}

/// What one distinct plan asks an approver to accept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Review {
    pub changes: usize,
    pub unchanged: usize,
    pub blocked: usize,
    pub destructive_steps: usize,
    /// Removals that will run on the host (`cleanup`).
    pub removals: usize,
    /// Removals that only forget the resource (`leave`).
    pub forgotten: usize,
    /// Removals of kinds that can take unrelated things down.
    pub cautions: usize,
    pub deferred: usize,
    pub always_runs: usize,
}

impl Review {
    /// Destructive steps plus removals that run: the number the approval
    /// confirmation names.
    pub fn destructive(&self) -> usize {
        self.destructive_steps + self.removals
    }
}

pub fn review(plan: &PlanDoc) -> Review {
    let mut r = Review::default();
    for step in plan.modules.iter().flat_map(|m| &m.steps) {
        match &step.planned {
            Planned::Change { .. } => r.changes += 1,
            Planned::Blocked { .. } => r.blocked += 1,
            Planned::NoOp | Planned::Unknown => r.unchanged += 1,
        }
        r.destructive_steps += usize::from(destructive_step(step));
        r.always_runs += usize::from(step.always_runs && step.planned != Planned::NoOp);
    }
    for removal in &plan.removals {
        if removal.runs() {
            r.removals += 1;
        } else {
            r.forgotten += 1;
        }
        r.cautions += usize::from(removal.caution && removal.runs());
    }
    r.deferred = plan.deferred.len();
    r
}

/// What approving a phase binds: the distinct plans its hosts would apply.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalScope {
    /// Indices into the rollout's plan groups, in their order.
    pub groups: Vec<usize>,
    pub digests: Vec<String>,
    /// Hosts in the phase whose plan is one of `digests`.
    pub hosts: Vec<String>,
    /// Hosts in the phase with no plan yet (offline, still planning).
    pub without_plan: Vec<String>,
    pub destructive: usize,
    pub removals: usize,
    pub changes: usize,
}

/// Works out what an approval of a phase covers. A host belongs to a plan
/// group by its own `plan_digest` first, or by being listed in the group.
pub fn approval_scope(phase_hosts: &[&RolloutHost], plans: &[PlanGroup]) -> ApprovalScope {
    let mut scope = ApprovalScope::default();
    let mut used = BTreeSet::new();
    for host in phase_hosts {
        let group = plans.iter().position(|g| {
            host.plan_digest.as_deref() == Some(g.digest.as_str())
                || (host.plan_digest.is_none() && g.hosts.contains(&host.host))
        });
        match group {
            Some(i) => {
                used.insert(i);
                scope.hosts.push(host.host.clone());
            }
            None => scope.without_plan.push(host.host.clone()),
        }
    }
    for i in used {
        let g = &plans[i];
        scope.groups.push(i);
        scope.digests.push(g.digest.clone());
        match PlanDoc::read(&g.plan) {
            Some(doc) => {
                let r = review(&doc);
                scope.destructive += r.destructive();
                scope.removals += r.removals;
                scope.changes += r.changes;
            }
            None => {
                scope.removals += g.removals as usize;
                scope.changes += g.changes as usize;
                scope.destructive += if g.destructive {
                    (g.removals as usize).max(1)
                } else {
                    0
                };
            }
        }
    }
    scope.hosts.sort();
    scope.without_plan.sort();
    scope
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::HostRolloutState;
    use chrono::Utc;
    use serde_json::json;

    fn sample() -> serde_json::Value {
        json!({
            "revision": "7c2e1f0", "host": "pi-1", "digest": "5b1c9e02",
            "modules": [{
                "module": "nomad", "needs": ["docker"],
                "steps": [
                    {"task": {"module": "nomad", "id": "pkg", "kind": "apt.package"},
                     "resource": {"module": "nomad", "kind": "apt.package", "key": "nomad"},
                     "planned": {"outcome": "change", "diff": {"kind": "update", "summary": "1.9.2 → 1.9.3"}},
                     "locks": ["apt"]},
                    {"task": {"module": "nomad", "id": "conf", "kind": "file.template"},
                     "resource": {"module": "nomad", "kind": "file.template", "key": "/etc/nomad.d/nomad.hcl"},
                     "planned": {"outcome": "change", "diff": {"kind": "update", "summary": "2 lines changed",
                       "unified": "-servers = [\"«secret:nomad_server@3f9a»\"]\n+servers = [\"«secret:nomad_server@b21c»\"]\n"}}},
                    {"task": {"module": "nomad", "id": "old", "kind": "file.absent"},
                     "resource": {"module": "nomad", "kind": "file.absent", "key": "/etc/nomad.d/old.hcl"},
                     "planned": {"outcome": "change", "diff": {"kind": "delete", "summary": "delete /etc/nomad.d/old.hcl"}}},
                    {"task": {"module": "nomad", "id": "unit", "kind": "systemd.unit"},
                     "resource": {"module": "nomad", "kind": "systemd.unit", "key": "nomad"},
                     "planned": {"outcome": "no_op"}},
                    {"task": {"module": "nomad", "id": "x", "kind": "future.kind"},
                     "resource": {"module": "nomad", "kind": "future.kind", "key": "x"},
                     "planned": {"outcome": "something_new"}}
                ]
            }],
            "removals": [
                {"resource": {"module": "grey", "kind": "systemd.unit", "key": "grey"}, "policy": "cleanup",
                 "summary": "disable + stop", "caution": false},
                {"resource": {"module": "grey", "kind": "apt.package", "key": "grey"}, "policy": "cleanup",
                 "summary": "remove package", "caution": true},
                {"resource": {"module": "grey", "kind": "file.copy", "key": "/opt/x"}, "policy": "leave",
                 "summary": "forget only"}
            ],
            "deferred": [{"module": "otel", "waiting_on": ["docker.version"], "reason": "waits for facts"}],
            "future_field": true
        })
    }

    #[test]
    fn a_plan_is_read_leniently_and_reviewed() {
        let doc = PlanDoc::read(&sample()).expect("readable");
        assert_eq!(doc.modules[0].steps.len(), 5);
        assert_eq!(doc.modules[0].steps[4].planned, Planned::Unknown);
        let r = review(&doc);
        assert_eq!(r.changes, 3);
        assert_eq!(r.unchanged, 2);
        assert_eq!(r.destructive_steps, 1);
        assert_eq!((r.removals, r.forgotten, r.cautions), (2, 1, 1));
        assert_eq!(r.deferred, 1);
        assert_eq!(r.destructive(), 3);
        assert_eq!(PlanDoc::read(&serde_json::Value::Null), None);
    }

    #[test]
    fn absent_states_of_risky_kinds_count_as_destructive() {
        let step = |kind: &str, summary: &str| StepDoc {
            task: TaskDoc {
                kind: kind.into(),
                ..TaskDoc::default()
            },
            planned: Planned::Change {
                diff: Diff::Update {
                    summary: summary.into(),
                    unified: None,
                },
            },
            ..StepDoc::default()
        };
        assert!(destructive_step(&step("apt.package", "remove nomad")));
        assert!(destructive_step(&step("user.account", "account absent")));
        assert!(!destructive_step(&step("apt.package", "1.9.2 → 1.9.3")));
        assert!(!destructive_step(&step("file.line", "remove line")));
        assert!(destructive_step(&step("file.absent", "anything")));
    }

    #[test]
    fn an_approval_scope_names_the_hosts_digests_and_destructive_steps() {
        let plans = vec![
            PlanGroup {
                digest: "5b1c9e02".into(),
                hosts: vec!["oci-1".into(), "oci-2".into()],
                changes: 3,
                removals: 2,
                destructive: true,
                plan: sample(),
            },
            PlanGroup {
                digest: "e07aa341".into(),
                hosts: vec!["oci-3".into()],
                changes: 1,
                removals: 0,
                destructive: false,
                plan: serde_json::Value::Null,
            },
            PlanGroup {
                digest: "ffff0000".into(),
                hosts: vec!["elsewhere".into()],
                changes: 9,
                removals: 9,
                destructive: true,
                plan: serde_json::Value::Null,
            },
        ];
        let host = |name: &str, digest: Option<&str>| RolloutHost {
            host: name.into(),
            phase: "cloud".into(),
            wave: None,
            state: HostRolloutState::Planned,
            plan_digest: digest.map(Into::into),
            plan_changes: None,
            plan_removals: None,
            failed_module: None,
            failed_checks: vec![],
            waived_checks: vec![],
            error: None,
            updated_at: Utc::now(),
            bake_until: None,
            needs_approval: true,
        };
        let hosts = [
            host("oci-2", Some("5b1c9e02")),
            host("oci-1", None),
            host("oci-3", Some("e07aa341")),
            host("az-1", None),
        ];
        let refs: Vec<&RolloutHost> = hosts.iter().collect();
        let scope = approval_scope(&refs, &plans);
        assert_eq!(scope.groups, vec![0, 1]);
        assert_eq!(scope.digests, vec!["5b1c9e02", "e07aa341"]);
        assert_eq!(scope.hosts, vec!["oci-1", "oci-2", "oci-3"]);
        assert_eq!(scope.without_plan, vec!["az-1"]);
        // Counted once per distinct plan, not once per host.
        assert_eq!(scope.destructive, 3);
        assert_eq!(scope.removals, 2);
        assert_eq!(scope.changes, 4);
    }
}
