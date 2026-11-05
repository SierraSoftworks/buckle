//! The rollout controller (design §10–11): a persisted state machine that
//! moves one revision through ordered phases in waves, gated by approvals,
//! baseline health, bake windows and pre-phase checks, with a virtual
//! stragglers stage after promotion.

pub mod bake;
mod controller;
pub mod link;
pub mod model;
pub mod stages;
pub mod view;

pub use controller::{
    Command, CommandError, CommandErrorKind, Connected, ControllerDeps, ControllerHandle,
    ControllerOptions, approval_token, encode_pending_report, pending_report_key, spawn,
};
pub use link::{HostLink, Sessions};
pub use model::RolloutAction;

#[cfg(test)]
mod tests;
