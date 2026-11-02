use serde::{Deserialize, Serialize};

/// `GET /api/v1/me` → 200 with this, or 204 when OIDC is not configured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub subject: String,
    pub is_admin: bool,
}

/// `GET /api/v1/csrf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CsrfToken {
    pub token: String,
}
