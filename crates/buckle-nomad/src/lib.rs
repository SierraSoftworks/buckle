//! Nomad executor primitives for Buckle cluster targets.
//!
//! An executor applies a cluster target's workloads through the Nomad API.
//! Each workload key (`job`, `volumes`, `variables`, `policies`,
//! `namespaces`, `node_pools`, `resources`) compiles to a [`primitives`]
//! step with the same contract as a task on a host: plan is pure, the
//! approval binds a digest of the diff, apply refuses when the live object
//! moved under the plan, and removal is explicit.
//!
//! The executor keeps no state for the cluster: ownership and digests are
//! [`stamps`] on the objects, the target's [`index`] variable lists the
//! entities that carry no metadata, and diffs come from Nomad's plan API.
//! Secret values reach Nomad only through
//! [`buckle_core::secrets::sinks::ApiSink`] and appear in plans as keyed
//! fingerprints.

pub mod actions;
pub mod client;
pub mod diff;
pub mod hcl;
pub mod health;
pub mod index;
pub mod primitives;
pub mod stamps;
pub mod transport;
pub mod workload;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod tests;

pub use client::Nomad;
pub use primitives::{Context, Kind, ObjectKey, Removal, Step};
pub use workload::{Workload, WorkloadPlan};
