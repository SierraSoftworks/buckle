//! A workload compiled for a Nomad cluster: its keys as primitives in the
//! implicit order (resources, namespaces, node pools, volumes, variables,
//! policies, then the job), planned together and applied in that order.

use crate::primitives::{
    Context, EntityKind, EntitySpec, JobSpec, Kind, ObjectKey, PolicySpec, Primitive, Removal,
    Step, VariableSpec, VolumeSpec,
};
use buckle_core::Result;
use buckle_core::task::Applied;

/// The Nomad keys of one workload, with files read and values rendered.
#[derive(Debug, Clone, Default)]
pub struct Workload {
    pub name: String,
    pub job: Option<JobSpec>,
    pub volumes: Vec<VolumeSpec>,
    pub variables: Vec<VariableSpec>,
    pub policies: Vec<PolicySpec>,
    pub namespaces: Vec<EntitySpec>,
    pub node_pools: Vec<EntitySpec>,
    pub resources: Vec<EntitySpec>,
    pub removal: Removal,
}

impl Workload {
    /// The primitives in apply order.
    pub fn primitives(&self) -> Vec<Primitive> {
        let mut out: Vec<Primitive> = Vec::new();
        out.extend(self.resources.iter().cloned().map(Primitive::Entity));
        out.extend(self.namespaces.iter().cloned().map(|mut e| {
            e.kind = EntityKind::Namespace;
            Primitive::Entity(e)
        }));
        out.extend(self.node_pools.iter().cloned().map(|mut e| {
            e.kind = EntityKind::NodePool;
            Primitive::Entity(e)
        }));
        out.extend(self.volumes.iter().cloned().map(Primitive::Volume));
        out.extend(self.variables.iter().cloned().map(Primitive::Variable));
        out.extend(self.policies.iter().cloned().map(Primitive::Policy));
        out.extend(self.job.iter().cloned().map(Primitive::Job));
        out.sort_by_key(Primitive::kind);
        out
    }

    /// Plans every key. Pure: nothing in the cluster changes.
    pub async fn plan(&self, cx: &Context) -> Result<WorkloadPlan> {
        self.plan_after(cx, &[]).await
    }

    /// [`Workload::plan`] after workloads that apply first and create
    /// `earlier` (a namespace this workload's job lands in, say): those
    /// objects plan as present-to-be rather than missing.
    pub async fn plan_after(&self, cx: &Context, earlier: &[ObjectKey]) -> Result<WorkloadPlan> {
        let mut steps = Vec::new();
        let mut created: Vec<ObjectKey> = earlier.to_vec();
        for p in self.primitives() {
            let step = p.plan(cx, &created).await?;
            if matches!(
                step.planned,
                buckle_core::plan::Planned::Change {
                    diff: buckle_core::plan::Diff::Create { .. }
                }
            ) {
                created.push(step.key.clone());
            }
            steps.push(step);
        }
        let mut seen = std::collections::HashSet::new();
        for s in &steps {
            if !seen.insert(&s.key) {
                return Err(buckle_core::error::user(
                    format!("The workload {} declares {} twice.", self.name, s.key),
                    &[
                        "Each job, volume, variable path, policy and entity may appear once per workload.",
                    ],
                ));
            }
        }
        Ok(WorkloadPlan { steps })
    }
}

/// A workload's plan: one step per key, in apply order.
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
                    buckle_core::plan::Planned::Change {
                        diff: buckle_core::plan::Diff::Create { .. }
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
            buckle_core::plan::Planned::Blocked { reason } => Some((&s.key, reason.as_str())),
            _ => None,
        })
    }

    /// Applies the steps in order, stopping at the first failure. A job's
    /// step lasts until its deployment finishes.
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

/// The job of a workload, when it has one, for health and actions.
pub fn job_key(plan: &WorkloadPlan) -> Option<&ObjectKey> {
    plan.steps
        .iter()
        .map(|s| &s.key)
        .find(|k| k.kind == Kind::Job)
}
