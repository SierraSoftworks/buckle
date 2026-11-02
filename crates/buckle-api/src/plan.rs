//! A plan as a reviewer reads it (design §11): the engine's redacted plan,
//! typed for the UI. The server builds it from `buckle_core::plan::Plan`;
//! this crate stays free of the engine, so the shape here is the contract.
//! Every text in it (summaries, diffs, previews, reasons) is already
//! redacted: secrets appear only as `«secret:NAME@fingerprint»` tokens.

use serde::{Deserialize, Serialize};

/// One host's plan, shared by every host with the same digest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanView {
    /// The revision planned.
    pub revision: String,
    /// The host this copy was planned on (one of the group's hosts).
    pub host: String,
    pub digest: String,
    /// Modules selected for the host, in dependency order.
    #[serde(default)]
    pub modules: Vec<ModuleView>,
    /// Resources in state that the revision no longer declares.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removals: Vec<RemovalView>,
    /// Records a desired resource took over instead of being removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rehomed: Vec<RehomeView>,
    /// Modules planned only after a sibling provides the facts they read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deferred: Vec<ModuleNote>,
    /// Drift plans: modules not applied at this revision, left alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub held: Vec<ModuleNote>,
    /// Modules whose selection or plan could not be determined; never
    /// applied, and their resources never treated as orphans.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indeterminate: Vec<ModuleNote>,
    /// Modules evaluated but not selected, with the selector that excluded them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<String>,
    /// Warnings worth showing (always-run commands, cautionary removals, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleView {
    pub module: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub needs: Vec<String>,
    /// Every task of the module, unchanged ones included.
    #[serde(default)]
    pub steps: Vec<StepView>,
    /// Handlers that run at the end of the module.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handlers: Vec<HandlerView>,
}

/// What a step's plan decided.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    /// The resource already matches.
    #[default]
    NoOp,
    Change,
    /// The task cannot run here; `summary` says why.
    Blocked,
}

/// The shape of a change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Create,
    Update,
    Delete,
    /// Nothing to diff, e.g. "run `nomad node eligibility -enable`".
    Opaque,
}

/// One task in a module plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepView {
    /// The task kind (`file.template`).
    pub kind: String,
    /// The resource key (`/etc/nomad.d/nomad.hcl`).
    pub key: String,
    /// The task's id within its module.
    #[serde(default)]
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub outcome: StepOutcome,
    /// The change's summary, or why the step is blocked; empty for no-ops.
    #[serde(default)]
    pub summary: String,
    /// For a change: its shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffKind>,
    /// For an update: a unified diff of redacted shadows (current vs
    /// planned), when the content is text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified: Option<String>,
    /// For a create: a redacted preview of the content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// The step deletes something from the host (a `delete` change: a
    /// removed file, an absent package, user or group).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub destructive: bool,
    /// Resource locks the executor serialises on (`apt`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<String>,
    /// Handlers notified when the step changes something.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notifies: Vec<String>,
    /// Runs on every apply (a command with no idempotency guard).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub always_runs: bool,
}

impl StepView {
    pub fn is_change(&self) -> bool {
        self.outcome == StepOutcome::Change
    }

    /// `module/kind[key]`-style display without the module.
    pub fn resource(&self) -> String {
        format!("{}[{}]", self.kind, self.key)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandlerView {
    pub handler: String,
    pub kind: String,
    pub summary: String,
}

/// A resource the revision no longer declares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalView {
    pub module: String,
    pub kind: String,
    pub key: String,
    /// What removing it does, or why nothing happens.
    pub summary: String,
    /// `true`: the kind's cleanup runs on the host (`removal: cleanup`);
    /// `false`: the record is only forgotten (`removal: leave`).
    pub cleanup: bool,
    /// The kind's removal can take unrelated things down (dependent
    /// packages, files owned by a user).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub caution: bool,
    /// The revision that introduced the resource, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub introduced_in: Option<String>,
}

impl RemovalView {
    /// `module/kind[key]`.
    pub fn resource(&self) -> String {
        format!("{}/{}[{}]", self.module, self.kind, self.key)
    }
}

/// A record kept because a desired resource took it over.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RehomeView {
    /// `module/kind[key]` of the old record.
    pub from: String,
    /// `module/kind[key]` of the resource that took it over.
    pub to: String,
    /// `moved` (same kind and key, another module) or `superseded`
    /// (another resource manages the same thing).
    pub how: String,
    pub summary: String,
}

/// A module the plan says something about without planning its steps:
/// deferred, held or indeterminate.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleNote {
    pub module: String,
    pub reason: String,
    /// Deferred modules: the facts they wait for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waiting_on: Vec<String>,
    /// Indeterminate modules: selected, but planning them failed (the apply
    /// reports the module failed; others still run).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub plan_error: bool,
}

impl PlanView {
    pub fn steps(&self) -> impl Iterator<Item = &StepView> {
        self.modules.iter().flat_map(|m| m.steps.iter())
    }

    /// Steps that change something.
    pub fn changes(&self) -> usize {
        self.steps().filter(|s| s.is_change()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_round_trips_and_leaves_empty_sections_off_the_wire() {
        let plan = PlanView {
            revision: "7c2e1f0".into(),
            host: "pi-1".into(),
            digest: "5b1c9e02".into(),
            modules: vec![ModuleView {
                module: "nomad".into(),
                needs: vec![],
                steps: vec![
                    StepView {
                        kind: "file.absent".into(),
                        key: "/etc/nomad.d/old.hcl".into(),
                        task: "old".into(),
                        outcome: StepOutcome::Change,
                        summary: "delete /etc/nomad.d/old.hcl".into(),
                        diff: Some(DiffKind::Delete),
                        destructive: true,
                        ..Default::default()
                    },
                    StepView {
                        kind: "systemd.unit".into(),
                        key: "nomad".into(),
                        ..Default::default()
                    },
                ],
                handlers: vec![],
            }],
            ..Default::default()
        };
        let text = serde_json::to_string(&plan).unwrap();
        for absent in [
            "removals",
            "held",
            "warnings",
            "unified",
            "needs",
            "always_runs",
        ] {
            assert!(!text.contains(absent), "{absent} in {text}");
        }
        assert!(text.contains(r#""diff":"delete""#));
        assert!(text.contains(r#""outcome":"no_op""#));
        assert_eq!(serde_json::from_str::<PlanView>(&text).unwrap(), plan);
        assert_eq!(plan.changes(), 1);
        assert_eq!(
            plan.modules[0].steps[0].resource(),
            "file.absent[/etc/nomad.d/old.hcl]"
        );
    }
}
