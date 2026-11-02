//! End-to-end scenarios: one real server, real agents, the production engine
//! over fake hosts, driven and observed through the HTTP API, SSE and the
//! agents' own state. One binary, so the expensive link happens once.

mod auto_trigger;
mod common;
mod enrolment;
mod failures;
mod health_gating;
mod phased;
