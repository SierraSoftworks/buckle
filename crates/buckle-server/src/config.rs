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
        let used = std::cell::RefCell::new(std::collections::BTreeSet::new());
        interpolate_env(&mut value, &|name| {
            used.borrow_mut().insert(name.to_owned());
            std::env::var(name).ok()
        })?;
        let mut config: ServerConfig = serde_yaml::from_value(value).to_human_error()?;
        // Whatever server.yaml reads from the environment is the server's
        // own configuration; fleet secrets may not read it back out.
        config.secrets.reserved_env = used.into_inner();
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> buckle_core::Result<()> {
        if let Some(git) = &self.source.git
            && git
                .webhook_secret
                .as_deref()
                .is_some_and(|s| s.trim().is_empty())
        {
            return Err(buckle_core::error::user(
                "`source.git.webhook_secret` is empty, so anyone could sign webhook calls.",
                &[
                    "Set it to a long random value (and the same in the webhook), or remove it to disable the webhook.",
                ],
            ));
        }
        if self.web.oidc.is_some() && self.web.base_url.is_none() {
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
    /// How long one fetch may take before it is interrupted (default 2m).
    #[serde(default = "GitSource::default_fetch_timeout", with = "humantime_serde")]
    pub fetch_timeout: Duration,
}

impl std::fmt::Debug for GitSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitSource")
            .field("url", &redact_url(&self.url))
            .field("branch", &self.branch)
            .field("poll", &self.poll)
            .field("fetch_timeout", &self.fetch_timeout)
            .field("credentials", &self.credentials)
            .field(
                "webhook_secret",
                &self.webhook_secret.as_ref().map(|_| "***"),
            )
            .finish()
    }
}

/// A URL with any embedded password replaced, for logs. A user name with
/// no password is replaced too, since tokens are often passed that way
/// (`https://ghp_…@github.com/…`).
pub fn redact_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) if u.password().is_some() => {
            let _ = u.set_password(Some("***"));
            u.to_string()
        }
        Ok(mut u) if !u.username().is_empty() => {
            let _ = u.set_username("***");
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
    fn default_fetch_timeout() -> Duration {
        Duration::from_secs(120)
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
    /// What happens to a host the server has never seen: `auto` makes it
    /// active at once, `approve` leaves it pending for an admin. Under
    /// Tailscale auth, `auto` only activates nodes that `tailscale` admits.
    #[serde(default)]
    pub registration: Registration,
    #[serde(default)]
    pub minimum_agent_version: Option<String>,
    #[serde(default)]
    pub desired_version: Option<String>,
    /// Path to the tailscaled LocalAPI socket for whois.
    #[serde(default = "AgentsConfig::default_tailscale_socket")]
    pub tailscale_socket: PathBuf,
    /// Which tailnet nodes are trusted as agents (design §02, §08).
    #[serde(default)]
    pub tailscale: TailscaleConfig,
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
            .field("tailscale", &self.tailscale)
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
            tailscale: TailscaleConfig::default(),
            disconnect_timeout: Self::default_disconnect_timeout(),
            apply_timeout: Self::default_apply_timeout(),
        }
    }
}

/// `agents.tailscale`: which tailnet nodes are trusted as agents.
///
/// Every node on the tailnet can reach the gRPC listener, including nodes
/// shared in from other tailnets, so whois alone proves *who* a peer is, not
/// that it belongs to the fleet. A node is admitted when it carries one of
/// `allowed_tags` (ACL tags such as `tag:buckle`; the `tag:` prefix is
/// optional here) or is owned by one of `allowed_users` (exact login names
/// such as `ada@example.com`).
///
/// ```yaml
/// agents:
///   registration: auto
///   tailscale:
///     allowed_tags: ["tag:buckle"]
///     allowed_users: ["ops@example.com"]
/// ```
///
/// * With at least one entry, a node that matches neither list is refused,
///   whether or not it is already registered, and a matching new node is
///   registered according to `registration`.
/// * With both lists empty (the default), nothing is trusted to activate
///   itself: under `registration: auto` a new tailnet node is registered as
///   *pending* and waits for an admin, exactly as under `approve`. The server
///   warns at start-up when this is the case.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TailscaleConfig {
    #[serde(default)]
    pub allowed_tags: Vec<String>,
    #[serde(default)]
    pub allowed_users: Vec<String>,
}

impl TailscaleConfig {
    /// Whether any node is restricted at all (some tag or user is listed).
    pub fn is_configured(&self) -> bool {
        !self.allowed_tags.is_empty() || !self.allowed_users.is_empty()
    }

    /// Whether a node with these tags and owner is admitted.
    pub fn admits(&self, tags: &[String], user: Option<&str>) -> bool {
        fn norm(t: &str) -> &str {
            t.strip_prefix("tag:").unwrap_or(t)
        }
        let tagged = tags
            .iter()
            .any(|t| self.allowed_tags.iter().any(|a| norm(a) == norm(t)));
        let owned = user.is_some_and(|u| self.allowed_users.iter().any(|a| a == u));
        tagged || owned
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
    /// Take the client address (for `client_ip` in ACLs and sign-in rate
    /// limits) from `X-Forwarded-For`, as written by `trusted_proxy_hops`
    /// reverse proxies in front of the server.
    #[serde(default)]
    pub trust_proxy: bool,
    /// How many reverse proxies in front of the server append to
    /// `X-Forwarded-For` (default 1). The client address is the entry that
    /// many places from the right; entries further left are whatever the
    /// client sent and are never trusted.
    #[serde(default)]
    pub trusted_proxy_hops: Option<u32>,
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
    /// Environment variables server.yaml interpolates (filled in by
    /// [`ServerConfig::parse`]); the env provider never reads them.
    #[serde(skip)]
    pub reserved_env: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub file: Option<FileProviderConfig>,
    #[serde(default)]
    pub env: Option<EnvProviderConfig>,
    /// Development only: commands the exec provider may run.
    #[serde(default)]
    pub exec: Option<ExecProviderConfig>,
}

/// `secrets.providers.env`: which of the server's environment variables
/// fleet secrets may read (`{ env: NAME }` in buckle.yaml).
///
/// Entries are exact names or prefixes ending in `*`. The server never hands
/// out [`EnvProviderConfig::ALWAYS_DENIED`], any variable server.yaml itself
/// interpolates (`${{ env.NAME }}`), or anything in `deny`; when `allow` is
/// given, only matching variables are read at all.
///
/// ```yaml
/// secrets:
///   providers:
///     env:
///       allow: ["FLEET_*"]          # optional; absent means any not denied
///       deny: [AWS_SECRET_ACCESS_KEY]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvProviderConfig {
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl EnvProviderConfig {
    /// The server's own variables, denied whatever the configuration says.
    pub const ALWAYS_DENIED: &'static [&'static str] = &[
        "BUCKLE_*",
        "OIDC_CLIENT_SECRET",
        "GIT_TOKEN",
        "GIT_WEBHOOK_SECRET",
        "OTEL_EXPORTER_OTLP_HEADERS",
        "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        "OTEL_EXPORTER_OTLP_METRICS_HEADERS",
        "OTEL_EXPORTER_OTLP_LOGS_HEADERS",
    ];
}

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

/// `telemetry`: where traces and metrics go. The binary reads this section
/// (through [`TelemetryConfig::peek`]) before the server starts, to set up
/// OTLP export; `--otel-endpoint` and the standard `OTEL_EXPORTER_OTLP_*`
/// variables take precedence over it.
///
/// ```yaml
/// telemetry:
///   otlp_endpoint: http://otel-collector.example.internal:4317
///   otlp_headers: { x-honeycomb-team: "${{ env.HONEYCOMB_KEY }}" }
/// ```
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryConfig {
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    /// Extra OTLP headers (often an API key); values are redacted in logs.
    #[serde(default)]
    pub otlp_headers: IndexMap<String, String>,
}

impl TelemetryConfig {
    /// The `telemetry` section of the server.yaml at `path`, interpolated,
    /// for setting up tracing before the server starts. `None` when the file
    /// or the section cannot be read; the full load reports why.
    pub fn peek(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let root: serde_yaml::Value = serde_yaml::from_str(&text).ok()?;
        let mut section = root.get("telemetry")?.clone();
        interpolate_env(&mut section, &|name| std::env::var(name).ok()).ok()?;
        serde_yaml::from_value(section).ok()
    }
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
  tailscale:
    allowed_tags: ["tag:buckle"]
    allowed_users: [ops@example.com]
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
        assert!(cfg.agents.tailscale.admits(&["tag:buckle".into()], None));
        assert!(cfg.agents.tailscale.admits(&[], Some("ops@example.com")));
        assert!(!cfg.agents.tailscale.admits(&[], Some("eve@example.com")));
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
        assert_eq!(
            redact_url("https://ghp_token@github.com/x.git"),
            "https://***@github.com/x.git"
        );
        assert_eq!(
            cfg.source.git.as_ref().unwrap().fetch_timeout,
            Duration::from_secs(120)
        );
    }

    #[test]
    fn interpolated_variables_are_reserved_from_the_env_provider() {
        // SAFETY: the variable is unique to this test.
        unsafe { std::env::set_var("BUCKLE_CFG_TEST_TOKEN", "boot") };
        let cfg = ServerConfig::parse(
            "listen: { grpc: \"127.0.0.1:0\", http: \"127.0.0.1:0\" }\nsource: { path: /x }\nagents: { bootstrap_token: \"${{ env.BUCKLE_CFG_TEST_TOKEN }}\" }\nsecrets: { providers: { env: { deny: [AWS_*] } } }\n",
        )
        .unwrap();
        assert!(cfg.secrets.reserved_env.contains("BUCKLE_CFG_TEST_TOKEN"));
        assert_eq!(
            cfg.secrets.providers.env.as_ref().unwrap().deny,
            vec!["AWS_*".to_string()]
        );
    }

    #[test]
    fn an_empty_webhook_secret_is_refused() {
        let yaml = EXAMPLE.replace(
            "    poll: 30s\n",
            "    poll: 30s\n    webhook_secret: \"\"\n",
        );
        let err = with_env(&yaml).unwrap_err();
        assert!(err.description().contains("webhook_secret"));
    }

    #[test]
    fn the_telemetry_section_can_be_read_before_the_server_starts() {
        // SAFETY: the variable is unique to this test.
        unsafe { std::env::set_var("BUCKLE_CFG_TEST_OTLP_KEY", "k3y") };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.yaml");
        std::fs::write(
            &path,
            "listen: { grpc: x, http: y }\nsource: { path: /x }\ntelemetry:\n  otlp_endpoint: http://collector:4317\n  otlp_headers: { x-api-key: \"${{ env.BUCKLE_CFG_TEST_OTLP_KEY }}\" }\n",
        )
        .unwrap();
        let t = TelemetryConfig::peek(&path).unwrap();
        assert_eq!(t.otlp_endpoint.as_deref(), Some("http://collector:4317"));
        assert_eq!(t.otlp_headers["x-api-key"], "k3y");
        assert!(!format!("{t:?}").contains("k3y"));
        assert!(TelemetryConfig::peek(&dir.path().join("missing.yaml")).is_none());
    }

    #[test]
    fn oidc_requires_a_base_url() {
        let yaml = EXAMPLE.replace("  base_url: https://buckle.example.ts.net\n", "");
        assert!(with_env(&yaml).is_err());
    }
}
