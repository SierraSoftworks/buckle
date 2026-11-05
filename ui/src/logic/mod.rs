//! The parts of the UI that are decisions rather than markup: how a pipeline
//! is laid out, which colour a state is, how a live event changes what a page
//! shows, what a plan asks an approver to accept. None of it touches the DOM,
//! so all of it is tested with a plain `cargo test` on the host target.

pub mod actions;
pub mod blueprint;
pub mod clusters;
pub mod controls;
pub mod diff;
pub mod error_text;
pub mod filter;
pub mod fleet;
pub mod fleet_plan;
pub mod format;
pub mod health;
pub mod join;
pub mod labels;
pub mod live;
pub mod pipeline;
pub mod plan;
pub mod run;
pub mod ticker;
pub mod tone;
