//! The executor's primitives: one per workload key, each with a pure plan,
//! an apply that refuses when the live object moved under the plan, and a
//! removal. They are internal to the executor; a workload never lists them.

mod entity;
pub(crate) mod job;
mod policy;
mod variable;
mod volume;

pub use entity::{EntityKind, EntitySpec};
pub use job::{JobSpec, follow_deployment};
pub use policy::{Attach, PolicySpec};
pub use variable::VariableSpec;
pub use volume::VolumeSpec;

use crate::client::Nomad;
use buckle_core::Result;
use buckle_core::plan::Planned;
use buckle_core::secrets::InstallKey;
use buckle_core::task::Applied;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Reports progress while an apply runs (deployment health, for the ticker).
pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

/// Everything a primitive needs: the cluster, and who is applying what.
#[derive(Clone)]
pub struct Context {
    pub nomad: Nomad,
    /// The client for ACL policies, which Nomad serves only to management
    /// tokens; `None` uses `nomad`.
    pub acl: Option<Nomad>,
    /// The cluster target's id (`home-nomad`).
    pub target: String,
    /// The workload being applied (`paste`).
    pub module: String,
    /// The revision being applied.
    pub revision: String,
    /// Keys the fingerprints of variable values. It must be the same for
    /// every executor of a target, so the plan's digest does not depend on
    /// which executor made it.
    pub fingerprints: Arc<InstallKey>,
    pub progress: Progress,
}

impl Context {
    pub fn new(
        nomad: Nomad,
        target: impl Into<String>,
        module: impl Into<String>,
        revision: impl Into<String>,
        fingerprints: Arc<InstallKey>,
    ) -> Self {
        Self {
            nomad,
            acl: None,
            target: target.into(),
            module: module.into(),
            revision: revision.into(),
            fingerprints,
            progress: Arc::new(|_| {}),
        }
    }

    pub fn with_progress(mut self, progress: Progress) -> Self {
        self.progress = progress;
        self
    }

    /// Uses `acl` for ACL policies.
    pub fn with_acl(mut self, acl: Option<Nomad>) -> Self {
        self.acl = acl;
        self
    }

    /// The client ACL policies are read and written with.
    pub fn acl(&self) -> &Nomad {
        self.acl.as_ref().unwrap_or(&self.nomad)
    }
}

/// The kinds of object a workload manages, in the order they apply:
/// entities before the job that uses them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Resource,
    Namespace,
    NodePool,
    Volume,
    Variable,
    Policy,
    Job,
}

impl Kind {
    /// The resource kind used in plans (`nomad.job`).
    pub fn task_kind(self) -> &'static str {
        match self {
            Kind::Resource => "nomad.resource",
            Kind::Namespace => "nomad.namespace",
            Kind::NodePool => "nomad.node_pool",
            Kind::Volume => "nomad.volume",
            Kind::Variable => "nomad.variable",
            Kind::Policy => "nomad.policy",
            Kind::Job => "nomad.job",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Resource => "resource",
            Kind::Namespace => "namespace",
            Kind::NodePool => "node pool",
            Kind::Volume => "volume",
            Kind::Variable => "variable",
            Kind::Policy => "policy",
            Kind::Job => "job",
        }
    }

    /// Whether removal can take data or other workloads down.
    pub fn removal_caution(self) -> bool {
        matches!(self, Kind::Namespace | Kind::Volume | Kind::NodePool)
    }
}

/// An object's identity in the cluster.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectKey {
    pub kind: Kind,
    /// Empty for cluster-wide objects (namespaces, node pools, policies).
    #[serde(default)]
    pub namespace: String,
    pub name: String,
}

impl ObjectKey {
    pub fn new(kind: Kind, namespace: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            kind,
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    /// The resource key used in plans: `default/paste`, or `team-a` for
    /// cluster-wide objects.
    pub fn key(&self) -> String {
        if self.namespace.is_empty() {
            self.name.clone()
        } else {
            format!("{}/{}", self.namespace, self.name)
        }
    }
}

impl std::fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.kind.label(), self.key())
    }
}

/// What to do with an object when its workload (or the key) goes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Removal {
    /// Stop jobs; keep data (volumes, variables, namespaces). The default.
    #[default]
    Stop,
    /// Stop jobs and delete everything the workload declared.
    Purge,
    /// Leave every object as it is.
    Keep,
}

/// One primitive's plan: what changes, why to be careful, and (privately)
/// what the apply sends.
#[derive(Debug, Clone)]
pub struct Step {
    pub key: ObjectKey,
    pub planned: Planned,
    /// Shown with the plan: placement failures, adoptions, scheduler warnings.
    pub warnings: Vec<String>,
    /// The scheduler preview of a job plan, one line per group.
    pub preview: Vec<String>,
    pub(crate) pending: Pending,
}

impl Step {
    /// The approval digest of this step: over what operators see, never
    /// over a value (variable values appear as fingerprints only).
    pub fn digest(&self) -> buckle_core::Digest {
        buckle_core::Digest::of_json(&(&self.key, &self.planned, &self.warnings, &self.preview))
    }

    pub fn is_change(&self) -> bool {
        self.planned.is_change()
    }

    pub(crate) fn noop(key: ObjectKey) -> Self {
        Self {
            key,
            planned: Planned::NoOp,
            warnings: Vec::new(),
            preview: Vec::new(),
            pending: Pending::None,
        }
    }

    pub(crate) fn blocked(key: ObjectKey, reason: impl Into<String>) -> Self {
        Self {
            planned: Planned::blocked(reason),
            ..Self::noop(key)
        }
    }

    /// Applies the step. A step that planned no change does nothing.
    pub async fn apply(&self, cx: &Context) -> Result<Applied> {
        match &self.pending {
            Pending::None => Ok(Applied::unchanged(format!("{} unchanged", self.key))),
            Pending::Job(p) => job::apply(cx, &self.key, p).await,
            Pending::Variable(p) => variable::apply(cx, &self.key, p).await,
            Pending::Entity(p) => entity::apply(cx, &self.key, p).await,
            Pending::Volume(p) => volume::apply(cx, &self.key, p).await,
            Pending::Policy(p) => policy::apply(cx, &self.key, p).await,
        }
    }
}

/// What an apply sends, fixed at plan time.
#[derive(Debug, Clone)]
pub(crate) enum Pending {
    None,
    Job(job::Pending),
    Variable(variable::Pending),
    Entity(entity::Pending),
    Volume(volume::Pending),
    Policy(policy::Pending),
}

/// One workload key, ready to plan.
#[derive(Debug, Clone)]
pub enum Primitive {
    Job(JobSpec),
    Volume(VolumeSpec),
    Variable(VariableSpec),
    Policy(PolicySpec),
    Entity(EntitySpec),
}

impl Primitive {
    pub fn kind(&self) -> Kind {
        match self {
            Primitive::Job(_) => Kind::Job,
            Primitive::Volume(_) => Kind::Volume,
            Primitive::Variable(_) => Kind::Variable,
            Primitive::Policy(_) => Kind::Policy,
            Primitive::Entity(e) => e.kind.kind(),
        }
    }

    /// Plans the primitive. Pure: reads the cluster, changes nothing.
    /// `created` lists the objects earlier steps of the same workload will
    /// create, so a job in a namespace created in the same apply plans as a
    /// registration rather than failing against the absent namespace.
    pub async fn plan(&self, cx: &Context, created: &[ObjectKey]) -> Result<Step> {
        match self {
            Primitive::Job(s) => job::plan(cx, s, created).await,
            Primitive::Volume(s) => volume::plan(cx, s).await,
            Primitive::Variable(s) => variable::plan(cx, s).await,
            Primitive::Policy(s) => policy::plan(cx, s).await,
            Primitive::Entity(s) => entity::plan(cx, s).await,
        }
    }
}

/// An object stamped by (or indexed for) a target that no workload of the
/// current revision declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub key: ObjectKey,
    pub module: String,
    /// Volume id, for volumes.
    pub id: Option<String>,
}

impl Orphan {
    /// What removing it under `removal` does, for the plan.
    pub fn describe(&self, removal: Removal) -> Option<String> {
        let what = &self.key;
        match (removal, self.key.kind) {
            (Removal::Keep, _) => None,
            (_, Kind::Job) if removal == Removal::Purge => Some(format!("stop and purge {what}")),
            (_, Kind::Job) => Some(format!("stop {what}")),
            (Removal::Stop, Kind::Volume | Kind::Variable | Kind::Namespace) => None,
            (_, Kind::Volume) => Some(format!("delete {what} if no allocation uses it")),
            _ => Some(format!("delete {what}")),
        }
    }

    /// Whether removing it under `removal` deserves a caution in the plan.
    pub fn caution(&self, removal: Removal) -> bool {
        self.describe(removal).is_some()
            && (removal == Removal::Purge || self.key.kind.removal_caution())
    }

    /// Removes the object under `removal`.
    pub async fn remove(&self, cx: &Context, removal: Removal) -> Result<Applied> {
        if self.describe(removal).is_none() {
            if matches!(self.key.kind, Kind::Volume | Kind::Variable) && removal != Removal::Keep {
                crate::index::update(&cx.nomad, &cx.target, |e| {
                    crate::index::forget(e, self.key.kind, &self.key.namespace, &self.key.name)
                })
                .await?;
            }
            return Ok(Applied::unchanged(format!("kept {}", self.key)));
        }
        match self.key.kind {
            Kind::Job => job::remove(cx, &self.key, removal == Removal::Purge).await,
            Kind::Variable => variable::remove(cx, &self.key).await,
            Kind::Volume => volume::remove(cx, &self.key, self.id.as_deref()).await,
            Kind::Policy => policy::remove(cx, &self.key).await,
            Kind::Namespace | Kind::NodePool | Kind::Resource => {
                entity::remove(cx, &self.key).await
            }
        }
    }
}

/// Lists the objects stamped by (or indexed for) `cx.target` that are not
/// in `declared`: what a removed workload or key left behind.
pub async fn orphans(cx: &Context, declared: &[ObjectKey]) -> Result<Vec<Orphan>> {
    let mut found = Vec::new();
    found.extend(job::owned(cx).await?);
    found.extend(policy::owned(cx).await?);
    found.extend(entity::owned(cx).await?);
    for e in crate::index::read(&cx.nomad, &cx.target).await?.entries {
        found.push(Orphan {
            key: ObjectKey::new(e.kind, e.namespace, e.name),
            module: e.module,
            id: e.id,
        });
    }
    found.retain(|o| !declared.contains(&o.key));
    // Jobs are found by their meta and listed in the index: keep one of each.
    let mut seen = std::collections::HashSet::new();
    found.retain(|o| seen.insert(o.key.clone()));
    found.sort_by(|a, b| b.key.kind.cmp(&a.key.kind).then(a.key.cmp(&b.key)));
    Ok(found)
}

/// The namespace of a namespaced spec, defaulting like Nomad.
pub(crate) fn namespace_or_default(ns: Option<&str>) -> String {
    match ns {
        Some(ns) if !ns.is_empty() => ns.to_owned(),
        _ => "default".to_owned(),
    }
}
