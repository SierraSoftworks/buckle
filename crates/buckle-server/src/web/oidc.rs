//! The OpenID Connect client for operator sign-in (design §12, analytics
//! variant), hardened along the lines of rustak's implementation:
//!
//! * discovery is cached for an hour and the key set for a day; a token
//!   naming an unknown `kid` refetches the key set (rotation), at most once
//!   a minute so forged `kid`s cannot hammer the provider; the discovery
//!   document's issuer must be the configured endpoint;
//! * only asymmetric algorithms are accepted, `kid` is required, `exp`,
//!   `aud` and `iss` are required, a multi-audience token must name us as
//!   `azp`, and the nonce of the flow that asked is checked;
//! * PKCE S256 binds the code to this server's verifier.
//!
//! The id token itself is the session: it lives in an HttpOnly cookie and is
//! re-validated on every request.

use crate::config::OidcConfig;
use base64::Engine as _;
use buckle_core::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
const JWKS_TTL: Duration = Duration::from_secs(24 * 3600);
/// The least time between two key-set refetches forced by an unknown `kid`.
const JWKS_REFETCH_INTERVAL: Duration = Duration::from_secs(60);
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Claims that carry protocol meaning rather than anything about the person;
/// hidden from ACL expressions.
pub const PROTOCOL_CLAIMS: &[&str] = &[
    "exp",
    "nbf",
    "iat",
    "iss",
    "aud",
    "jti",
    "nonce",
    "at_hash",
    "c_hash",
    "azp",
    "auth_time",
];

const ACCEPTED_ALGORITHMS: &[jsonwebtoken::Algorithm] = &[
    jsonwebtoken::Algorithm::RS256,
    jsonwebtoken::Algorithm::RS384,
    jsonwebtoken::Algorithm::RS512,
    jsonwebtoken::Algorithm::PS256,
    jsonwebtoken::Algorithm::PS384,
    jsonwebtoken::Algorithm::PS512,
    jsonwebtoken::Algorithm::ES256,
    jsonwebtoken::Algorithm::ES384,
    jsonwebtoken::Algorithm::EdDSA,
];

const ADVICE_PROVIDER: &[&str] = &[
    "Check that web.oidc.endpoint names a working OpenID Connect provider.",
    "Check that the server can reach that provider over the network.",
];
const ADVICE_SIGN_IN: &[&str] = &["Sign in again."];

/// The part of the discovery document Buckle uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discovery {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
}

/// Tokens returned by the token endpoint.
#[derive(Clone, Deserialize)]
pub struct TokenSet {
    pub id_token: Option<String>,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("id_token", &self.id_token.as_ref().map(|_| "***"))
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// A PKCE verifier and its S256 challenge.
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"***")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// 32 random bytes, base64url.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    B64.encode(bytes)
}

/// SHA-256, base64url.
pub fn sha256_b64(input: &str) -> String {
    B64.encode(Sha256::digest(input.as_bytes()))
}

/// Mints a fresh PKCE pair.
pub fn pkce() -> Pkce {
    let verifier = random_token();
    let challenge = sha256_b64(&verifier);
    Pkce {
        verifier,
        challenge,
    }
}

struct Cached<T> {
    value: T,
    at: Instant,
}

/// An OIDC relying party with cached discovery and keys.
pub struct OidcClient {
    config: OidcConfig,
    http: reqwest::Client,
    discovery: Mutex<Option<Cached<Discovery>>>,
    jwks: Mutex<Option<Cached<jsonwebtoken::jwk::JwkSet>>>,
    /// When an unknown `kid` last forced a key-set refetch.
    forced_refetch: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for OidcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcClient")
            .field("endpoint", &self.config.endpoint)
            .field("client_id", &self.config.client_id)
            .finish_non_exhaustive()
    }
}

impl OidcClient {
    /// A client for `config`.
    pub fn new(config: OidcConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap_or_default(),
            discovery: Mutex::new(None),
            jwks: Mutex::new(None),
            forced_refetch: Mutex::new(None),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &OidcConfig {
        &self.config
    }

    async fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, what: &str) -> Result<T> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| {
                buckle_core::error::wrap_system(
                    e,
                    format!("The identity provider could not be reached for its {what}."),
                    ADVICE_PROVIDER,
                )
            })?
            .error_for_status()
            .map_err(|e| {
                buckle_core::error::wrap_system(
                    e,
                    format!("The identity provider refused to serve its {what}."),
                    ADVICE_PROVIDER,
                )
            })?;
        response.json().await.map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                format!("The identity provider's {what} could not be read."),
                ADVICE_PROVIDER,
            )
        })
    }

    /// The discovery document (cached for an hour).
    pub async fn discovery(&self) -> Result<Discovery> {
        if let Some(d) = self.discovery.lock().ok().and_then(|g| {
            g.as_ref()
                .filter(|c| c.at.elapsed() < DISCOVERY_TTL)
                .map(|c| c.value.clone())
        }) {
            return Ok(d);
        }
        let url = format!(
            "{}/.well-known/openid-configuration",
            self.config.endpoint.trim_end_matches('/')
        );
        let d: Discovery = self.get_json(&url, "discovery document").await?;
        // OpenID Connect Discovery §4.3: the issuer must be the URL the
        // document was fetched for, or a provider (or anything between us
        // and it) could make us accept another issuer's tokens.
        if d.issuer.trim_end_matches('/') != self.config.endpoint.trim_end_matches('/') {
            return Err(buckle_core::error::system(
                format!(
                    "The identity provider's discovery document names the issuer '{}', not the configured '{}'.",
                    d.issuer, self.config.endpoint
                ),
                &["Set web.oidc.endpoint to the provider's exact issuer URL."],
            ));
        }
        if let Ok(mut g) = self.discovery.lock() {
            *g = Some(Cached {
                value: d.clone(),
                at: Instant::now(),
            });
        }
        Ok(d)
    }

    async fn jwks(&self, discovery: &Discovery, force: bool) -> Result<jsonwebtoken::jwk::JwkSet> {
        if !force
            && let Some(k) = self.jwks.lock().ok().and_then(|g| {
                g.as_ref()
                    .filter(|c| c.at.elapsed() < JWKS_TTL)
                    .map(|c| c.value.clone())
            })
        {
            return Ok(k);
        }
        let keys: jsonwebtoken::jwk::JwkSet =
            self.get_json(&discovery.jwks_uri, "signing keys").await?;
        if let Ok(mut g) = self.jwks.lock() {
            *g = Some(Cached {
                value: keys.clone(),
                at: Instant::now(),
            });
        }
        Ok(keys)
    }

    /// Whether an unknown `kid` may force a key-set refetch now (at most
    /// once per [`JWKS_REFETCH_INTERVAL`]); records the refetch when it may.
    fn may_refetch_keys(&self) -> bool {
        let mut last = self
            .forced_refetch
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        if last.is_some_and(|t| now.duration_since(t) < JWKS_REFETCH_INTERVAL) {
            return false;
        }
        *last = Some(now);
        true
    }

    /// The URL to send the browser to.
    pub async fn authorize_url(
        &self,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        challenge: &str,
    ) -> Result<String> {
        let d = self.discovery().await?;
        let mut url = url::Url::parse(&d.authorization_endpoint).map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "The identity provider's authorization endpoint is not a URL.",
                ADVICE_PROVIDER,
            )
        })?;
        let scopes = if self.config.scopes.iter().any(|s| s == "openid") {
            self.config.scopes.join(" ")
        } else {
            let mut s = vec!["openid".to_string()];
            s.extend(self.config.scopes.iter().cloned());
            s.join(" ")
        };
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("scope", &scopes)
            .append_pair("state", state)
            .append_pair("nonce", nonce)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.to_string())
    }

    async fn token_request(&self, form: &[(&str, &str)]) -> Result<TokenSet> {
        let d = self.discovery().await?;
        let response = self
            .http
            .post(&d.token_endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(
                url::form_urlencoded::Serializer::new(String::new())
                    .extend_pairs(form.iter())
                    .finish(),
            )
            .send()
            .await
            .map_err(|e| {
                buckle_core::error::wrap_system(
                    e,
                    "The identity provider's token endpoint could not be reached.",
                    ADVICE_PROVIDER,
                )
            })?;
        if !response.status().is_success() {
            return Err(buckle_core::error::user(
                format!(
                    "The identity provider refused the token request ({}).",
                    response.status()
                ),
                ADVICE_SIGN_IN,
            ));
        }
        response.json().await.map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "The identity provider's token response could not be read.",
                ADVICE_PROVIDER,
            )
        })
    }

    /// Redeems an authorization code.
    pub async fn exchange(
        &self,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenSet> {
        self.token_request(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &self.config.client_id),
            ("client_secret", &self.config.client_secret),
            ("code_verifier", verifier),
        ])
        .await
    }

    /// Uses a refresh token.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        self.token_request(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &self.config.client_id),
            ("client_secret", &self.config.client_secret),
        ])
        .await
    }

    /// Verifies an id token and returns its claims. `nonce` is checked when given.
    pub async fn validate(
        &self,
        token: &str,
        nonce: Option<&str>,
    ) -> Result<serde_json::Map<String, serde_json::Value>> {
        let d = self.discovery().await?;
        let mut keys = self.jwks(&d, false).await?;
        if let Some(kid) = jsonwebtoken::decode_header(token).ok().and_then(|h| h.kid)
            && keys.find(&kid).is_none()
            && self.may_refetch_keys()
        {
            keys = self.jwks(&d, true).await?;
        }
        let claims = verify(&self.config.client_id, &d.issuer, &keys, token)?;
        if let Some(expected) = nonce
            && claims.get("nonce").and_then(|v| v.as_str()) != Some(expected)
        {
            return Err(buckle_core::error::user(
                "That sign-in did not match the one this browser started.",
                &["Start signing in again from this page."],
            ));
        }
        Ok(claims)
    }
}

/// Signature and claim checks against a key set (no network).
pub fn verify(
    client_id: &str,
    issuer: &str,
    keys: &jsonwebtoken::jwk::JwkSet,
    token: &str,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let refuse = || {
        buckle_core::error::user(
            "The sign-in token was not one Buckle accepts.",
            ADVICE_SIGN_IN,
        )
    };
    let header = jsonwebtoken::decode_header(token).map_err(|_| refuse())?;
    if !ACCEPTED_ALGORITHMS.contains(&header.alg) {
        return Err(refuse());
    }
    let kid = header.kid.ok_or_else(refuse)?;
    let jwk = keys.find(&kid).ok_or_else(refuse)?;
    let key = jsonwebtoken::DecodingKey::from_jwk(jwk).map_err(|_| refuse())?;
    let mut validation = jsonwebtoken::Validation::new(header.alg);
    validation.set_audience(&[client_id]);
    validation.set_issuer(&[issuer]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.set_required_spec_claims(&["exp", "aud", "iss"]);
    let data = jsonwebtoken::decode::<serde_json::Map<String, serde_json::Value>>(
        token,
        &key,
        &validation,
    )
    .map_err(|_| refuse())?;
    let several = data
        .claims
        .get("aud")
        .and_then(|a| a.as_array())
        .is_some_and(|a| a.len() > 1);
    if several && data.claims.get("azp").and_then(|v| v.as_str()) != Some(client_id) {
        return Err(refuse());
    }
    Ok(data.claims)
}

/// The claims ACL expressions may see.
pub fn filterable_claims(
    claims: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    claims
        .iter()
        .filter(|(k, _)| !PROTOCOL_CLAIMS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenges_are_the_sha256_of_the_verifier() {
        let p = pkce();
        assert_eq!(p.challenge, sha256_b64(&p.verifier));
        assert_eq!(p.verifier.len(), 43);
        assert!(!format!("{p:?}").contains(&p.verifier));
    }

    #[test]
    fn symmetric_and_keyless_tokens_are_refused() {
        let empty = jsonwebtoken::jwk::JwkSet { keys: vec![] };
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("k".into());
        let token = jsonwebtoken::encode(
            &header,
            &serde_json::json!({"sub": "a", "aud": "buckle", "iss": "x", "exp": 9_999_999_999u64}),
            &jsonwebtoken::EncodingKey::from_secret(b"public-modulus"),
        )
        .unwrap();
        assert!(verify("buckle", "x", &empty, &token).is_err());
        assert!(verify("buckle", "x", &empty, "rubbish").is_err());
    }

    #[test]
    fn protocol_claims_are_hidden_from_acls() {
        let claims = serde_json::json!({"sub": "a", "email": "a@x", "exp": 1, "nonce": "n", "groups": ["g"]});
        let visible = filterable_claims(claims.as_object().unwrap());
        assert!(
            visible.contains_key("email")
                && visible.contains_key("groups")
                && visible.contains_key("sub")
        );
        assert!(!visible.contains_key("exp") && !visible.contains_key("nonce"));
    }
}
