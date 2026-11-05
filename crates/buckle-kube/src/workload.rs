//! A workload compiled for a Kubernetes cluster: its objects in the order
//! the workload lists them (each manifest document in file order), planned
//! together and applied in that order (and removed in reverse).

use crate::object::ObjectKey;
use crate::primitives::{Context, Primitive, Removal, Step};
use buckle_core::Result;
use buckle_core::plan::{Diff, Planned};
use buckle_core::task::Applied;

/// The objects of one workload, with files read and values rendered, in
/// apply order.
#[derive(Debug, Clone, Default)]
pub struct Workload {
    pub name: String,
    pub primitives: Vec<Primitive>,
    pub removal: Removal,
}

impl Workload {
    pub fn new(name: impl Into<String>, primitives: Vec<Primitive>) -> Self {
        Self {
            name: name.into(),
            primitives,
            removal: Removal::default(),
        }
    }

    /// Plans every object. Pure: nothing in the cluster changes.
    pub async fn plan(&self, cx: &Context) -> Result<WorkloadPlan> {
        self.plan_after(cx, &[]).await
    }

    /// [`Workload::plan`] after workloads that apply first and create
    /// `earlier` (a namespace, a CRD): those plan as present-to-be.
    pub async fn plan_after(&self, cx: &Context, earlier: &[ObjectKey]) -> Result<WorkloadPlan> {
        let mut steps: Vec<Step> = Vec::new();
        let mut created: Vec<ObjectKey> = earlier.to_vec();
        for p in &self.primitives {
            let step = p.plan(cx, &created).await?;
            if steps.iter().any(|s| s.key == step.key) {
                return Err(buckle_core::error::user(
                    format!("The workload {} declares {} twice.", self.name, step.key),
                    &["Each object may appear once per workload, across all of its files."],
                ));
            }
            if matches!(
                step.planned,
                Planned::Change {
                    diff: Diff::Create { .. }
                }
            ) {
                created.push(step.key.clone());
            }
            steps.push(step);
        }
        Ok(WorkloadPlan { steps })
    }
}

/// A workload's plan: one step per object, in apply order.
#[derive(Debug, Clone)]
pub struct WorkloadPlan {
    pub steps: Vec<Step>,
}

impl WorkloadPlan {
    pub fn changes(&self) -> usize {
        self.steps.iter().filter(|s| s.is_change()).count()
    }

    /// The objects the apply creates.
    pub fn created(&self) -> Vec<ObjectKey> {
        self.steps
            .iter()
            .filter(|s| {
                matches!(
                    s.planned,
                    Planned::Change {
                        diff: Diff::Create { .. }
                    }
                )
            })
            .map(|s| s.key.clone())
            .collect()
    }

    /// The objects this workload declares, for orphan detection.
    pub fn keys(&self) -> Vec<ObjectKey> {
        self.steps.iter().map(|s| s.key.clone()).collect()
    }

    /// The first blocked step's reason, if any step cannot run.
    pub fn blocked(&self) -> Option<(&ObjectKey, &str)> {
        self.steps.iter().find_map(|s| match &s.planned {
            Planned::Blocked { reason } => Some((&s.key, reason.as_str())),
            _ => None,
        })
    }

    /// Applies the steps in order, stopping at the first failure. A
    /// controller's step lasts until its rollout finishes.
    pub async fn apply(&self, cx: &Context) -> Result<Vec<(ObjectKey, Applied)>> {
        if let Some((key, reason)) = self.blocked() {
            return Err(buckle_core::error::user(
                format!("{key} cannot be applied: {reason}"),
                &["Resolve the conflict and plan again."],
            ));
        }
        let mut out = Vec::new();
        for step in self.steps.iter().filter(|s| s.is_change()) {
            out.push((step.key.clone(), step.apply(cx).await?));
        }
        Ok(out)
    }
}
