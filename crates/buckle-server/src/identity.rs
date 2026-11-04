//! Buckle as an identity issuer: the server signs short-lived JWTs that a
//! host agent exchanges for a cluster credential (a Nomad ACL token through a
//! `jwt` auth method, or a bearer token a Kubernetes JWT authenticator
//! accepts), and publishes the keys clusters verify them with.
//!
//! The server signs and audits; it holds nothing that can reach a cluster.
//!
//! * Keys: one ES256 (P-256) signing key at `data_dir/identity/signing.key`,
//!   generated on first start. A `retired.key` beside it is still published
//!   (never used to sign), so rotation is: move `signing.key` to
//!   `retired.key`, restart, and delete `retired.key` after one TTL.
//! * Documents: a JWKS, an OIDC discovery document and the public keys as
//!   PEM (Nomad's `JWTValidationPubKeys` takes them without any URL).
//! * Tokens: `iss` the configured issuer, `sub` the host, `aud` the target,
//!   with `purpose`, `target`, `cluster` and `rollout` claims for binding
//!   rules and RBAC.

use crate::config::IdentityConfig;
use base64::Engine as _;
use buckle_core::Result;
use buckle_core::error::HumanizableResult;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::Digest as _;
use std::path::Path;
use std::time::Duration;

/// The only algorithm the issuer signs with; trust setups must allow it
/// (Nomad: `JWTSupportedAlgs = ["ES256"]`).
pub const ALGORITHM: &str = "ES256";

/// What a credential may be used for, which binding rules map to policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// Facts, plans and health: reading the cluster.
    Read,
    /// Applying workloads.
    Deploy,
    /// Draining (and marking ineligible) the requesting host's own node.
    Drain,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Read => "read",
            Purpose::Deploy => "deploy",
            Purpose::Drain => "drain",
        }
    }
}

impl std::fmt::Display for Purpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who a token is for, already authorised by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    /// The host the token speaks for (`sub`).
    pub host: String,
    /// The cluster target it is for (`aud`).
    pub target: String,
    /// The cluster the target is on: the target itself unless it is a
    /// scoped target on a shared cluster.
    pub cluster: String,
    pub purpose: Purpose,
    /// The rollout the credential serves, when one does.
    pub rollout: Option<String>,
}

/// How a cluster target lets hosts obtain credentials for it: the part of
/// its declaration identity issuance reads.
#[derive(Debug, Clone)]
pub struct ClusterTrust {
    /// The target's name (the token's audience).
    pub target: String,
    /// The cluster it is on (itself unless it is a scoped target).
    pub cluster: String,
    /// The cluster's member hosts: who may drain their own node.
    pub nodes: Option<buckle_core::Filter>,
    /// Who may read and deploy (defaults to `nodes`).
    pub executors: Option<buckle_core::Filter>,
    /// Whether the target trusts Buckle-issued tokens (`identity:
    /// buckle_jwt`); other modes never get one.
    pub buckle_jwt: bool,
}

impl ClusterTrust {
    /// Why `ctx`'s host may not have a `purpose` credential, or `Ok`.
    pub fn authorise(
        &self,
        ctx: &buckle_core::selectors::HostContext,
        purpose: Purpose,
    ) -> std::result::Result<(), String> {
        if !self.buckle_jwt {
            return Err(format!(
                "{} does not use Buckle-issued identities (identity is not buckle_jwt)",
                self.target
            ));
        }
        let (selector, role) = match purpose {
            Purpose::Drain => (self.nodes.as_ref(), "a node of"),
            Purpose::Read | Purpose::Deploy => (
                self.executors.as_ref().or(self.nodes.as_ref()),
                "an executor for",
            ),
        };
        match selector {
            Some(f) if ctx.matches(f) => Ok(()),
            _ => Err(format!("{} is not {role} {}", ctx.host.id, self.target)),
        }
    }
}

/// A signed token.
#[derive(Debug, Clone)]
pub struct Issued {
    /// The compact JWS.
    pub token: String,
    /// `jti`, recorded in the audit log.
    pub id: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// The claims of an issued token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub iat: i64,
    pub nbf: i64,
    pub exp: i64,
    pub jti: String,
    pub purpose: Purpose,
    pub host: String,
    pub target: String,
    pub cluster: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<String>,
}

/// A public key as the JWKS and PEM documents publish it.
#[derive(Debug, Clone)]
struct PublicKey {
    kid: String,
    x: String,
    y: String,
    pem: String,
}

impl PublicKey {
    fn of(pair: &rcgen::KeyPair) -> Result<Self> {
        let raw = pair.public_key_raw();
        // An uncompressed SEC1 point: 0x04 || X (32 bytes) || Y (32 bytes).
        if raw.len() != 65 || raw[0] != 4 {
            return Err(buckle_core::error::system(
                "The identity signing key is not an uncompressed P-256 point.",
                &["Delete the key under data_dir/identity to generate a new one."],
            ));
        }
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let (x, y) = (b64(&raw[1..33]), b64(&raw[33..]));
        // RFC 7638 thumbprint: members in lexicographic order, no whitespace.
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let kid = b64(&sha2::Sha256::digest(canonical.as_bytes()));
        Ok(Self {
            kid,
            x,
            y,
            pem: pair.public_key_pem(),
        })
    }

    fn jwk(&self) -> serde_json::Value {
        serde_json::json!({
            "kty": "EC",
            "crv": "P-256",
            "x": self.x,
            "y": self.y,
            "kid": self.kid,
            "use": "sig",
            "alg": ALGORITHM,
        })
    }
}

/// The signing key and the documents that publish it.
pub struct Issuer {
    issuer: String,
    ttl: Duration,
    encoding: jsonwebtoken::EncodingKey,
    current: PublicKey,
    retired: Option<PublicKey>,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Issuer")
            .field("issuer", &self.issuer)
            .field("kid", &self.current.kid)
            .finish_non_exhaustive()
    }
}

const SIGNING_KEY: &str = "signing.key";
const RETIRED_KEY: &str = "retired.key";

fn read_key(path: &Path) -> Result<rcgen::KeyPair> {
    let pem = std::fs::read_to_string(path).to_human_error()?;
    rcgen::KeyPair::from_pem(&pem).map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            format!("The identity key at {} is not a PEM key.", path.display()),
            &["Delete the file to generate a new key (clusters must then trust the new key)."],
        )
    })
}

fn create_key(path: &Path) -> Result<()> {
    let pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).map_err(|e| {
        buckle_core::error::wrap_system(
            e,
            "Could not generate the identity signing key.",
            &["Report this issue."],
        )
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).to_human_error()?;
    }
    std::fs::write(path, pair.serialize_pem()).to_human_error()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).to_human_error()?;
    }
    tracing::info!(path = %path.display(), "generated the identity signing key");
    Ok(())
}

impl Issuer {
    /// Loads the keys under `dir` (creating the signing key on first start).
    pub fn load(dir: &Path, config: &IdentityConfig, issuer: String) -> Result<Self> {
        let signing = dir.join(SIGNING_KEY);
        if !signing.is_file() {
            create_key(&signing)?;
        }
        let pair = read_key(&signing)?;
        if pair.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
            return Err(buckle_core::error::user(
                format!(
                    "The identity key at {} is not a P-256 key.",
                    signing.display()
                ),
                &["Delete the file to generate a new key."],
            ));
        }
        let encoding = jsonwebtoken::EncodingKey::from_ec_der(&pair.serialize_der());
        let retired_path = dir.join(RETIRED_KEY);
        let retired = if retired_path.is_file() {
            Some(PublicKey::of(&read_key(&retired_path)?)?)
        } else {
            None
        };
        Ok(Self {
            issuer,
            ttl: config.ttl,
            encoding,
            current: PublicKey::of(&pair)?,
            retired,
        })
    }

    /// The `iss` of every token, and the base of the discovery URLs.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The key id tokens are signed with.
    pub fn kid(&self) -> &str {
        &self.current.kid
    }

    /// How long a token lives.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Signs a token for `grant`, valid from `now` for the configured TTL.
    pub fn issue(&self, grant: &Grant, now: DateTime<Utc>) -> Result<Issued> {
        let ttl = chrono::Duration::from_std(self.ttl).unwrap_or(chrono::Duration::minutes(15));
        let expires_at = now + ttl;
        let id = uuid::Uuid::new_v4().to_string();
        let claims = Claims {
            iss: self.issuer.clone(),
            sub: grant.host.clone(),
            aud: grant.target.clone(),
            iat: now.timestamp(),
            nbf: now.timestamp(),
            exp: expires_at.timestamp(),
            jti: id.clone(),
            purpose: grant.purpose,
            host: grant.host.clone(),
            target: grant.target.clone(),
            cluster: grant.cluster.clone(),
            rollout: grant.rollout.clone(),
        };
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
        header.kid = Some(self.current.kid.clone());
        let token = jsonwebtoken::encode(&header, &claims, &self.encoding).map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "Could not sign an identity token.",
                &["Report this issue."],
            )
        })?;
        Ok(Issued {
            token,
            id,
            issued_at: now,
            expires_at,
        })
    }

    fn keys(&self) -> impl Iterator<Item = &PublicKey> {
        std::iter::once(&self.current).chain(self.retired.as_ref())
    }

    /// `/.well-known/jwks.json`.
    pub fn jwks(&self) -> serde_json::Value {
        serde_json::json!({ "keys": self.keys().map(PublicKey::jwk).collect::<Vec<_>>() })
    }

    /// `/.well-known/openid-configuration`: the subset a JWT authenticator
    /// (Kubernetes, Nomad's `OIDCDiscoveryURL`) reads.
    pub fn discovery(&self) -> serde_json::Value {
        let base = self.issuer.trim_end_matches('/');
        serde_json::json!({
            "issuer": self.issuer,
            "jwks_uri": format!("{base}/.well-known/jwks.json"),
            "response_types_supported": ["id_token"],
            "subject_types_supported": ["public"],
            "id_token_signing_alg_values_supported": [ALGORITHM],
            "claims_supported": [
                "iss", "sub", "aud", "iat", "nbf", "exp", "jti",
                "purpose", "host", "target", "cluster", "rollout",
            ],
        })
    }

    /// `/.well-known/buckle-identity.pem`: every published public key, for
    /// Nomad's `JWTValidationPubKeys`.
    pub fn public_pem(&self) -> String {
        self.keys()
            .map(|k| k.pem.as_str())
            .collect::<Vec<_>>()
            .join("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn config() -> IdentityConfig {
        IdentityConfig::default()
    }

    fn grant(purpose: Purpose) -> Grant {
        Grant {
            host: "pi-1".into(),
            target: "home-nomad".into(),
            cluster: "home-nomad".into(),
            purpose,
            rollout: Some("r-1".into()),
        }
    }

    /// Verifies `token` the way a cluster does: against the published JWKS.
    fn verify(
        issuer: &Issuer,
        token: &str,
        audience: &str,
    ) -> jsonwebtoken::errors::Result<Claims> {
        let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(issuer.jwks()).unwrap();
        let kid = jsonwebtoken::decode_header(token)?.kid.unwrap();
        let jwk = jwks.find(&kid).expect("the token's kid is published");
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::ES256);
        validation.set_audience(&[audience]);
        validation.set_issuer(&[issuer.issuer()]);
        jsonwebtoken::decode::<Claims>(
            token,
            &jsonwebtoken::DecodingKey::from_jwk(jwk)?,
            &validation,
        )
        .map(|d| d.claims)
    }

    #[rstest]
    #[case(Purpose::Read)]
    #[case(Purpose::Deploy)]
    #[case(Purpose::Drain)]
    fn a_token_verifies_against_the_published_keys(#[case] purpose: Purpose) {
        let dir = tempfile::tempdir().unwrap();
        let issuer = Issuer::load(dir.path(), &config(), "https://buckle.example".into()).unwrap();
        let issued = issuer.issue(&grant(purpose), Utc::now()).unwrap();
        let claims = verify(&issuer, &issued.token, "home-nomad").expect("valid");
        assert_eq!(claims.sub, "pi-1");
        assert_eq!(claims.purpose, purpose);
        assert_eq!(claims.rollout.as_deref(), Some("r-1"));
        assert_eq!(claims.jti, issued.id);
        assert_eq!(claims.exp - claims.iat, 15 * 60, "the default TTL");
    }

    #[test]
    fn a_token_for_another_target_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = Issuer::load(dir.path(), &config(), "buckle".into()).unwrap();
        let issued = issuer.issue(&grant(Purpose::Deploy), Utc::now()).unwrap();
        assert!(verify(&issuer, &issued.token, "lab-k3s").is_err());
    }

    #[test]
    fn an_expired_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let issuer = Issuer::load(dir.path(), &config(), "buckle".into()).unwrap();
        let issued = issuer
            .issue(
                &grant(Purpose::Read),
                Utc::now() - chrono::Duration::hours(1),
            )
            .unwrap();
        assert!(verify(&issuer, &issued.token, "home-nomad").is_err());
    }

    #[test]
    fn the_key_survives_a_restart_and_rotation_publishes_both_keys() {
        let dir = tempfile::tempdir().unwrap();
        let first = Issuer::load(dir.path(), &config(), "buckle".into()).unwrap();
        let old = first.issue(&grant(Purpose::Read), Utc::now()).unwrap();
        let again = Issuer::load(dir.path(), &config(), "buckle".into()).unwrap();
        assert_eq!(again.kid(), first.kid(), "the key is reused");

        std::fs::rename(dir.path().join(SIGNING_KEY), dir.path().join(RETIRED_KEY)).unwrap();
        let rotated = Issuer::load(dir.path(), &config(), "buckle".into()).unwrap();
        assert_ne!(rotated.kid(), first.kid(), "a new key signs");
        assert_eq!(rotated.jwks()["keys"].as_array().unwrap().len(), 2);
        assert_eq!(rotated.public_pem().matches("BEGIN PUBLIC KEY").count(), 2);
        verify(&rotated, &old.token, "home-nomad")
            .expect("tokens from the retired key still verify");
        let new = rotated.issue(&grant(Purpose::Read), Utc::now()).unwrap();
        verify(&rotated, &new.token, "home-nomad").expect("new tokens verify");
    }

    #[test]
    fn discovery_points_at_the_jwks_under_the_issuer() {
        let dir = tempfile::tempdir().unwrap();
        let issuer =
            Issuer::load(dir.path(), &config(), "https://nas.example.ts.net/".into()).unwrap();
        let doc = issuer.discovery();
        assert_eq!(doc["issuer"], "https://nas.example.ts.net/");
        assert_eq!(
            doc["jwks_uri"],
            "https://nas.example.ts.net/.well-known/jwks.json"
        );
        assert_eq!(doc["id_token_signing_alg_values_supported"][0], ALGORITHM);
    }

    fn trust(nodes: Option<&str>, executors: Option<&str>, buckle_jwt: bool) -> ClusterTrust {
        let f = |s: Option<&str>| s.map(|s| buckle_core::Filter::new(s).unwrap());
        ClusterTrust {
            target: "home-nomad".into(),
            cluster: "home-nomad".into(),
            nodes: f(nodes),
            executors: f(executors),
            buckle_jwt,
        }
    }

    fn host(id: &str) -> buckle_core::selectors::HostContext {
        buckle_core::selectors::HostContext::new(buckle_core::selectors::HostIdentity {
            id: id.into(),
            labels: Default::default(),
        })
    }

    const NODES: &str = r#"host.id startswith "pi-""#;
    const SERVERS: &str = r#"host.id == "pi-1""#;

    #[rstest]
    #[case::executor_reads("pi-1", Purpose::Read, Some(SERVERS), true)]
    #[case::executor_deploys("pi-1", Purpose::Deploy, Some(SERVERS), true)]
    #[case::node_cannot_deploy("pi-2", Purpose::Deploy, Some(SERVERS), false)]
    #[case::node_drains_itself("pi-2", Purpose::Drain, Some(SERVERS), true)]
    #[case::stranger_cannot_drain("nas", Purpose::Drain, Some(SERVERS), false)]
    #[case::executors_default_to_nodes("pi-2", Purpose::Deploy, None, true)]
    fn grants_follow_the_selectors(
        #[case] id: &str,
        #[case] purpose: Purpose,
        #[case] executors: Option<&str>,
        #[case] granted: bool,
    ) {
        let t = trust(Some(NODES), executors, true);
        assert_eq!(t.authorise(&host(id), purpose).is_ok(), granted);
    }

    #[test]
    fn a_target_without_buckle_jwt_grants_nothing() {
        let t = trust(Some(NODES), None, false);
        let err = t.authorise(&host("pi-1"), Purpose::Read).unwrap_err();
        assert!(err.contains("buckle_jwt"), "{err}");
    }

    #[test]
    fn a_corrupt_key_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SIGNING_KEY), "not a key").unwrap();
        let err = Issuer::load(dir.path(), &config(), "buckle".into()).unwrap_err();
        assert!(err.to_string().contains("not a PEM key"), "{err}");
    }
}
