//! The rollout controller (design §10–11): a persisted state machine that
//! moves one revision through ordered phases in waves, gated by approvals,
//! baseline health, bake windows and pre-phase checks, with a virtual
//! stragglers stage after promotion.

mod controller;
pub mod link;
pub mod model;
pub mod view;

pub use controller::{
    Command, CommandError, CommandErrorKind, Connected, ControllerDeps, ControllerHandle,
    ControllerOptions, approval_token, spawn,
};
pub use link::{HostLink, Sessions};

#[cfg(test)]
mod tests;
