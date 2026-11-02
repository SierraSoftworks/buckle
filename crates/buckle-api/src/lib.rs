//! Data transfer types for Buckle's HTTP API (`/api/v1`), shared by the server
//! and the WebAssembly UI. Deliberately free of web-framework, database and
//! engine dependencies so that it compiles for both targets.

pub mod audit;
pub mod auth;
pub mod catalog;
pub mod fleet;
pub mod plan;
pub mod revision;
pub mod rollout;

pub use audit::*;
pub use auth::*;
pub use catalog::*;
pub use fleet::*;
pub use plan::*;
pub use revision::*;
pub use rollout::*;

use serde::{Deserialize, Serialize};

/// Every error response has this body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub advice: Vec<String>,
}

/// A page of results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    pub total: u64,
}
