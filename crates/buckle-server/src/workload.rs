//! Workload identity for executors running inside a cluster
//! (`agents.auth: workload_identity`). The executor presents the token its
//! scheduler gave it (a Nomad workload identity, a projected ServiceAccount
//! token); the server accepts it when it is signed by one of the cluster's
//! keys, is meant for `buckle`, and names one of the cluster's `trust`
//! subjects.
//!
//! Keys come from three places: the `trust.jwks_url` the server fetches
//! (cached for an hour, refetched at most once a minute for an unknown
//! `kid`), the `trust.keys` pinned in the cluster's spec, and keys learned
//! from the executor. An executor relays the key set it reads inside the
//! cluster; the server learns it only once the token has verified against
//! a key it already trusted and the relayed set holds that same key, so a
//! relay can follow the cluster's key rotation but never introduce trust.

use buckle_api::{ClusterKind, ClusterTrust};
use jsonwebtoken::jwk::{Jwk, JwkSet};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The audience an executor's token must carry.
pub const AUDIENCE: &str = "buckle";

const JWKS_TTL: Duration = Duration::from_secs(3600);
const JWKS_REFETCH_INTERVAL: Duration = Duration::from_secs(60);

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

/// The kv key holding the key set learned for a cluster.
pub fn learned_key(cluster: &str) -> String {
    format!("trust/{cluster}/jwks")
}

/// A cluster that might accept a token.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub cluster: String,
    pub kind: ClusterKind,
    pub trust: ClusterTrust,
    /// The key set learned from its executors before (JWKS JSON).
    pub learned: Option<String>,
}

/// A token one cluster accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub cluster: String,
    pub subject: String,
    /// A key set to remember for the cluster, when the executor relayed one
    /// that holds the key that verified its token and differs from what
    /// was learned before.
    pub learn: Option<String>,
}

struct Cached {
    keys: JwkSet,
    at: Instant,
}

/// Checks workload identity tokens against clusters' trust.
pub struct WorkloadTrust {
    http: reqwest::Client,
    fetched: Mutex<HashMap<String, Cached>>,
    forced: Mutex<HashMap<String, Instant>>,
}

impl Default for WorkloadTrust {
    fn default() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            fetched: Mutex::new(HashMap::new()),
            forced: Mutex::new(HashMap::new()),
        }
    }
}

impl WorkloadTrust {
    /// The one cluster among `candidates` that accepts `token`. `relayed` is
    /// the key set the executor read inside its cluster.
    pub async fn verify(
        &self,
        candidates: &[Candidate],
        token: &str,
        relayed: Option<&str>,
    ) -> Result<Verified, String> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| "The workload identity token is not a JWT.".to_owned())?;
        if !ACCEPTED_ALGORITHMS.contains(&header.alg) {
            return Err(format!(
                "The workload identity token is signed with {:?}; only asymmetric signatures are accepted.",
                header.alg
            ));
        }
        let kid = header.kid.ok_or_else(|| {
            "The workload identity token names no signing key (`kid`).".to_owned()
        })?;
        let mut accepted = Vec::new();
        let mut refusals = Vec::new();
        for c in candidates {
            let Some(jwk) = self.key_for(c, &kid).await else {
                continue;
            };
            match check(c, &jwk, header.alg, token) {
                Ok(subject) => accepted.push((c, subject, jwk)),
                Err(why) => refusals.push(format!("{}: {why}", c.cluster)),
            }
        }
        match accepted.len() {
            0 if refusals.is_empty() => Err(
                "No cluster's trust holds the key that signed this workload identity token."
                    .to_owned(),
            ),
            0 => Err(format!(
                "The workload identity token was refused ({}).",
                refusals.join("; ")
            )),
            1 => {
                let (c, subject, jwk) = accepted.remove(0);
                Ok(Verified {
                    cluster: c.cluster.clone(),
                    subject,
                    learn: learnable(relayed, &kid, &jwk, c.learned.as_deref()),
                })
            }
            _ => Err(format!(
                "The workload identity token is accepted by several clusters ({}); give each cluster its own subjects.",
                accepted
                    .iter()
                    .map(|(c, ..)| c.cluster.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// The key `kid` among the cluster's pinned, learned and fetched keys.
    async fn key_for(&self, c: &Candidate, kid: &str) -> Option<Jwk> {
        let local = [c.trust.keys.as_deref(), c.learned.as_deref()];
        for set in local.into_iter().flatten() {
            if let Some(k) = parse(set).and_then(|s| s.find(kid).cloned()) {
                return Some(k);
            }
        }
        let url = c.trust.jwks_url.as_deref()?;
        if let Some(k) = self
            .fetch(url, false)
            .await
            .and_then(|s| s.find(kid).cloned())
        {
            return Some(k);
        }
        if self.may_refetch(url) {
            return self
                .fetch(url, true)
                .await
                .and_then(|s| s.find(kid).cloned());
        }
        None
    }

    async fn fetch(&self, url: &str, force: bool) -> Option<JwkSet> {
        if !force
            && let Ok(cache) = self.fetched.lock()
            && let Some(c) = cache.get(url)
            && c.at.elapsed() < JWKS_TTL
        {
            return Some(c.keys.clone());
        }
        let got = async {
            self.http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json::<JwkSet>()
                .await
        }
        .await;
        match got {
            Ok(keys) => {
                if let Ok(mut cache) = self.fetched.lock() {
                    cache.insert(
                        url.to_owned(),
                        Cached {
                            keys: keys.clone(),
                            at: Instant::now(),
                        },
                    );
                }
                Some(keys)
            }
            Err(err) => {
                tracing::warn!(%url, error = %err, "could not read a cluster's signing keys");
                // A stale set is better than none while the cluster is unreachable.
                self.fetched
                    .lock()
                    .ok()
                    .and_then(|c| c.get(url).map(|c| c.keys.clone()))
            }
        }
    }

    fn may_refetch(&self, url: &str) -> bool {
        let Ok(mut forced) = self.forced.lock() else {
            return false;
        };
        let now = Instant::now();
        match forced.get(url) {
            Some(at) if now.duration_since(*at) < JWKS_REFETCH_INTERVAL => false,
            _ => {
                forced.insert(url.to_owned(), now);
                true
            }
        }
    }
}

fn parse(set: &str) -> Option<JwkSet> {
    serde_json::from_str(set).ok()
}

/// The token's signature, audience, lifetime, issuer and subject against
/// one cluster; `Ok` names the subject it accepted.
fn check(
    c: &Candidate,
    jwk: &Jwk,
    alg: jsonwebtoken::Algorithm,
    token: &str,
) -> Result<String, String> {
    let key = jsonwebtoken::DecodingKey::from_jwk(jwk)
        .map_err(|_| "its signing key cannot be read".to_owned())?;
    let mut validation = jsonwebtoken::Validation::new(alg);
    validation.set_audience(&[AUDIENCE]);
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.set_required_spec_claims(&["exp", "aud"]);
    if let Some(iss) = &c.trust.issuer {
        validation.set_issuer(&[iss]);
    }
    let claims = jsonwebtoken::decode::<serde_json::Map<String, serde_json::Value>>(
        token,
        &key,
        &validation,
    )
    .map_err(|e| match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => "the token has expired".to_owned(),
        jsonwebtoken::errors::ErrorKind::InvalidAudience => {
            format!("the token is not meant for `{AUDIENCE}`")
        }
        jsonwebtoken::errors::ErrorKind::InvalidIssuer => "the token's issuer differs".to_owned(),
        _ => "the token's signature or claims do not verify".to_owned(),
    })?
    .claims;
    let subject =
        subject(c.kind, &claims).ok_or_else(|| "the token names no subject".to_owned())?;
    if c.trust.subjects(c.kind).contains(&subject) {
        Ok(subject)
    } else {
        Err(format!("'{subject}' is not one of its trusted subjects"))
    }
}

/// Who a token names: `namespace/job` for a Nomad workload identity, else
/// `sub`.
fn subject(
    kind: ClusterKind,
    claims: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let text = |k: &str| {
        claims
            .get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
    };
    match (kind, text("nomad_namespace"), text("nomad_job_id")) {
        (ClusterKind::Nomad, Some(ns), Some(job)) => Some(format!("{ns}/{job}")),
        _ => text("sub").map(str::to_owned),
    }
}

/// The relayed set, when it holds the key that verified the token (the
/// same key, not just its `kid`) and is not what was learned already.
fn learnable(
    relayed: Option<&str>,
    kid: &str,
    used: &Jwk,
    learned: Option<&str>,
) -> Option<String> {
    let relayed = relayed.filter(|r| !r.trim().is_empty())?;
    let set = parse(relayed)?;
    let same = set.find(kid).is_some_and(|k| k.algorithm == used.algorithm);
    let known = learned.and_then(parse).is_some_and(|l| l.keys == set.keys);
    (same && !known).then(|| serde_json::to_string(&set).unwrap_or_default())
}

#[cfg(test)]
pub(crate) mod tests;
