//! Joining new hosts (`/api/v1/join`): the one-line command an admin pastes
//! on a new host to install Buckle, optionally join the tailnet, and enrol
//! the agent.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The operating system family a join command is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinPlatform {
    Linux,
    Macos,
    Windows,
}

impl JoinPlatform {
    pub const ALL: [JoinPlatform; 3] = [Self::Linux, Self::Macos, Self::Windows];

    pub fn label(self) -> &'static str {
        match self {
            Self::Linux => "Linux",
            Self::Macos => "macOS",
            Self::Windows => "Windows",
        }
    }
}

/// How the new agent proves who it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JoinMethod {
    /// Over the tailnet: the server identifies the node by whois. With
    /// Tailscale provisioning configured, the command also joins the host to
    /// the tailnet with a single-use auth key.
    Tailscale,
    /// With a single-use join token, exchanged for a per-host token on the
    /// agent's first connection.
    Token,
}

/// `GET /api/v1/join`: what the Add an agent wizard can offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinOptions {
    /// The methods this server accepts, preferred first.
    pub methods: Vec<JoinMethod>,
    /// The gRPC URL agents are told to dial.
    pub server_url: String,
    /// Tailscale provisioning, when configured: the command joins the host
    /// to the tailnet too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale: Option<TailscaleJoinOptions>,
    /// How long a join command stays usable, in seconds.
    pub ttl_seconds: u64,
    /// New hosts wait for approval (`agents.registration: approve`), except
    /// those enrolled by a join command, which an admin created.
    pub approval_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailscaleJoinOptions {
    /// ACL tags the joined node may carry; the first is the default.
    pub tags: Vec<String>,
    /// Each command needs the admin's consent in Tailscale (an OAuth app);
    /// otherwise the server mints a tagged key itself (an OAuth client).
    #[serde(default)]
    pub consent: bool,
}

/// `POST /api/v1/join`: a join command for one new host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    pub platform: JoinPlatform,
    pub method: JoinMethod,
    /// The host's name (its tailnet node name over Tailscale). Absent: the
    /// host's own hostname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// Seed labels for the agent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// With Tailscale provisioning: the ACL tags the node advertises (a
    /// subset of [`TailscaleJoinOptions::tags`]; empty takes the first).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tailscale_tags: Vec<String>,
}

/// A join command in the making, or made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinTicket {
    pub id: String,
    pub platform: JoinPlatform,
    pub method: JoinMethod,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    /// After this the command's keys no longer work.
    pub expires_at: DateTime<Utc>,
    #[serde(flatten)]
    pub state: JoinState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JoinState {
    /// Tailscale must consent to issuing the node's auth key: send the
    /// browser to `authorize_url`, which returns to the hosts page.
    Authorize { authorize_url: String },
    /// Paste `command` on the new host. It carries single-use keys, so it
    /// is shown only to the admin who created it.
    Ready { command: String },
    /// Tailscale refused, or the exchange failed.
    Failed { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_carry_their_state_inline() {
        let ticket = JoinTicket {
            id: "j1".into(),
            platform: JoinPlatform::Linux,
            method: JoinMethod::Tailscale,
            host_id: None,
            labels: BTreeMap::new(),
            created_by: "ada@example.com".into(),
            created_at: DateTime::UNIX_EPOCH,
            expires_at: DateTime::UNIX_EPOCH,
            state: JoinState::Ready {
                command: "curl …".into(),
            },
        };
        let v = serde_json::to_value(&ticket).unwrap();
        assert_eq!(v["state"], "ready");
        assert_eq!(v["command"], "curl …");
        assert_eq!(serde_json::from_value::<JoinTicket>(v).unwrap(), ticket);
    }
}
