//! Join commands (the Add an agent wizard): one line an admin pastes on a new
//! host to install Buckle, optionally join the tailnet, and enrol the agent.
//!
//! A [`JoinTicket`] is created by an admin and lives in memory until it
//! expires (`agents.join.ttl`, at most an hour), so the single-use keys it
//! carries never reach the disk:
//!
//! - a **join token**, when the agent enrols by token (or must skip
//!   approval): exchanged once, by the host it names, for a per-host token;
//! - a **Tailscale auth key**, when `agents.tailscale.provisioning` is set:
//!   the admin consents in Tailscale (OAuth authorization code flow), and the
//!   server trades the code for a key that adds one node, once, within the
//!   hour. The OAuth client secret never leaves the server.
//!
//! Only the admin who created a ticket may read it back.

use crate::config::{AgentAuth, Registration, ServerConfig, TailscaleProvisioning, TlsMode};
use buckle_api::{
    JoinMethod, JoinOptions, JoinPlatform, JoinRequest, JoinState, JoinTicket, TailscaleJoinOptions,
};
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Tickets alive at once; more is someone scripting the endpoint.
pub const MAX_TICKETS: usize = 64;

/// The OAuth scope that lets Tailscale issue one auth key per consent.
pub const TAILSCALE_SCOPE: &str = "auth_keys:create:once";

/// Where Tailscale returns after consent, below `web.base_url`.
pub const CALLBACK_PATH: &str = "/api/v1/join/tailscale/callback";

/// What every join command is built from.
#[derive(Debug, Clone)]
pub struct JoinSettings {
    /// The gRPC URL agents dial.
    pub server_url: String,
    /// `--insecure-plaintext` (the gRPC listener has no TLS).
    pub insecure_plaintext: bool,
    /// `--ca-fingerprint`, for a generated (`auto`) certificate.
    pub ca_fingerprint: Option<String>,
    /// The directory `install.sh` and `install.ps1` are fetched from.
    pub install_url: String,
    /// The release the installer installs; `None` takes the latest.
    pub version: Option<String>,
    pub ttl: Duration,
    /// Accepted methods, preferred first.
    pub methods: Vec<JoinMethod>,
    pub approval_required: bool,
    pub tailscale: Option<Provisioning>,
}

/// Tailscale provisioning with what the flow needs resolved.
#[derive(Debug, Clone)]
pub struct Provisioning {
    pub config: TailscaleProvisioning,
    /// The tags a node may advertise, default first.
    pub tags: Vec<String>,
    pub redirect_uri: String,
}

impl JoinSettings {
    /// Settings for `config`, whose gRPC certificate has `fingerprint` (hex),
    /// served by a binary of `version`.
    pub fn from_config(config: &ServerConfig, fingerprint: Option<&str>, version: &str) -> Self {
        let tls = config.listen.tls.mode;
        let join = &config.agents.join;
        let release = crate::upgrades::is_release_version(version)
            && !version.contains('-')
            && !version.starts_with("0.");
        let version = release.then(|| version.trim_start_matches('v').to_owned());
        let install_url = join
            .install_url
            .clone()
            .unwrap_or_else(|| match &version {
                Some(v) => {
                    format!("https://github.com/SierraSoftworks/buckle/releases/download/v{v}")
                }
                None => "https://github.com/SierraSoftworks/buckle/releases/latest/download".into(),
            })
            .trim_end_matches('/')
            .to_owned();
        let server_url = join.server_url.clone().unwrap_or_else(|| {
            derive_server_url(config.web.base_url.as_deref(), &config.listen.grpc, tls)
        });
        let mut methods = Vec::new();
        for (auth, method) in [
            (AgentAuth::Tailscale, JoinMethod::Tailscale),
            (AgentAuth::Token, JoinMethod::Token),
        ] {
            if config.agents.auth.contains(&auth) {
                methods.push(method);
            }
        }
        let tailscale = config.agents.tailscale.provisioning.clone().map(|p| {
            let tags = if p.tags.is_empty() {
                config.agents.tailscale.allowed_tags.clone()
            } else {
                p.tags.clone()
            };
            Provisioning {
                tags: tags.iter().map(|t| normalise_tag(t)).collect(),
                redirect_uri: format!(
                    "{}{CALLBACK_PATH}",
                    config
                        .web
                        .base_url
                        .as_deref()
                        .unwrap_or_default()
                        .trim_end_matches('/')
                ),
                config: p,
            }
        });
        Self {
            server_url,
            insecure_plaintext: tls == TlsMode::None,
            ca_fingerprint: (tls == TlsMode::Auto)
                .then(|| fingerprint.map(|f| format!("sha256:{}", f.trim_start_matches("sha256:"))))
                .flatten(),
            install_url,
            version,
            ttl: join.ttl,
            methods,
            approval_required: config.agents.registration == Registration::Approve,
            tailscale,
        }
    }

    pub fn options(&self) -> JoinOptions {
        JoinOptions {
            methods: self.methods.clone(),
            server_url: self.server_url.clone(),
            tailscale: self.tailscale.as_ref().map(|p| TailscaleJoinOptions {
                tags: p.tags.clone(),
            }),
            ttl_seconds: self.ttl.as_secs(),
            approval_required: self.approval_required,
        }
    }

    /// Whether a ticket for `method` carries a join token: always for token
    /// enrolment, and over Tailscale only to skip approval.
    fn needs_token(&self, method: JoinMethod) -> bool {
        method == JoinMethod::Token || self.approval_required
    }
}

fn normalise_tag(tag: &str) -> String {
    if tag.starts_with("tag:") {
        tag.to_owned()
    } else {
        format!("tag:{tag}")
    }
}

/// `web.base_url`'s host with the `listen.grpc` port, or the listen address
/// itself when there is no base URL.
pub fn derive_server_url(base_url: Option<&str>, grpc: &str, tls: TlsMode) -> String {
    let scheme = if tls == TlsMode::None {
        "http"
    } else {
        "https"
    };
    let port = grpc.rsplit_once(':').map(|(_, p)| p).unwrap_or("7700");
    let host = base_url
        .and_then(|u| url::Url::parse(u).ok())
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| {
            grpc.rsplit_once(':')
                .map(|(h, _)| h.to_owned())
                .unwrap_or_else(|| grpc.to_owned())
        });
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    format!("{scheme}://{host}:{port}")
}

/// The secrets a command carries.
#[derive(Debug, Default, Clone, Copy)]
pub struct Keys<'a> {
    pub join_token: Option<&'a str>,
    pub tailscale_key: Option<&'a str>,
}

/// The command for `request` on its platform.
pub fn render(
    settings: &JoinSettings,
    request: &JoinRequest,
    tags: &[String],
    keys: Keys<'_>,
) -> String {
    let windows = request.platform == JoinPlatform::Windows;
    let quote = |s: &str| if windows { ps_quote(s) } else { sh_quote(s) };
    let flag = |unix: &str, ps: &str| {
        if windows {
            ps.to_owned()
        } else {
            unix.to_owned()
        }
    };

    let mut installer: Vec<String> = Vec::new();
    if let Some(v) = &settings.version {
        installer.push(format!("{} {}", flag("--version", "-Version"), quote(v)));
    }
    if let Some(key) = keys.tailscale_key {
        installer.push(format!(
            "{} {}",
            flag("--tailscale-auth-key", "-TailscaleAuthKey"),
            quote(key)
        ));
        if !tags.is_empty() {
            installer.push(format!(
                "{} {}",
                flag("--tailscale-tags", "-TailscaleTags"),
                quote(&tags.join(","))
            ));
        }
        if let Some(host) = &request.host_id {
            installer.push(format!(
                "{} {}",
                flag("--tailscale-hostname", "-TailscaleHostname"),
                quote(host)
            ));
        }
    }

    let mut up = vec![format!("up --server {}", quote(&settings.server_url))];
    if settings.insecure_plaintext {
        up.push("--insecure-plaintext".into());
    }
    if let Some(fp) = &settings.ca_fingerprint {
        up.push(format!("--ca-fingerprint {}", quote(fp)));
    }
    if let Some(token) = keys.join_token {
        up.push(format!("--token {}", quote(token)));
    }
    if let Some(host) = &request.host_id {
        up.push(format!("--host-id {}", quote(host)));
    }
    for (k, v) in &request.labels {
        up.push(format!("--label {}", quote(&format!("{k}={v}"))));
    }

    let (head, cont) = if windows {
        (
            format!(
                "& ([scriptblock]::Create((irm {}))) `",
                ps_quote(&format!("{}/install.ps1", settings.install_url))
            ),
            " `",
        )
    } else {
        (
            format!(
                "curl -fsSL {} | sudo sh -s -- \\",
                sh_quote(&format!("{}/install.sh", settings.install_url))
            ),
            " \\",
        )
    };
    let mut lines = vec![head];
    lines.extend(installer.into_iter().map(|l| format!("  {l}{cont}")));
    lines.push(format!("  {}", up.join(" ")));
    lines.join("\n")
}

/// Single-quotes `s` for a POSIX shell unless it is plainly safe.
pub fn sh_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.,:/=@%+".contains(c));
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Single-quotes `s` for PowerShell unless it is plainly safe.
pub fn ps_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:/=".contains(c))
        && !s.starts_with('-');
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// Why a request for a join command was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// The request itself is wrong (400).
    Invalid(String),
    /// Too many tickets are alive (429).
    TooMany,
    /// No such ticket for this admin, or it expired (404).
    NotFound,
}

impl std::fmt::Display for JoinError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => f.write_str(m),
            Self::TooMany => write!(
                f,
                "{MAX_TICKETS} join commands are already waiting to be used; let some expire first."
            ),
            Self::NotFound => f.write_str("That join command does not exist or has expired."),
        }
    }
}

struct Entry {
    ticket: JoinTicket,
    request: JoinRequest,
    tags: Vec<String>,
    /// The OAuth `state` while consent is outstanding.
    oauth_state: Option<String>,
    /// The join token in the clear until the command is rendered.
    join_token: Option<String>,
    token_hash: Option<String>,
    redeemed: bool,
}

/// What redeeming a join token tells the enrolment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    pub ticket: String,
    /// The admin who created the command.
    pub created_by: String,
}

/// The live tickets.
pub struct Joins {
    settings: JoinSettings,
    http: reqwest::Client,
    inner: Mutex<HashMap<String, Entry>>,
}

impl std::fmt::Debug for Joins {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Joins").finish_non_exhaustive()
    }
}

impl Default for Joins {
    /// Token joins against a local plaintext server (for tests).
    fn default() -> Self {
        Self::new(JoinSettings {
            server_url: "http://127.0.0.1:7700".into(),
            insecure_plaintext: true,
            ca_fingerprint: None,
            install_url: "https://github.com/SierraSoftworks/buckle/releases/latest/download"
                .into(),
            version: None,
            ttl: Duration::from_secs(3600),
            methods: vec![JoinMethod::Token],
            approval_required: false,
            tailscale: None,
        })
    }
}

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::fill(&mut b[..]);
    hex::encode(b)
}

fn valid_label_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
}

impl Joins {
    pub fn new(settings: JoinSettings) -> Self {
        Self {
            settings,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            inner: Mutex::default(),
        }
    }

    pub fn settings(&self) -> &JoinSettings {
        &self.settings
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn validate(&self, request: &JoinRequest) -> Result<Vec<String>, JoinError> {
        let s = &self.settings;
        if !s.methods.contains(&request.method) {
            return Err(JoinError::Invalid(format!(
                "This server does not accept agents by {:?}; it accepts {:?}.",
                request.method, s.methods
            )));
        }
        if let Some(h) = &request.host_id
            && !crate::grpc::auth::valid_host_id(h)
        {
            return Err(JoinError::Invalid(format!(
                "'{h}' is not a host name: use 1–128 letters, digits, '.', '_' or '-'."
            )));
        }
        for (k, v) in &request.labels {
            if !valid_label_key(k) || v.contains(['\n', '\r']) {
                return Err(JoinError::Invalid(format!(
                    "'{k}' is not a label key: use letters, digits, '.', '_', '-' or '/'."
                )));
            }
        }
        let provisioning = match (&s.tailscale, request.method) {
            (Some(p), JoinMethod::Tailscale) => p,
            _ if !request.tailscale_tags.is_empty() => {
                return Err(JoinError::Invalid(
                    "Tailscale tags only apply when the server provisions the node's tailnet key."
                        .into(),
                ));
            }
            _ => return Ok(Vec::new()),
        };
        if request.tailscale_tags.is_empty() {
            return Ok(provisioning.tags.first().cloned().into_iter().collect());
        }
        let tags: Vec<String> = request
            .tailscale_tags
            .iter()
            .map(|t| normalise_tag(t))
            .collect();
        if let Some(t) = tags.iter().find(|t| !provisioning.tags.contains(t)) {
            return Err(JoinError::Invalid(format!(
                "'{t}' is not one of the tags joined nodes may advertise ({}).",
                provisioning.tags.join(", ")
            )));
        }
        Ok(tags)
    }

    /// Creates a ticket for `request` on behalf of `actor`.
    pub fn create(
        &self,
        request: JoinRequest,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Result<JoinTicket, JoinError> {
        let tags = self.validate(&request)?;
        let mut map = self.lock();
        map.retain(|_, e| e.ticket.expires_at > now);
        if map.len() >= MAX_TICKETS {
            return Err(JoinError::TooMany);
        }
        let id = random_hex(12);
        let join_token = self
            .settings
            .needs_token(request.method)
            .then(crate::grpc::auth::new_token);
        let ttl =
            chrono::Duration::from_std(self.settings.ttl).unwrap_or(chrono::Duration::hours(1));
        let mut entry = Entry {
            ticket: JoinTicket {
                id: id.clone(),
                platform: request.platform,
                method: request.method,
                host_id: request.host_id.clone(),
                labels: request.labels.clone(),
                created_by: actor.to_owned(),
                created_at: now,
                expires_at: now + ttl,
                state: JoinState::Failed {
                    reason: String::new(),
                },
            },
            token_hash: join_token.as_deref().map(crate::grpc::auth::hash_token),
            join_token,
            request,
            tags,
            oauth_state: None,
            redeemed: false,
        };
        match (&self.settings.tailscale, entry.request.method) {
            (Some(p), JoinMethod::Tailscale) => {
                let state = random_hex(24);
                entry.ticket.state = JoinState::Authorize {
                    authorize_url: authorize_url(p, &state),
                };
                entry.oauth_state = Some(state);
            }
            _ => self.make_ready(&mut entry, None),
        }
        let ticket = entry.ticket.clone();
        map.insert(id, entry);
        Ok(ticket)
    }

    fn make_ready(&self, entry: &mut Entry, tailscale_key: Option<&str>) {
        let command = render(
            &self.settings,
            &entry.request,
            &entry.tags,
            Keys {
                join_token: entry.join_token.as_deref(),
                tailscale_key,
            },
        );
        entry.join_token = None;
        entry.ticket.state = JoinState::Ready { command };
    }

    /// The ticket `id`, for the admin who created it.
    pub fn get(&self, id: &str, actor: &str, now: DateTime<Utc>) -> Result<JoinTicket, JoinError> {
        let map = self.lock();
        map.get(id)
            .filter(|e| e.ticket.created_by == actor && e.ticket.expires_at > now)
            .map(|e| e.ticket.clone())
            .ok_or(JoinError::NotFound)
    }

    /// Finishes consent for the ticket waiting on `state`: trades `code`
    /// for a Tailscale auth key (or records `error`). Returns the ticket id;
    /// the ticket says whether it worked.
    pub async fn complete_tailscale(
        &self,
        state: &str,
        code: Option<&str>,
        error: Option<&str>,
        actor: &str,
        now: DateTime<Utc>,
    ) -> Result<String, JoinError> {
        let Some(p) = &self.settings.tailscale else {
            return Err(JoinError::NotFound);
        };
        let id = {
            let mut map = self.lock();
            let (id, entry) = map
                .iter_mut()
                .find(|(_, e)| e.oauth_state.as_deref() == Some(state))
                .ok_or(JoinError::NotFound)?;
            if entry.ticket.created_by != actor || entry.ticket.expires_at <= now {
                return Err(JoinError::NotFound);
            }
            // One use: a replayed callback finds nothing.
            entry.oauth_state = None;
            id.clone()
        };
        let outcome = match (code, error) {
            (_, Some(e)) if e == "access_denied" => {
                Err("Tailscale access was denied, so no auth key was issued.".to_owned())
            }
            (_, Some(e)) => Err(format!("Tailscale refused to issue an auth key ({e}).")),
            (Some(code), None) => self.exchange(p, code).await,
            (None, None) => Err("Tailscale returned without a code.".to_owned()),
        };
        let mut map = self.lock();
        let entry = map.get_mut(&id).ok_or(JoinError::NotFound)?;
        match outcome {
            Ok((key, lifetime)) => {
                if let Some(life) = lifetime {
                    entry.ticket.expires_at = entry.ticket.expires_at.min(now + life);
                }
                self.make_ready(entry, Some(&key));
            }
            Err(reason) => {
                tracing::warn!(%reason, "Tailscale provisioning failed");
                entry.join_token = None;
                entry.token_hash = None;
                entry.ticket.state = JoinState::Failed { reason };
            }
        }
        Ok(id)
    }

    /// The authorization code flow's token exchange: the access token is the
    /// auth key.
    async fn exchange(
        &self,
        p: &Provisioning,
        code: &str,
    ) -> Result<(String, Option<chrono::Duration>), String> {
        #[derive(serde::Deserialize)]
        struct Token {
            access_token: String,
            #[serde(default)]
            expires_in: Option<i64>,
        }
        let mut form = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", code.to_owned()),
            ("client_id", p.config.client_id.clone()),
            ("client_secret", p.config.client_secret.clone()),
            ("redirect_uri", p.redirect_uri.clone()),
        ];
        if !p.config.node_attributes.is_empty() {
            form.push(("node_attributes", p.config.node_attributes.join(",")));
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter())
            .finish();
        let url = format!(
            "{}/api/v2/oauth/token",
            p.config.api_url.trim_end_matches('/')
        );
        let response = self
            .http
            .post(&url)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|e| format!("Tailscale could not be reached ({e})."))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let text: String = text.chars().take(200).collect();
            return Err(format!("Tailscale refused the code ({status}): {text}"));
        }
        let token: Token = response
            .json()
            .await
            .map_err(|e| format!("Tailscale's answer could not be read ({e})."))?;
        Ok((
            token.access_token,
            token.expires_in.map(chrono::Duration::seconds),
        ))
    }

    /// Claims the join token `token` for `host`: once, before it expires,
    /// and only by the host the ticket names (if it names one).
    pub fn redeem(&self, token: &str, host: &str, now: DateTime<Utc>) -> Option<Redeemed> {
        let hash = crate::grpc::auth::hash_token(token);
        let mut map = self.lock();
        let entry = map
            .values_mut()
            .find(|e| e.token_hash.as_deref() == Some(hash.as_str()))?;
        let usable = !entry.redeemed
            && entry.ticket.expires_at > now
            && matches!(entry.ticket.state, JoinState::Ready { .. })
            && entry.ticket.host_id.as_deref().is_none_or(|h| h == host);
        if !usable {
            return None;
        }
        entry.redeemed = true;
        Some(Redeemed {
            ticket: entry.ticket.id.clone(),
            created_by: entry.ticket.created_by.clone(),
        })
    }

    /// Gives back a token [`Self::redeem`] claimed when enrolment then failed.
    pub fn release(&self, ticket: &str) {
        if let Some(e) = self.lock().get_mut(ticket) {
            e.redeemed = false;
        }
    }
}

fn authorize_url(p: &Provisioning, state: &str) -> String {
    let mut url = url::Url::parse(&format!(
        "{}/a/oauth_authorize",
        p.config.login_url.trim_end_matches('/')
    ))
    .expect("the Tailscale login URL is valid");
    url.query_pairs_mut()
        .append_pair("client_id", &p.config.client_id)
        .append_pair("redirect_uri", &p.redirect_uri)
        .append_pair("scope", TAILSCALE_SCOPE)
        .append_pair("state", state)
        .append_pair("response_type", "code");
    url.to_string()
}

#[cfg(test)]
mod tests;
