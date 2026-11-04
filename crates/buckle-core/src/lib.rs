//! Buckle core: the configuration model, the planner and executor, and the
//! traits that tasks, fact providers and health checks implement.
//!
//! The crate is organised around a few contracts that the rest of Buckle
//! builds on:
//!
//! * [`model`] — the on-disk configuration (`buckle.yaml`, `module.yaml`).
//! * [`task::Task`], [`facts::FactProvider`], [`health::HealthCheck`] — the
//!   pluggable kinds, registered with [`register_task!`], [`register_facts!`]
//!   and [`register_health!`].
//! * [`system::System`] — everything a task may do to the host, behind a trait
//!   so that tests run against [`testing::FakeSystem`].
//! * [`secrets`] — the sealed [`secrets::Secret`] type and the sinks that are
//!   the only way to read one.
//! * [`plan`] — the redacted, serialisable description of what an apply will do.
//! * [`state`] — what the agent remembers about resources it applied.
//! * [`engine`] — plan and apply over all of the above.

#[macro_use]
pub mod macros;

pub mod digest;
pub mod engine;
pub mod error;
pub mod facts;
pub mod health;
pub mod model;
pub mod plan;
pub mod platform;
pub mod reboot;
pub mod secrets;
pub mod selectors;
pub mod state;
pub mod system;
pub mod task;
pub mod template;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[doc(hidden)]
pub mod __private {
    pub use inventory;
}

pub use digest::Digest;
pub use error::{Error, Result};
pub use filt_rs::{Filter, FilterValue, Filterable};
pub use platform::{Platform, Platforms};
