//! Cluster credentials from Buckle-issued identities.
//!
//! * [`Transport::issue_identity`] asks the server for a short-lived JWT for
//!   a cluster target and a [`Purpose`]; the server checks the grant and
//!   audits it.
//! * [`Login`] turns the JWT into what the cluster's API takes: a Nomad ACL
//!   token from `POST /v1/acl/login` against a `jwt` auth method, or the JWT
//!   itself as a Kubernetes bearer token.
//! * [`Credentials`] caches each credential until shortly before it expires,
//!   so a plan, apply and health pass in a row log in once. Nothing is
//!   written to disk.

use crate::transport::Transport;
use buckle_core::plan::RevisionId;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// What a credential may be used for; the cluster's binding rules or RBAC
/// map it to permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Purpose {
    /// Facts, plans and health.
    Read,
    /// Applying workloads.
    Deploy,
    /// Draining this host's own node.
    Drain,
}

/// A request for an identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IdentityRequest {
    /// The cluster target (the token's audience).
    pub target: String,
    pub purpose: Purpose,
    /// The revision declaring the target; `None` for the stable revision.
    pub revision: Option<RevisionId>,
    /// The rollout the credential serves, if any.
    pub rollout: Option<String>,
}

/// A JWT the server issued.
#[derive(Clone)]
pub struct IssuedIdentity {
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub audience: String,
    /// The token's `jti`, as the server's audit log records it.
    pub id: String,
}

impl std::fmt::Debug for IssuedIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedIdentity")
            .field("audience", &self.audience)
            .field("id", &self.id)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

/// How a cluster accepts a Buckle-issued JWT.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Login {
    /// Exchange it at `{api}/v1/acl/login` with the named `jwt` auth method.
    Nomad { api: url::Url, auth_method: String },
    /// Present it as a bearer token (a JWT authenticator trusts the issuer).
    Kubernetes,
}

impl Login {
    /// The auth method name the Nomad trust setup creates by default.
    pub const DEFAULT_NOMAD_AUTH_METHOD: &'static str = "buckle";
}

/// A credential for a cluster's API.
#[derive(Clone)]
pub enum Credential {
    /// A Nomad ACL token (`X-Nomad-Token`).
    Nomad {
        secret_id: String,
        accessor_id: String,
        expires_at: DateTime<Utc>,
    },
    /// A bearer token (`Authorization: Bearer`).
    Bearer {
        token: String,
        expires_at: DateTime<Utc>,
    },
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Nomad {
                accessor_id,
                expires_at,
                ..
            } => f
                .debug_struct("Nomad")
                .field("accessor_id", accessor_id)
                .field("expires_at", expires_at)
                .finish_non_exhaustive(),
            Credential::Bearer { expires_at, .. } => f
                .debug_struct("Bearer")
                .field("expires_at", expires_at)
                .finish_non_exhaustive(),
        }
    }
}

impl Credential {
    pub fn expires_at(&self) -> DateTime<Utc> {
        match self {
            Credential::Nomad { expires_at, .. } | Credential::Bearer { expires_at, .. } => {
                *expires_at
            }
        }
    }

    /// The header the API authenticates with: name and value.
    pub fn header(&self) -> (&'static str, String) {
        match self {
            Credential::Nomad { secret_id, .. } => ("X-Nomad-Token", secret_id.clone()),
            Credential::Bearer { token, .. } => ("Authorization", format!("Bearer {token}")),
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "PascalCase")]
struct NomadLoginRequest<'a> {
    auth_method_name: &'a str,
    login_token: &'a str,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NomadAclToken {
    #[serde(rename = "AccessorID")]
    accessor_id: String,
    #[serde(rename = "SecretID")]
    secret_id: String,
    #[serde(default)]
    expiration_time: Option<DateTime<Utc>>,
}

impl Login {
    /// Exchanges `identity` for a credential.
    pub async fn login(
        &self,
        http: &reqwest::Client,
        identity: &IssuedIdentity,
    ) -> buckle_core::Result<Credential> {
        match self {
            Login::Kubernetes => Ok(Credential::Bearer {
                token: identity.token.clone(),
                expires_at: identity.expires_at,
            }),
            Login::Nomad { api, auth_method } => {
                let url = api.join("v1/acl/login").map_err(|e| {
                    buckle_core::error::wrap_user(
                        e,
                        format!("The Nomad API address {api} is not a base URL."),
                        &["Set the cluster's `api` to a URL like http://127.0.0.1:4646."],
                    )
                })?;
                let failed = |e: reqwest::Error| {
                    buckle_core::error::wrap_user(
                        e,
                        format!(
                            "Could not log in to Nomad at {api} for {}.",
                            identity.audience
                        ),
                        &[
                            "Check that the Nomad API is reachable from this host.",
                            "Check the cluster's trust setup: a `jwt` auth method trusting Buckle's keys and a binding rule for this purpose (see the server's identity docs).",
                        ],
                    )
                };
                let token: NomadAclToken = http
                    .post(url)
                    .json(&NomadLoginRequest {
                        auth_method_name: auth_method,
                        login_token: &identity.token,
                    })
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                    .map_err(failed)?
                    .json()
                    .await
                    .map_err(failed)?;
                tracing::debug!(accessor = %token.accessor_id, audience = %identity.audience, "logged in to Nomad");
                Ok(Credential::Nomad {
                    secret_id: token.secret_id,
                    accessor_id: token.accessor_id,
                    // A token without an expiry still outlives its JWT's use.
                    expires_at: token.expiration_time.unwrap_or(identity.expires_at),
                })
            }
        }
    }
}

/// A credential is renewed when less than this is left of it.
pub const RENEW_BEFORE: chrono::Duration = chrono::Duration::seconds(60);

/// Credentials per target, purpose, rollout and login, renewed on demand.
pub struct Credentials {
    transport: Arc<dyn Transport>,
    http: reqwest::Client,
    cache: Mutex<HashMap<(IdentityRequest, Login), Credential>>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").finish_non_exhaustive()
    }
}

impl Credentials {
    pub fn new(transport: Arc<dyn Transport>, http: reqwest::Client) -> Self {
        Self {
            transport,
            http,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// A credential for `request` through `login`: the cached one while it
    /// has more than [`RENEW_BEFORE`] left, else a fresh identity and login.
    pub async fn get(
        &self,
        request: &IdentityRequest,
        login: &Login,
    ) -> buckle_core::Result<Credential> {
        let http = self.http.clone();
        self.get_with(request, login, |identity| async move {
            login.login(&http, &identity).await
        })
        .await
    }

    /// [`Credentials::get`], exchanging a fresh identity through `exchange`
    /// rather than [`Login::login`]: an executor logs in to Nomad over its
    /// own API client (which also reaches a Unix socket). `login` still keys
    /// the cache.
    pub async fn get_with<F, Fut>(
        &self,
        request: &IdentityRequest,
        login: &Login,
        exchange: F,
    ) -> buckle_core::Result<Credential>
    where
        F: FnOnce(IssuedIdentity) -> Fut,
        Fut: std::future::Future<Output = buckle_core::Result<Credential>>,
    {
        let key = (request.clone(), login.clone());
        let mut cache = self.cache.lock().await;
        let now = Utc::now();
        cache.retain(|_, c| c.expires_at() - RENEW_BEFORE > now);
        if let Some(c) = cache.get(&key) {
            return Ok(c.clone());
        }
        let identity = self.transport.issue_identity(request).await?;
        let credential = exchange(identity).await?;
        cache.insert(key, credential.clone());
        Ok(credential)
    }

    /// Forgets every cached credential for `target` (after the cluster
    /// rejected one, say).
    pub async fn forget(&self, target: &str) {
        self.cache
            .lock()
            .await
            .retain(|(r, _), _| r.target != target);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{Hello, RevisionSource, Session, WireSecret};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Issues `jwt-<n>` tokens that live `ttl`.
    struct Issuing {
        issued: AtomicUsize,
        ttl: chrono::Duration,
    }

    #[async_trait::async_trait]
    impl Transport for Issuing {
        async fn connect(&self, _hello: Hello) -> buckle_core::Result<Session> {
            unimplemented!()
        }
        async fn fetch_revision(
            &self,
            _revision: &RevisionId,
            _tree_digest: Option<&str>,
        ) -> buckle_core::Result<RevisionSource> {
            unimplemented!()
        }
        async fn fetch_secrets(
            &self,
            _revision: &RevisionId,
            _names: &[String],
        ) -> buckle_core::Result<Vec<WireSecret>> {
            unimplemented!()
        }
        async fn issue_identity(
            &self,
            request: &IdentityRequest,
        ) -> buckle_core::Result<IssuedIdentity> {
            let n = self.issued.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(IssuedIdentity {
                token: format!("jwt-{n}"),
                expires_at: Utc::now() + self.ttl,
                audience: request.target.clone(),
                id: n.to_string(),
            })
        }
    }

    fn credentials(ttl: chrono::Duration) -> (Arc<Issuing>, Credentials) {
        let transport = Arc::new(Issuing {
            issued: AtomicUsize::new(0),
            ttl,
        });
        let creds = Credentials::new(transport.clone(), reqwest::Client::new());
        (transport, creds)
    }

    fn request(purpose: Purpose) -> IdentityRequest {
        IdentityRequest {
            target: "home-nomad".into(),
            purpose,
            revision: None,
            rollout: None,
        }
    }

    async fn nomad(expiry: Option<&str>) -> MockServer {
        let server = MockServer::start().await;
        let mut token = serde_json::json!({
            "AccessorID": "acc-1",
            "SecretID": "secret-1",
            "Name": "buckle",
            "Type": "client",
        });
        if let Some(e) = expiry {
            token["ExpirationTime"] = e.into();
        }
        Mock::given(method("POST"))
            .and(path("/v1/acl/login"))
            .and(body_json(serde_json::json!({
                "AuthMethodName": "buckle",
                "LoginToken": "jwt-1",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(token))
            .mount(&server)
            .await;
        server
    }

    fn nomad_login(server: &MockServer) -> Login {
        Login::Nomad {
            api: url::Url::parse(&server.uri()).unwrap(),
            auth_method: Login::DEFAULT_NOMAD_AUTH_METHOD.into(),
        }
    }

    #[tokio::test]
    async fn nomad_exchanges_the_jwt_for_an_acl_token() {
        let server = nomad(Some("2030-01-01T00:00:00.123456789Z")).await;
        let (_, creds) = credentials(chrono::Duration::minutes(15));
        let c = creds
            .get(&request(Purpose::Deploy), &nomad_login(&server))
            .await
            .expect("logged in");
        assert_eq!(c.header(), ("X-Nomad-Token", "secret-1".to_string()));
        assert_eq!(c.expires_at().date_naive().to_string(), "2030-01-01");
        assert!(
            !format!("{c:?}").contains("secret-1"),
            "Debug redacts the secret"
        );
    }

    #[tokio::test]
    async fn a_nomad_token_without_an_expiry_lives_as_long_as_its_jwt() {
        let server = nomad(None).await;
        let (_, creds) = credentials(chrono::Duration::minutes(15));
        let c = creds
            .get(&request(Purpose::Read), &nomad_login(&server))
            .await
            .unwrap();
        assert!(c.expires_at() < Utc::now() + chrono::Duration::minutes(16));
    }

    #[tokio::test]
    async fn a_refused_login_explains_the_trust_setup() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("no binding rules"))
            .mount(&server)
            .await;
        let (_, creds) = credentials(chrono::Duration::minutes(15));
        let err = creds
            .get(&request(Purpose::Deploy), &nomad_login(&server))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("log in to Nomad"), "{err}");
    }

    #[tokio::test]
    async fn kubernetes_presents_the_jwt_as_a_bearer_token() {
        let (_, creds) = credentials(chrono::Duration::minutes(15));
        let c = creds
            .get(&request(Purpose::Read), &Login::Kubernetes)
            .await
            .unwrap();
        assert_eq!(c.header(), ("Authorization", "Bearer jwt-1".to_string()));
    }

    #[tokio::test]
    async fn credentials_are_reused_until_they_near_expiry() {
        let (issuing, creds) = credentials(chrono::Duration::minutes(15));
        for _ in 0..3 {
            creds
                .get(&request(Purpose::Read), &Login::Kubernetes)
                .await
                .unwrap();
        }
        assert_eq!(
            issuing.issued.load(Ordering::SeqCst),
            1,
            "one identity serves them all"
        );
        creds
            .get(&request(Purpose::Deploy), &Login::Kubernetes)
            .await
            .unwrap();
        assert_eq!(
            issuing.issued.load(Ordering::SeqCst),
            2,
            "another purpose is another identity"
        );
        creds.forget("home-nomad").await;
        creds
            .get(&request(Purpose::Read), &Login::Kubernetes)
            .await
            .unwrap();
        assert_eq!(
            issuing.issued.load(Ordering::SeqCst),
            3,
            "forgotten credentials are re-issued"
        );
    }

    #[tokio::test]
    async fn the_grpc_transport_carries_the_request_and_the_refusal() {
        let (fleet, _sessions) = crate::tests::fake_server::FakeFleet::new();
        let dir = tempfile::tempdir().unwrap();
        let transport = crate::transport::grpc::GrpcTransport::from_channel(
            fleet.in_memory_channel(),
            crate::store::Store::new(dir.path()),
            Some("boot".into()),
        );
        let issued = transport
            .issue_identity(&IdentityRequest {
                target: "home-nomad".into(),
                purpose: Purpose::Drain,
                revision: Some(RevisionId::new("abc")),
                rollout: Some("r-1".into()),
            })
            .await
            .expect("issued");
        assert_eq!(issued.token, "home-nomad/Drain/abc/r-1");
        assert_eq!(issued.expires_at.timestamp(), 1_893_456_000);

        let err = transport
            .issue_identity(&IdentityRequest {
                target: "denied".into(),
                ..request(Purpose::Read)
            })
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("did not issue an identity for denied"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_credential_close_to_expiry_is_renewed() {
        let (issuing, creds) = credentials(RENEW_BEFORE / 2);
        for _ in 0..2 {
            creds
                .get(&request(Purpose::Read), &Login::Kubernetes)
                .await
                .unwrap();
        }
        assert_eq!(issuing.issued.load(Ordering::SeqCst), 2);
    }
}
