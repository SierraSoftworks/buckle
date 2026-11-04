//! `/etc/buckle/agent.yaml`.

use buckle_core::error::HumanizableResult;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Server URL (`https://buckle.example.ts.net:7700`). Mutually exclusive with `source`.
    #[serde(default)]
    pub server: Option<String>,
    /// A local configuration directory to watch instead of a server.
    #[serde(default)]
    pub source: Option<PathBuf>,
    /// Overrides the Tailscale node name / hostname as the host identity.
    #[serde(default)]
    pub host_id: Option<String>,
    /// Seed labels; server-managed labels override these.
    #[serde(default)]
    pub labels: IndexMap<String, String>,
    #[serde(default = "AgentConfig::default_state_dir")]
    pub state_dir: PathBuf,
    /// Bootstrap token for first enrolment (the per-host token lives in state_dir).
    #[serde(default)]
    pub bootstrap_token: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub drift: DriftConfig,
    /// Host-specific variable overrides (the rare case).
    #[serde(default)]
    pub vars: IndexMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// For an executor running inside a cluster: the workload identity it
    /// authenticates with (`agents.auth: workload_identity` on the server).
    #[serde(default)]
    pub workload_identity: Option<WorkloadIdentityConfig>,
    /// In local mode (`source`), programs the `exec` secret provider may run
    /// (e.g. `op`). Never read from agent.yaml: only an explicit command-line
    /// flag (`buckle agent --allow-secret-exec op`) enables it, as on the
    /// server (design §06, "exec exists only behind an explicit allowlist").
    #[serde(skip)]
    pub local_secret_exec: Vec<String>,
}

impl AgentConfig {
    fn default_state_dir() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\ProgramData\buckle")
        } else {
            PathBuf::from("/var/lib/buckle")
        }
    }

    pub fn load(path: &Path) -> buckle_core::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!(
                    "Could not read the agent configuration at {}.",
                    path.display()
                ),
                &["Run `buckle up` to create it, or pass --config."],
            )
        })?;
        let config: AgentConfig = serde_yaml::from_str(&text).to_human_error()?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> buckle_core::Result<()> {
        match (&self.server, &self.source) {
            (Some(_), Some(_)) => Err(buckle_core::error::user(
                "The agent configuration sets both `server` and `source`; choose one.",
                &[
                    "Use `server` to connect to a Buckle server, or `source` to watch a local directory.",
                ],
            )),
            (None, None) => Err(buckle_core::error::user(
                "The agent configuration must set either `server` or `source`.",
                &[
                    "Use `server` to connect to a Buckle server, or `source` to watch a local directory.",
                ],
            )),
            (Some(_), None) if self.workload_identity.is_some() && self.host_id.is_none() => {
                Err(buckle_core::error::user(
                    "The agent configuration sets `workload_identity` without `host_id`.",
                    &[
                        "Give the in-cluster executor a stable `host_id` (such as `home-nomad-executor`): its container's hostname changes with every allocation.",
                    ],
                ))
            }
            _ => Ok(()),
        }
    }

    /// Whether this agent runs against a local directory.
    pub fn is_local(&self) -> bool {
        self.source.is_some()
    }
}

/// Where an in-cluster executor finds its workload identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadIdentityConfig {
    /// The token file the scheduler keeps current (Nomad:
    /// `secrets/nomad_buckle.jwt` with `identity { file = true }`;
    /// Kubernetes: a projected ServiceAccount token).
    pub token_file: PathBuf,
    /// The cluster's key set as the executor reaches it, relayed to a server
    /// that cannot reach the cluster itself so it can follow key rotation.
    #[serde(default)]
    pub jwks_url: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// `sha256:…` fingerprint of the server certificate pinned at install time.
    #[serde(default)]
    pub ca_fingerprint: Option<String>,
    /// Path to a CA bundle to trust instead of the system roots.
    #[serde(default)]
    pub ca_file: Option<PathBuf>,
    /// Allow plaintext (only sensible over a tailnet).
    #[serde(default)]
    pub insecure_plaintext: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriftConfig {
    #[serde(default = "DriftConfig::default_interval", with = "humantime_serde")]
    pub interval: Duration,
    /// Apply corrections automatically; false reports only.
    #[serde(default = "DriftConfig::default_correct")]
    pub correct: bool,
}

impl DriftConfig {
    fn default_interval() -> Duration {
        Duration::from_secs(15 * 60)
    }
    fn default_correct() -> bool {
        true
    }
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            interval: Self::default_interval(),
            correct: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    #[serde(default)]
    pub otlp_headers: IndexMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let yaml = r#"
server: https://buckle.example.ts.net:7700
labels: { role: worker, site: home }
state_dir: /var/lib/buckle
drift: { interval: 10m, correct: false }
tls: { ca_fingerprint: "sha256:abc" }
"#;
        let cfg: AgentConfig = serde_yaml::from_str(yaml).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.drift.interval, Duration::from_secs(600));
        assert!(!cfg.drift.correct);
        assert!(!cfg.is_local());
        assert_eq!(cfg.labels["site"], "home");
    }

    #[test]
    fn requires_exactly_one_of_server_or_source() {
        let both: AgentConfig = serde_yaml::from_str("server: x\nsource: /y\n").unwrap();
        assert!(both.validate().is_err());
        let neither: AgentConfig = serde_yaml::from_str("labels: {}\n").unwrap();
        assert!(neither.validate().is_err());
        let local: AgentConfig = serde_yaml::from_str("source: /srv/config\n").unwrap();
        local.validate().unwrap();
        assert!(local.is_local());
    }

    #[test]
    fn an_in_cluster_executor_needs_a_stable_host_id() {
        let wi = "server: x\nworkload_identity: { token_file: /secrets/nomad_buckle.jwt }\n";
        let unnamed: AgentConfig = serde_yaml::from_str(wi).unwrap();
        assert!(unnamed.validate().is_err());
        let named: AgentConfig =
            serde_yaml::from_str(&format!("{wi}host_id: home-nomad-executor\n")).unwrap();
        named.validate().unwrap();
    }
}
