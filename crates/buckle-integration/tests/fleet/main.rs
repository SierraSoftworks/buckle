//! End-to-end scenarios: one real server, real agents, the production engine
//! over fake hosts, driven and observed through the HTTP API, SSE and the
//! agents' own state. One binary, so the expensive link happens once.

mod auto_trigger;
mod common;
mod drift;
mod durability;
mod enrolment;
mod failures;
mod gaps;
mod health_gating;
mod local_rollback;
mod phased;
mod reports;
mod secrets;
mod staleness;
