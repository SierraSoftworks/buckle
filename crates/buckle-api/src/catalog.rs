use serde::{Deserialize, Serialize};

/// `GET /api/v1/catalog`: the registered kinds with their JSON schemas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub tasks: Vec<KindDescriptor>,
    pub facts: Vec<KindDescriptor>,
    pub health: Vec<KindDescriptor>,
    pub server_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KindDescriptor {
    pub kind: String,
    pub description: String,
    pub platforms: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal_default: Option<String>,
    #[serde(default)]
    pub removal_caution: bool,
    pub schema: serde_json::Value,
}
