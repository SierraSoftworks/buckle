//! Who is asking and what they may do.
//!
//! Every `/api/v1` request (except the sign-in endpoints and the HMAC-checked
//! webhook) is resolved to a [`Principal`] by [`resolve`]: with OIDC, from
//! the id token in the HttpOnly session cookie (re-validated each request);
//! without it, an anonymous principal. `user_acl` decides whether the
//! request may proceed at all (absent: anyone authenticated) and
//! `admin_acl` whether it carries administrative rights (absent: nobody),
//! both evaluated over [`AclRequest`]. Mutating requests also need the
//! double-submit CSRF token.
//!
//! ACLs see the path the router dispatches on: percent-decoded, with any
//! path that could be read two ways (an encoded `/` or `\`, a `.` or `..`
//! segment) refused outright, so an encoded path cannot slip past a path
//! rule. Signing out revokes the session server-side.

use super::{ApiError, AppState};
use actix_web::cookie::{Cookie, SameSite};
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header::HeaderMap;
use actix_web::{FromRequest, HttpMessage, HttpRequest, body::BoxBody};
use buckle_core::{FilterValue, Filterable};
use std::collections::HashMap;
use std::future::{Ready, ready};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The session cookie (holds the id token).
pub const SESSION_COOKIE: &str = "buckle_session";
/// The refresh cookie (holds the refresh token, scoped to the auth routes).
pub const REFRESH_COOKIE: &str = "buckle_refresh";
/// The CSRF cookie (double submit).
pub const CSRF_COOKIE: &str = "buckle_csrf";
/// The header carrying the CSRF token on mutating requests.
pub const CSRF_HEADER: &str = "x-csrf-token";
/// The sign-in state cookie (its SHA-256 is the `state` parameter).
pub const STATE_COOKIE: &str = "buckle_auth_state";
/// The browser binding cookie (`__Host-` prefixed over HTTPS).
pub const BINDING_COOKIE: &str = "buckle_auth_binding";

/// Who a request speaks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub subject: String,
    pub name: String,
    pub email: Option<String>,
    /// Claims visible to ACLs (protocol claims removed).
    pub claims: serde_json::Map<String, serde_json::Value>,
    pub is_admin: bool,
    /// Signed in through OIDC (false without OIDC configured).
    pub authenticated: bool,
}

impl Principal {
    /// The actor name for audit rows.
    pub fn actor(&self) -> String {
        if self.authenticated {
            self.email.clone().unwrap_or_else(|| self.subject.clone())
        } else {
            "anonymous".into()
        }
    }
}

/// What an ACL expression sees: `method`, `path`, `client_ip`,
/// `headers.<name>` and `claims.<path>`.
pub struct AclRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub client_ip: Option<String>,
    pub headers: &'a HeaderMap,
    pub claims: Option<&'a serde_json::Map<String, serde_json::Value>>,
}

impl Filterable for AclRequest<'_> {
    fn get(&self, key: &str) -> FilterValue<'_> {
        let lower = key.to_ascii_lowercase();
        match lower.as_str() {
            "method" => self.method.into(),
            "path" => self.path.into(),
            "client_ip" => self.client_ip.as_deref().into(),
            _ => {
                if let Some(name) = lower.strip_prefix("headers.") {
                    if matches!(name, "cookie" | "authorization") {
                        return FilterValue::Null;
                    }
                    return self
                        .headers
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(|s| FilterValue::String(s.to_owned().into()))
                        .unwrap_or(FilterValue::Null);
                }
                if let Some(path) = key
                    .strip_prefix("claims.")
                    .or_else(|| key.strip_prefix("CLAIMS."))
                {
                    let Some(claims) = self.claims else {
                        return FilterValue::Null;
                    };
                    let root = serde_json::Value::Object(claims.clone());
                    return buckle_core::selectors::lookup_path(&root, path)
                        .map(buckle_core::selectors::json_to_filter_value)
                        .unwrap_or(FilterValue::Null);
                }
                FilterValue::Null
            }
        }
    }
}

/// The client address, honouring `web.trust_proxy` for `X-Forwarded-For`:
/// the entry `web.trusted_proxy_hops` (default 1) places from the right,
/// which is the address the outermost trusted proxy saw. Entries to its
/// left are the client's own claims and never used.
pub fn client_ip(web: &crate::config::WebConfig, req: &HttpRequest) -> Option<String> {
    if web.trust_proxy {
        let hops = web.trusted_proxy_hops.unwrap_or(1).max(1) as usize;
        let chain: Vec<&str> = req
            .headers()
            .get_all("x-forwarded-for")
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if !chain.is_empty() {
            // Fewer entries than hops: every entry was written by a trusted
            // proxy, so the leftmost is the client.
            let index = chain.len().saturating_sub(hops);
            return Some(chain[index].to_owned());
        }
    }
    req.peer_addr().map(|a| a.ip().to_string())
}

/// The request path as the router dispatches on it, percent-decoded, or
/// `None` when it is ambiguous and must be refused: an encoded `/` or `\`,
/// a literal backslash, a `.` or `..` segment (encoded or not), or bytes
/// that do not decode to UTF-8.
pub fn normalised_path(raw: &str) -> Option<String> {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' {
            let decoded = match (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            ) {
                (Some(h), Some(l)) => h * 16 + l,
                _ => return None,
            };
            if matches!(decoded, b'/' | b'\\') {
                return None;
            }
            out.push(decoded);
            i += 3;
        } else {
            if b == b'\\' {
                return None;
            }
            out.push(b);
            i += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    if path.split('/').any(|seg| seg == "." || seg == "..") {
        return None;
    }
    Some(path)
}

fn is_mutating(method: &actix_web::http::Method) -> bool {
    !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS")
}

/// Paths that do their own authentication.
fn exempt(path: &str) -> bool {
    path.starts_with("/api/v1/auth/")
        || path == "/api/v1/csrf"
        || path == "/api/v1/webhooks/source"
        || path == "/api/v1/me"
}

/// Whether the CSRF header matches the CSRF cookie.
pub fn csrf_ok(req: &HttpRequest) -> bool {
    let header = req.headers().get(CSRF_HEADER).and_then(|v| v.to_str().ok());
    let cookie = req.cookie(CSRF_COOKIE).map(|c| c.value().to_owned());
    match (header, cookie) {
        (Some(h), Some(c)) if !h.is_empty() => crate::grpc::auth::constant_time_eq(h, &c),
        _ => false,
    }
}

/// Resolves the principal behind a request. `Err` is 401 (no or invalid
/// session) or 403 (`user_acl` refused).
pub async fn resolve(state: &AppState, req: &HttpRequest) -> Result<Principal, ApiError> {
    let web = &state.web;
    let (claims, authenticated) = match &state.oidc {
        Some(oidc) => {
            let Some(cookie) = req.cookie(SESSION_COOKIE) else {
                return Err(ApiError::unauthorized());
            };
            if state.revoked.contains(cookie.value()) {
                return Err(ApiError::unauthorized());
            }
            match oidc.validate(cookie.value(), None).await {
                Ok(c) => (Some(c), true),
                Err(e) if e.is(buckle_core::error::Kind::System) => {
                    tracing::warn!(error = %e, "the identity provider could not be consulted");
                    return Err(ApiError::unavailable(
                        "The identity provider could not be reached.",
                    ));
                }
                Err(_) => return Err(ApiError::unauthorized()),
            }
        }
        None => (None, false),
    };
    let visible = claims.as_ref().map(super::oidc::filterable_claims);
    let (allowed, is_admin) = evaluate(web, req, visible.as_ref());
    if !allowed {
        return Err(ApiError::forbidden("You are not allowed to use Buckle."));
    }
    let claims = claims.unwrap_or_default();
    let get = |k: &str| claims.get(k).and_then(|v| v.as_str()).map(str::to_owned);
    let subject = get("sub").unwrap_or_else(|| "anonymous".into());
    Ok(Principal {
        name: get("name")
            .or_else(|| get("preferred_username"))
            .unwrap_or_else(|| subject.clone()),
        email: get("email"),
        subject,
        claims: visible.unwrap_or_default(),
        is_admin,
        authenticated,
    })
}

/// Evaluates `user_acl` (absent: allowed) and `admin_acl` (absent: denied)
/// for a request carrying `claims`. An expression that fails to evaluate
/// counts as not matching.
pub fn evaluate(
    web: &crate::config::WebConfig,
    req: &HttpRequest,
    claims: Option<&serde_json::Map<String, serde_json::Value>>,
) -> (bool, bool) {
    // The middleware refuses ambiguous paths before this runs; the raw
    // path is only a fallback for callers outside it.
    let path = normalised_path(req.path()).unwrap_or_else(|| req.path().to_owned());
    let method = req.method().as_str().to_owned();
    let acl = AclRequest {
        method: &method,
        path: &path,
        client_ip: client_ip(web, req),
        headers: req.headers(),
        claims,
    };
    let allowed = web
        .user_acl
        .as_ref()
        .map(|f| f.matches(&acl).unwrap_or(false))
        .unwrap_or(true);
    let is_admin = web
        .admin_acl
        .as_ref()
        .map(|f| f.matches(&acl).unwrap_or(false))
        .unwrap_or(false);
    (allowed, is_admin)
}

/// The middleware: resolve, authorise and CSRF-check `/api/v1` requests.
pub async fn middleware(
    req: ServiceRequest,
    next: actix_web::middleware::Next<BoxBody>,
) -> Result<ServiceResponse<BoxBody>, actix_web::Error> {
    let Some(path) = normalised_path(req.path()) else {
        let (http, _) = req.into_parts();
        return Ok(ServiceResponse::new(
            http,
            ApiError::bad_request(
                "The request path contains an encoded separator or a dot segment.",
            )
            .response(),
        ));
    };
    if !path.starts_with("/api/v1/") || exempt(&path) {
        return next.call(req).await;
    }
    let Some(state) = req.app_data::<actix_web::web::Data<AppState>>().cloned() else {
        return next.call(req).await;
    };
    let (http, payload) = req.into_parts();
    let principal = match resolve(&state, &http).await {
        Ok(p) => p,
        Err(e) => return Ok(ServiceResponse::new(http, e.response())),
    };
    if is_mutating(http.method()) && !csrf_ok(&http) {
        return Ok(ServiceResponse::new(http, ApiError::csrf().response()));
    }
    http.extensions_mut().insert(principal);
    next.call(ServiceRequest::from_parts(http, payload)).await
}

/// Extractor: the resolved principal (any authorised user).
#[derive(Debug, Clone)]
pub struct Authenticated(pub Principal);

impl FromRequest for Authenticated {
    type Error = ApiError;
    type Future = Ready<Result<Self, ApiError>>;

    fn from_request(req: &HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        ready(
            req.extensions()
                .get::<Principal>()
                .cloned()
                .map(Authenticated)
                .ok_or_else(ApiError::unauthorized),
        )
    }
}

/// Extractor: the resolved principal, which must pass `admin_acl`.
#[derive(Debug, Clone)]
pub struct Administrative(pub Principal);

impl FromRequest for Administrative {
    type Error = ApiError;
    type Future = Ready<Result<Self, ApiError>>;

    fn from_request(req: &HttpRequest, _: &mut actix_web::dev::Payload) -> Self::Future {
        ready(match req.extensions().get::<Principal>() {
            None => Err(ApiError::unauthorized()),
            Some(p) if p.is_admin => Ok(Administrative(p.clone())),
            Some(_) => Err(ApiError::forbidden(
                "This action needs administrative rights (web.admin_acl).",
            )),
        })
    }
}

/// A sign-in in progress, claimed once on callback.
#[derive(Clone)]
pub struct PendingAuth {
    pub verifier: String,
    pub nonce: String,
    pub return_to: String,
    /// SHA-256 of the binding cookie.
    pub binding: String,
    pub expires: Instant,
}

/// One-shot pending sign-ins keyed by the `state` parameter.
#[derive(Default)]
pub struct PendingAuths {
    inner: Mutex<HashMap<String, PendingAuth>>,
}

impl PendingAuths {
    /// Remembers a sign-in for ten minutes.
    pub fn insert(&self, state: String, pending: PendingAuth) {
        if let Ok(mut m) = self.inner.lock() {
            let now = Instant::now();
            m.retain(|_, p| p.expires > now);
            m.insert(state, pending);
        }
    }

    /// Removes and returns the sign-in for `state` if it has not expired.
    pub fn claim(&self, state: &str) -> Option<PendingAuth> {
        let p = self.inner.lock().ok()?.remove(state)?;
        (p.expires > Instant::now()).then_some(p)
    }
}

/// Sessions signed out before their id token expired, by SHA-256 of the
/// token, until that expiry.
#[derive(Default)]
pub struct RevokedSessions {
    inner: Mutex<HashMap<String, Instant>>,
}

impl RevokedSessions {
    /// The longest a revocation is kept when the token's expiry cannot be read.
    const FALLBACK: Duration = Duration::from_secs(24 * 3600);

    /// Revokes the session holding `token` until the token expires.
    pub fn revoke(&self, token: &str) {
        let remaining = unverified_expiry(token)
            .map(|exp| (exp - chrono::Utc::now().timestamp()).max(0) as u64)
            .map(Duration::from_secs)
            .unwrap_or(Self::FALLBACK)
            .min(Self::FALLBACK);
        let now = Instant::now();
        let mut m = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        m.retain(|_, until| *until > now);
        m.insert(super::oidc::sha256_b64(token), now + remaining);
    }

    /// Whether the session holding `token` was signed out.
    pub fn contains(&self, token: &str) -> bool {
        let key = super::oidc::sha256_b64(token);
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .is_some_and(|until| *until > Instant::now())
    }
}

/// The `exp` claim of a JWT, without verifying it (only used to bound how
/// long a revocation is remembered).
fn unverified_expiry(token: &str) -> Option<i64> {
    use base64::Engine as _;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()
}

/// A fixed-window limiter per client address for the sign-in endpoints.
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    inner: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl RateLimiter {
    /// `limit` requests per `window` per address.
    pub fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Counts a request; `false` when over the limit.
    pub fn allow(&self, ip: Option<IpAddr>) -> bool {
        let Some(ip) = ip else { return true };
        let Ok(mut m) = self.inner.lock() else {
            return true;
        };
        let now = Instant::now();
        if m.len() > 10_000 {
            m.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }
        let entry = m.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.limit
    }
}

/// Cookie attributes for this deployment.
pub fn cookie<'a>(
    state: &AppState,
    name: &'a str,
    value: String,
    path: &'a str,
    http_only: bool,
    max_age: Option<Duration>,
) -> Cookie<'a> {
    let mut c = Cookie::build(name, value)
        .path(path)
        .http_only(http_only)
        .secure(state.secure_cookies)
        .same_site(SameSite::Lax)
        .finish();
    if let Some(age) = max_age {
        c.set_max_age(actix_web::cookie::time::Duration::seconds(
            age.as_secs() as i64
        ));
    }
    c
}

/// A cookie that clears `name` at `path`.
pub fn clear_cookie<'a>(state: &AppState, name: &'a str, path: &'a str) -> Cookie<'a> {
    let mut c = cookie(state, name, String::new(), path, true, None);
    c.make_removal();
    c
}

/// The binding cookie's name (`__Host-` prefixed when cookies are secure).
pub fn binding_cookie_name(state: &AppState) -> &'static str {
    if state.secure_cookies {
        "__Host-buckle_auth_binding"
    } else {
        BINDING_COOKIE
    }
}

/// Accepts only local paths: starts with `/`, not `//`, no backslash.
pub fn safe_return_to(raw: Option<&str>) -> String {
    match raw {
        Some(p) if p.starts_with('/') && !p.starts_with("//") && !p.contains('\\') => p.to_owned(),
        _ => "/".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(Some("/rollouts/1"), "/rollouts/1")]
    #[case(Some("//evil.example"), "/")]
    #[case(Some("https://evil.example"), "/")]
    #[case(Some("/\\evil"), "/")]
    #[case(None, "/")]
    fn return_to_must_be_local(#[case] raw: Option<&str>, #[case] expected: &str) {
        assert_eq!(safe_return_to(raw), expected);
    }

    #[rstest]
    #[case("/api/v1/fleet", Some("/api/v1/fleet"))]
    #[case("/api/v1/fl%65et", Some("/api/v1/fleet"))]
    #[case("/api/v1/hosts/a%20b", Some("/api/v1/hosts/a b"))]
    #[case("/api/v1/hosts%2Fx", None)]
    #[case("/api/v1/hosts%2fx", None)]
    #[case("/api/v1/hosts%5Cx", None)]
    #[case("/api/v1\\hosts", None)]
    #[case("/api/v1/auth/../fleet", None)]
    #[case("/api/v1/auth/%2e%2e/fleet", None)]
    #[case("/api/v1/./fleet", None)]
    #[case("/api/v1/bad%zz", None)]
    #[case("/api/v1/bad%", None)]
    #[case("/api/v1/%ff", None)]
    #[case("/assets/app.js", Some("/assets/app.js"))]
    fn paths_are_decoded_once_and_ambiguous_ones_refused(
        #[case] raw: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(normalised_path(raw).as_deref(), expected);
    }

    fn forwarded(hops: Option<u32>, headers: &[&str]) -> Option<String> {
        let mut req =
            actix_web::test::TestRequest::get().peer_addr("10.9.9.9:1234".parse().unwrap());
        for h in headers {
            req = req.append_header(("x-forwarded-for", *h));
        }
        let web = crate::config::WebConfig {
            trust_proxy: true,
            trusted_proxy_hops: hops,
            ..Default::default()
        };
        client_ip(&web, &req.to_http_request())
    }

    #[rstest]
    #[case(None, &["6.6.6.6, 1.2.3.4"], Some("1.2.3.4"))]
    #[case(None, &["6.6.6.6", "1.2.3.4"], Some("1.2.3.4"))]
    #[case(Some(2), &["6.6.6.6, 1.2.3.4, 10.0.0.2"], Some("1.2.3.4"))]
    #[case(Some(3), &["1.2.3.4, 10.0.0.2"], Some("1.2.3.4"))]
    #[case(None, &[], Some("10.9.9.9"))]
    fn the_client_is_the_rightmost_untrusted_hop(
        #[case] hops: Option<u32>,
        #[case] headers: &[&str],
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(forwarded(hops, headers).as_deref(), expected);
    }

    #[test]
    fn without_trust_proxy_the_header_is_ignored() {
        let req = actix_web::test::TestRequest::get()
            .peer_addr("10.9.9.9:1234".parse().unwrap())
            .insert_header(("x-forwarded-for", "6.6.6.6"))
            .to_http_request();
        assert_eq!(
            client_ip(&Default::default(), &req).as_deref(),
            Some("10.9.9.9")
        );
    }

    #[test]
    fn revoked_sessions_are_remembered_until_the_token_expires() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let exp = chrono::Utc::now().timestamp() + 600;
        let token = format!(
            "h.{}.s",
            b64.encode(serde_json::json!({ "exp": exp }).to_string())
        );
        let revoked = RevokedSessions::default();
        assert!(!revoked.contains(&token));
        revoked.revoke(&token);
        assert!(revoked.contains(&token));
        assert!(!revoked.contains("another"));
        let expired = format!(
            "h.{}.s",
            b64.encode(serde_json::json!({ "exp": 1 }).to_string())
        );
        revoked.revoke(&expired);
        assert!(!revoked.contains(&expired), "nothing to remember");
    }

    #[test]
    fn rate_limits_apply_per_address() {
        let rl = RateLimiter::new(2, Duration::from_secs(60));
        let a: IpAddr = "10.0.0.1".parse().unwrap();
        let b: IpAddr = "10.0.0.2".parse().unwrap();
        assert!(rl.allow(Some(a)) && rl.allow(Some(a)));
        assert!(!rl.allow(Some(a)));
        assert!(rl.allow(Some(b)));
    }

    #[test]
    fn pending_sign_ins_are_one_shot() {
        let p = PendingAuths::default();
        p.insert(
            "s".into(),
            PendingAuth {
                verifier: "v".into(),
                nonce: "n".into(),
                return_to: "/".into(),
                binding: "b".into(),
                expires: Instant::now() + Duration::from_secs(600),
            },
        );
        assert!(p.claim("s").is_some());
        assert!(p.claim("s").is_none());
    }
}
