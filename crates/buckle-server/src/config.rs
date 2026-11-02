//! `server.yaml`. String values may reference environment variables with
//! `${{ env.NAME }}`; an unset variable is a start-up error.

use buckle_core::error::HumanizableResult;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: ListenConfig,
    #[serde(default = "ServerConfig::default_data_dir")]
    pub data_dir: PathBuf,
    pub source: SourceConfig,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub web: WebConfig,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
}

impl ServerConfig {
    fn default_data_dir() -> PathBuf {
        PathBuf::from("/data")
    }

    /// Loads, interpolates `${{ env.NAME }}` in string values, and validates.
    pub fn load(path: &Path) -> buckle_core::Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!(
                    "Could not read the server configuration at {}.",
                    path.display()
                ),
                &["Pass --config with the path to server.yaml."],
            )
        })?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> buckle_core::Result<Self> {
        let mut value: serde_yaml::Value = serde_yaml::from_str(text).to_human_error()?;
        interpolate_env(&mut value, &|name| std::env::var(name).ok())?;
        let config: ServerConfig = serde_yaml::from_value(value).to_human_error()?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> buckle_core::Result<()> {
        if self.web.oidc.is_none() && self.web.user_acl.is_none() {
            // Allowed: local development with no auth. Nothing to check.
        }
        if let Some(oidc) = &self.web.oidc
            && self.web.base_url.is_none()
        {
            let _ = oidc;
            return Err(buckle_core::error::user(
                "`web.base_url` is required when OIDC is configured, so the callback URL can be built.",
                &[
                    "Set web.base_url to the public URL operators use, e.g. https://buckle.example.ts.net.",
                ],
            ));
        }
        Ok(())
    }
}

/// Replaces `${{ env.NAME }}` inside every string scalar, recursively.
pub fn interpolate_env(
    value: &mut serde_yaml::Value,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> buckle_core::Result<()> {
    match value {
        serde_yaml::Value::String(s) => {
            if s.contains("${{") {
                *s = interpolate_str(s, lookup)?;
            }
            Ok(())
        }
        serde_yaml::Value::Sequence(items) => items
            .iter_mut()
            .try_for_each(|v| interpolate_env(v, lookup)),
        serde_yaml::Value::Mapping(map) => map
            .iter_mut()
            .try_for_each(|(_, v)| interpolate_env(v, lookup)),
        _ => Ok(()),
    }
}

fn interpolate_str(
    input: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> buckle_core::Result<String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 3..];
        let Some(end) = after.find("}}") else {
            return Err(buckle_core::error::user(
                format!("Unterminated `${{{{` expression in configuration value '{input}'."),
                &["Close the expression with `}}`."],
            ));
        };
        let expr = after[..end].trim();
        let Some(var) = expr.strip_prefix("env.") else {
            return Err(buckle_core::error::user(
                format!("Unknown interpolation expression '{expr}'."),
                &["Only `env.VARIABLE_NAME` expressions are supported."],
            ));
        };
        let value = lookup(var).ok_or_else(|| {
            buckle_core::error::user(
                format!("The environment variable `{var}` referenced by the configuration is not set."),
                &["Set the variable in the server's environment (for example through a Nomad template stanza)."],
            )
        })?;
        out.push_str(&value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListenConfig {
    pub grpc: String,
    pub http: String,
    #[serde(default)]
    pub tls: TlsConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    #[serde(default)]
    pub mode: TlsMode,
    #[serde(default)]
    pub cert: Option<PathBuf>,
    #[serde(default)]
    pub key: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    /// Generate a certificate on first start under data_dir; agents pin its fingerprint.
    #[default]
    Auto,
    /// Use `cert` and `key`.
    Files,
    /// Plaintext; only sensible when every agent arrives over the tailnet.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    #[serde(default)]
    pub git: Option<GitSource>,
    /// A local directory (development, or a CI job that already checked out the repo).
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSource {
    pub url: String,
    #[serde(default = "GitSource::default_branch")]
    pub branch: String,
    #[serde(default = "GitSource::default_poll", with = "humantime_serde")]
    pub poll: Duration,
    #[serde(default)]
    pub credentials: Option<GitCredentials>,
    #[serde(default)]
    pub webhook_secret: Option<String>,
}

impl std::fmt::Debug for GitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitSource")
            .field("url", &redact_url(&self.url))
            .field("branch", &self.branch)
            .field("poll", &self.poll)
            .field("credentials", &self.credentials)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "***"),
            )
            .finish()
    }
}

/// A URL with any embedded password replaced, for logs.
pub fn redact_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) if u.password().is_some() => {
            let _ = u.set_password(Some("***"));
            u.to_string()
        }
        _ => url.to_owned(),
    }
}

impl GitSource {
    fn default_branch() -> String {
        "main".into()
    }
    fn default_poll() -> Duration {
        Duration::from_secs(30)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitCredentials {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl std::fmt::Debug for GitCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitCredentials")
            .field("token", &self.token.as_ref().map(|_| "***"))
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsConfig {
    #[serde(default = "AgentsConfig::default_auth")]
    pub auth: Vec<AgentAuth>,
    #[serde(default)]
    pub bootstrap_token: Option<String>,
    #[serde(default)]
    pub registration: Registration,
    #[serde(default)]
    pub minimum_agent_version: Option<String>,
    #[serde(default)]
    pub desired_version: Option<String>,
    /// Path to the tailscaled LocalAPI socket for whois.
    #[serde(default = "AgentsConfig::default_tailscale_socket")]
    pub tailscale_socket: PathBuf,
    /// How long a host may stay disconnected in the middle of its wave
    /// before the rollout counts it as failed.
    #[serde(
        default = "AgentsConfig::default_disconnect_timeout",
        with = "humantime_serde"
    )]
    pub disconnect_timeout: Duration,
    /// How long a host may stay applying (from the Apply being sent or
    /// re-sent) without an ApplyResult before the rollout counts it as failed.
    #[serde(
        default = "AgentsConfig::default_apply_timeout",
        with = "humantime_serde"
    )]
    pub apply_timeout: Duration,
}

impl std::fmt::Debug for AgentsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentsConfig")
            .field("auth", &self.auth)
            .field(
                "bootstrap_token",
                &self.bootstrap_token.as_ref().map(|_| "***"),
            )
            .field("registration", &self.registration)
            .field("minimum_agent_version", &self.minimum_agent_version)
            .field("desired_version", &self.desired_version)
            .field("tailscale_socket", &self.tailscale_socket)
            .field("disconnect_timeout", &self.disconnect_timeout)
            .field("apply_timeout", &self.apply_timeout)
            .finish()
    }
}

impl AgentsConfig {
    fn default_disconnect_timeout() -> Duration {
        Duration::from_secs(600)
    }

    fn default_apply_timeout() -> Duration {
        Duration::from_secs(30 * 60)
    }

    fn default_auth() -> Vec<AgentAuth> {
        vec![AgentAuth::Tailscale, AgentAuth::Token]
    }
    fn default_tailscale_socket() -> PathBuf {
        PathBuf::from("/var/run/tailscale/tailscaled.sock")
    }
}

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            auth: Self::default_auth(),
            bootstrap_token: None,
            registration: Registration::default(),
            minimum_agent_version: None,
            desired_version: None,
            tailscale_socket: Self::default_tailscale_socket(),
            disconnect_timeout: Self::default_disconnect_timeout(),
            apply_timeout: Self::default_apply_timeout(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentAuth {
    Tailscale,
    Token,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Registration {
    #[default]
    Auto,
    Approve,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebConfig {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub trust_proxy: bool,
    /// Who may read. Absent means everyone the OIDC provider authenticates (or everyone, without OIDC).
    #[serde(default)]
    pub user_acl: Option<filt_rs::Filter>,
    /// Who may approve, pause, roll back and manage hosts. Absent means nobody.
    #[serde(default)]
    pub admin_acl: Option<filt_rs::Filter>,
    #[serde(default)]
    pub oidc: Option<OidcConfig>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    /// Issuer; discovery at `{endpoint}/.well-known/openid-configuration`.
    pub endpoint: String,
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "OidcConfig::default_scopes")]
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for OidcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcConfig")
            .field("endpoint", &self.endpoint)
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .field("scopes", &self.scopes)
            .finish()
    }
}

impl OidcConfig {
    fn default_scopes() -> Vec<String> {
        vec!["openid".into(), "profile".into(), "email".into()]
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    #[serde(default)]
    pub providers: ProvidersConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub file: Option<FileProviderConfig>,
    #[serde(default)]
    pub env: Option<EmptyConfig>,
    /// Development only: commands the exec provider may run.
    #[serde(default)]
    pub exec: Option<ExecProviderConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyConfig {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileProviderConfig {
    #[serde(default)]
    pub root: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecProviderConfig {
    pub allow: Vec<String>,
}

/// `telemetry`: where traces and metrics go.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// Extra OTLP headers (often an API key); values are redacted in logs.
    #[serde(default)]
    pub otlp_headers: IndexMap<String, String>,
}

impl std::fmt::Debug for TelemetryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryConfig")
            .field("otlp_endpoint", &self.otlp_endpoint)
            .field(
                "otlp_headers",
                &self.otlp_headers.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const EXAMPLE: &str = r#"
listen:
  grpc: "0.0.0.0:7700"
  http: "100.64.0.10:7701"
  tls: { mode: auto }
data_dir: /data
source:
  git:
    url: https://github.com/example/fleet-config.git
    branch: main
    poll: 30s
    credentials: { token: "${{ env.GIT_TOKEN }}" }
agents:
  auth: [tailscale, token]
  bootstrap_token: "${{ env.BUCKLE_BOOTSTRAP_TOKEN }}"
  registration: approve
  minimum_agent_version: "2.0.0"
web:
  base_url: https://buckle.example.ts.net
  user_acl: 'claims.email endswith "@example.com"'
  admin_acl: '"buckle-admins" in claims.groups'
  oidc:
    endpoint: https://auth.example.com
    client_id: buckle
    client_secret: "${{ env.OIDC_CLIENT_SECRET }}"
secrets:
  providers:
    file: { root: /secrets }
    env: {}
"#;

    fn with_env(text: &str) -> buckle_core::Result<ServerConfig> {
        let mut value: serde_yaml::Value = serde_yaml::from_str(text).unwrap();
        let lookup = |name: &str| match name {
            "GIT_TOKEN" => Some("ghp_x".to_string()),
            "BUCKLE_BOOTSTRAP_TOKEN" => Some("boot".to_string()),
            "OIDC_CLIENT_SECRET" => Some("shh".to_string()),
            _ => None,
        };
        interpolate_env(&mut value, &lookup)?;
        let cfg: ServerConfig = serde_yaml::from_value(value).to_human_error()?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn parses_the_documented_example_with_interpolation() {
        let cfg = with_env(EXAMPLE).expect("config parses");
        assert_eq!(
            cfg.source.git.as_ref().unwrap().poll,
            Duration::from_secs(30)
        );
        assert_eq!(
            cfg.source
                .git
                .as_ref()
                .unwrap()
                .credentials
                .as_ref()
                .unwrap()
                .token
                .as_deref(),
            Some("ghp_x")
        );
        assert_eq!(cfg.agents.bootstrap_token.as_deref(), Some("boot"));
        assert_eq!(cfg.agents.registration, Registration::Approve);
        assert_eq!(
            cfg.agents.auth,
            vec![AgentAuth::Tailscale, AgentAuth::Token]
        );
        assert_eq!(cfg.web.oidc.as_ref().unwrap().client_secret, "shh");
        assert!(cfg.web.admin_acl.is_some());
        assert_eq!(cfg.listen.tls.mode, TlsMode::Auto);
    }

    #[test]
    fn unset_variables_are_errors_naming_the_variable() {
        let mut value: serde_yaml::Value = serde_yaml::from_str("a: \"${{ env.NOPE }}\"").unwrap();
        let err = interpolate_env(&mut value, &|_| None).unwrap_err();
        assert!(err.description().contains("NOPE"));
    }

    #[rstest]
    #[case("plain", "plain")]
    #[case("${{ env.A }}", "1")]
    #[case("x-${{env.A}}-${{ env.B }}", "x-1-2")]
    fn interpolation_handles_multiple_expressions(#[case] input: &str, #[case] expected: &str) {
        let lookup = |n: &str| match n {
            "A" => Some("1".to_string()),
            "B" => Some("2".to_string()),
            _ => None,
        };
        assert_eq!(interpolate_str(input, &lookup).unwrap(), expected);
    }

    #[test]
    fn debug_output_redacts_secrets() {
        let cfg = with_env(EXAMPLE).unwrap();
        let dbg = format!("{cfg:?}");
        for secret in ["ghp_x", "boot", "shh"] {
            assert!(
                !dbg.contains(&format!("\"{secret}\"")),
                "{secret} leaked: {dbg}"
            );
        }
        assert_eq!(cfg.agents.disconnect_timeout, Duration::from_secs(600));
        assert_eq!(cfg.agents.apply_timeout, Duration::from_secs(1800));
        assert_eq!(
            redact_url("https://user:pw@github.com/x.git"),
            "https://user:***@github.com/x.git"
        );
    }

    #[test]
    fn oidc_requires_a_base_url() {
        let yaml = EXAMPLE.replace("  base_url: https://buckle.example.ts.net\n", "");
        assert!(with_env(&yaml).is_err());
    }
}
