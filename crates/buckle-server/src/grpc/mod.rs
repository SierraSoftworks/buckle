//! The gRPC front agents talk to (design §08): one bidirectional session per
//! agent plus content fetches.

pub mod auth;
pub mod guard;
mod service;
pub mod tls;

pub use auth::{LocalApiWhois, Whois, WhoisIdentity};
pub use guard::{LiveSessions, SessionGuard};
pub use service::{FleetService, serve};

#[cfg(test)]
mod tests;
