//! Buckle's standard library: task kinds, fact providers and health checks.
//!
//! Each kind lives in its own file and registers itself with
//! `buckle_core::register_task!`, `register_facts!` or `register_health!`.
//! Lanes 5 and 6 own `tasks/`, `facts/` and `health/`.

pub mod facts;
pub mod health;
pub mod tasks;

/// Referenced by the binary so the linker keeps this crate (and its
/// `inventory` registrations) even though nothing calls into it directly.
pub fn ensure_linked() {}
