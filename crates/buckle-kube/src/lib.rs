//! Kubernetes executor primitives for Buckle cluster targets.
//!
//! An executor applies a cluster target's workloads through the Kubernetes
//! API with the same contract as a task on a host: plan is pure, the
//! approval binds a digest of the diff, apply refuses when the live object
//! moved under the plan, and removal is explicit.
//!
//! * Every manifest object is applied with server-side apply under the
//!   `buckle` field manager; its plan is a dry-run apply, diffed over the
//!   fields Buckle owns ([`fields`]), so defaults and other controllers'
//!   fields never show as changes.
//! * The executor keeps no state for the cluster: objects carry ownership
//!   labels ([`object`]) and the target's [`index`] ConfigMap lists what was
//!   applied, so removed workloads are found without listing every type.
//! * Secret values reach the API only through
//!   [`buckle_core::secrets::sinks::ApiSink`] and appear in plans as keyed
//!   fingerprints.
//! * Health is implicit ([`health`]): rollouts, pods and `Ready` conditions;
//!   [`actions`] offers rollout restart and undo.

pub mod actions;
pub mod client;
pub mod config;
pub mod fields;
pub mod health;
pub mod index;
pub mod object;
pub mod primitives;
pub mod transport;
pub mod workload;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;

pub use client::Kube;
pub use config::Endpoint;
pub use object::ObjectKey;
pub use primitives::{Context, Orphan, Primitive, Removal, Step};
pub use workload::{Workload, WorkloadPlan};
