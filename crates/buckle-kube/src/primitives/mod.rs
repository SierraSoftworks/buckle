//! The executor's primitives: a manifest object (applied with server-side
//! apply) and a Secret (whose values reach the API only through the
//! [`ApiSink`](buckle_core::secrets::sinks::ApiSink)). Each has a pure plan,
//! an apply that refuses when the live object moved under the plan, and a
//! removal. They are internal to the executor; a workload never lists them.

mod manifest;
mod secret;

pub use manifest::{ManifestSpec, namespace};
pub use secret::SecretSpec;

use crate::client::Kube;
use crate::index;
use crate::object::{ObjectKey, Owner};
use buckle_core::Result;
use buckle_core::plan::Planned;
use buckle_core::secrets::InstallKey;
use buckle_core::task::Applied;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// Reports progress while an apply runs (rollout state, for the ticker).
pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

/// Everything a primitive needs: the cluster, and who is applying what.
#[derive(Clone)]
pub struct Context {
    pub kube: Kube,
    /// The cluster target's id (`lab-k3s`).
    pub target: String,
    /// The workload being applied (`grafana`).
    pub module: String,
    pub revision: String,
    /// The namespace objects without one land in (the workload's
    /// `namespace`, else `default`).
    pub namespace: String,
    /// Keys the fingerprints of Secret values; the same for every executor
    /// of a target, so plan digests do not depend on the executor.
    pub fingerprints: Arc<InstallKey>,
    pub progress: Progress,
    /// How often a rollout is polled while an apply waits for it.
    pub poll: Duration,
    /// The longest an apply waits for a rollout to finish (the workload's
    /// `timeouts.rollout`).
    pub rollout_timeout: Duration,
    /// The longest a controller's pods may stay unschedulable during an
    /// apply (the workload's `timeouts.placement`).
    pub placement_timeout: Duration,
}

/// The default `timeouts.placement`, as on Nomad.
pub const PLACEMENT_TIMEOUT: Duration = Duration::from_secs(300);
/// The default `timeouts.rollout`, as on Nomad; a Deployment's own
/// `progressDeadlineSeconds` usually ends it first.
pub const ROLLOUT_TIMEOUT: Duration = Duration::from_secs(3600);

impl Context {
    pub fn new(
        kube: Kube,
        target: impl Into<String>,
        module: impl Into<String>,
        revision: impl Into<String>,
        fingerprints: Arc<InstallKey>,
    ) -> Self {
        Self {
            kube,
            target: target.into(),
            module: module.into(),
            revision: revision.into(),
            namespace: "default".into(),
            fingerprints,
            progress: Arc::new(|_| {}),
            poll: Duration::from_secs(2),
            rollout_timeout: ROLLOUT_TIMEOUT,
            placement_timeout: PLACEMENT_TIMEOUT,
        }
    }

    /// The workload's `timeouts`, each defaulting when absent.
    pub fn with_timeouts(mut self, placement: Option<Duration>, rollout: Option<Duration>) -> Self {
        self.placement_timeout = placement.unwrap_or(PLACEMENT_TIMEOUT);
        self.rollout_timeout = rollout.unwrap_or(ROLLOUT_TIMEOUT);
        self
    }

    pub fn with_namespace(mut self, namespace: Option<&str>) -> Self {
        if let Some(ns) = namespace.filter(|n| !n.is_empty()) {
            self.namespace = ns.to_owned();
        }
        self
    }

    pub fn with_progress(mut self, progress: Progress) -> Self {
        self.progress = progress;
        self
    }
}

/// The resource kinds used in plans.
pub const MANIFEST_KIND: &str = "kubernetes.manifest";
pub const SECRET_KIND: &str = "kubernetes.secret";

/// One primitive's plan: what changes, why to be careful, and (privately)
/// what the apply sends.
#[derive(Debug, Clone)]
pub struct Step {
    pub key: ObjectKey,
    pub planned: Planned,
    /// Shown with the plan: adoptions, plans made without a dry run.
    pub warnings: Vec<String>,
    pub(crate) pending: Pending,
}

impl Step {
    /// The approval digest of this step, over what operators see (Secret
    /// values appear as fingerprints only).
    pub fn digest(&self) -> buckle_core::Digest {
        buckle_core::Digest::of_json(&(&self.key, &self.planned, &self.warnings))
    }

    pub fn is_change(&self) -> bool {
        self.planned.is_change()
    }

    /// The resource kind shown in plans.
    pub fn task_kind(&self) -> &'static str {
        match self.pending {
            Pending::Secret(_) => SECRET_KIND,
            _ if self.key.kind == "Secret" && self.key.api_version == "v1" => SECRET_KIND,
            _ => MANIFEST_KIND,
        }
    }

    pub(crate) fn noop(key: ObjectKey) -> Self {
        Self {
            key,
            planned: Planned::NoOp,
            warnings: Vec::new(),
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
            Pending::Manifest(p) => manifest::apply(cx, &self.key, p).await,
            Pending::Secret(p) => secret::apply(cx, &self.key, p).await,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Pending {
    None,
    Manifest(manifest::Pending),
    Secret(secret::Pending),
}

/// One object, ready to plan.
#[derive(Debug, Clone)]
pub enum Primitive {
    Manifest(ManifestSpec),
    Secret(SecretSpec),
}

impl Primitive {
    /// Plans the primitive. Pure: reads the cluster, changes nothing.
    /// `created` lists the objects earlier steps create, so an object in a
    /// namespace (or of a CRD) created in the same apply plans as a creation
    /// rather than failing its dry run.
    pub async fn plan(&self, cx: &Context, created: &[ObjectKey]) -> Result<Step> {
        match self {
            Primitive::Manifest(s) => manifest::plan(cx, s, created).await,
            Primitive::Secret(s) => secret::plan(cx, s).await,
        }
    }
}

/// What to do with an object when its workload (or its manifest) goes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Removal {
    /// Delete what runs; keep data (namespaces, volume claims, Secrets,
    /// ConfigMaps, CRDs). The default.
    #[default]
    Stop,
    /// Delete everything the workload declared.
    Purge,
    /// Leave every object as it is.
    Keep,
}

/// An object indexed for a target that no workload of the current revision
/// declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub key: ObjectKey,
    pub module: String,
}

impl Orphan {
    /// What removing it under `removal` does, for the plan.
    pub fn describe(&self, removal: Removal) -> Option<String> {
        match removal {
            Removal::Keep => None,
            Removal::Stop if self.key.holds_data() => None,
            _ => Some(format!("delete {}", self.key)),
        }
    }

    pub fn caution(&self, removal: Removal) -> bool {
        self.describe(removal).is_some() && (removal == Removal::Purge || self.key.holds_data())
    }

    /// Removes the object under `removal`. A kept data object leaves the
    /// index (it keeps its labels, so a workload declaring it again adopts it).
    pub async fn remove(&self, cx: &Context, removal: Removal) -> Result<Applied> {
        if self.describe(removal).is_none() {
            if removal != Removal::Keep {
                index::update(&cx.kube, &cx.target, |e| index::forget(e, &self.key)).await?;
            }
            return Ok(Applied::unchanged(format!("kept {}", self.key)));
        }
        let deleted = match cx
            .kube
            .resource(&self.key.api_version, &self.key.kind)
            .await?
        {
            Some(r) => {
                cx.kube
                    .delete(&r.path(&self.key.namespace, Some(&self.key.name)))
                    .await?
            }
            None => false,
        };
        index::update(&cx.kube, &cx.target, |e| index::forget(e, &self.key)).await?;
        Ok(if deleted {
            Applied::changed(format!("deleted {}", self.key))
        } else {
            Applied::unchanged(format!("{} was already gone", self.key))
        })
    }
}

/// The objects indexed for `cx.target` that are not in `declared` and still
/// carry its label: what a removed workload or manifest left behind, in
/// removal order (namespaced objects first, namespaces and CRDs last).
pub async fn orphans(cx: &Context, declared: &[ObjectKey]) -> Result<Vec<Orphan>> {
    let mut found = Vec::new();
    for e in index::read(&cx.kube, &cx.target).await?.entries {
        if declared.contains(&e.key) {
            continue;
        }
        let Some(resource) = cx.kube.resource(&e.key.api_version, &e.key.kind).await? else {
            continue;
        };
        let live = cx
            .kube
            .get(&resource.path(&e.key.namespace, Some(&e.key.name)))
            .await?;
        if matches!(Owner::of(live.as_ref(), &cx.target), Owner::Us(_)) {
            found.push(Orphan {
                key: e.key,
                module: e.workload,
            });
        }
    }
    found.sort_by_key(|o| (removal_rank(&o.key), o.key.clone()));
    Ok(found)
}

/// Every object indexed for `cx.target`, by workload: what health checks.
pub async fn owned(cx: &Context) -> Result<Vec<index::Entry>> {
    Ok(index::read(&cx.kube, &cx.target).await?.entries)
}

fn removal_rank(key: &ObjectKey) -> u8 {
    match key.kind.as_str() {
        "CustomResourceDefinition" => 3,
        "Namespace" => 2,
        _ if key.namespace.is_empty() => 1,
        _ => 0,
    }
}

/// A short digest over JSON, for comparisons that must not depend on order.
pub(crate) fn yaml(value: &serde_json::Value) -> String {
    serde_yaml::to_string(value).unwrap_or_default()
}
