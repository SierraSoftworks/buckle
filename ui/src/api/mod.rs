//! A thin client over the server's `/api/v1` routes (design §12).
//!
//! Sessions are `HttpOnly` cookies the page never sees. Every mutating
//! request carries the double-submit CSRF token in `X-CSRF-Token`, fetched
//! once from `/auth/csrf` and cached. A `401` triggers one session refresh
//! (coalesced across concurrent requests, see [`crate::auth`]) and one retry;
//! a `401` that survives it sends the browser to sign in, preserving where it
//! was. A `403` on a mutation is retried once with a fresh CSRF token, since
//! a stale token is the likeliest cause; one that survives is the access
//! policy refusing, which pages show rather than hide.
//!
//! # Demo mode
//!
//! This module is the one place that knows about [`crate::fixtures`]: with
//! `?demo` every call below is answered from the in-memory demo store, so no
//! page has a demo branch of its own and none can forget one.

pub mod sse;

use std::cell::RefCell;

use buckle_api::{
    ActionRequest, ApproveRequest, AuditEntry, ContinueRequest, CreateTarget, CsrfToken, ErrorBody,
    ExecutorSpec, Fleet, HostDetail, HostPatch, HostPreview, HostSummary, JoinOptions, JoinRequest,
    JoinTicket, Me, Page, PlanGroup, RejectRequest, RevisionSummary, RolloutDetail, RolloutSummary,
    RunReport, RunSummary, StartRollout, UpgradeRequest,
};
use gloo_net::http::{Request, RequestBuilder, Response};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::fixtures::demo;
#[cfg(debug_assertions)]
use crate::fixtures::store as demo_store;
use crate::util::enc;

pub const BASE: &str = "/api/v1";
pub const CSRF_HEADER: &str = "X-CSRF-Token";

thread_local! {
    static CSRF: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Why a call did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// No session, and none could be renewed. The browser is already on its
    /// way to the sign-in page.
    Unauthorized,
    /// The access policy refused (`user_acl` or `admin_acl`).
    Forbidden(String),
    NotFound(String),
    /// The request no longer fits the state (approving a phase twice).
    Conflict(String),
    /// The server could not be reached.
    Network(String),
    Server(String),
}

impl ApiError {
    pub fn is_forbidden(&self) -> bool {
        matches!(self, ApiError::Forbidden(_))
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => f.write_str("Your session has ended. Signing you in again…"),
            ApiError::Forbidden(m) if m.is_empty() => {
                f.write_str("Your account is not permitted to do this.")
            }
            ApiError::Forbidden(m)
            | ApiError::NotFound(m)
            | ApiError::Conflict(m)
            | ApiError::Server(m) => f.write_str(m),
            ApiError::Network(m) => write!(f, "The server could not be reached: {m}"),
        }
    }
}

fn net(e: impl std::fmt::Display) -> ApiError {
    ApiError::Network(e.to_string())
}

async fn error_from(response: Response) -> ApiError {
    let status = response.status();
    let message = match response.json::<ErrorBody>().await {
        Ok(body) if body.advice.is_empty() => body.error,
        Ok(body) => format!("{} {}", body.error, body.advice.join(" ")),
        Err(_) => String::new(),
    };
    match status {
        401 => ApiError::Unauthorized,
        403 => ApiError::Forbidden(message),
        404 if message.is_empty() => ApiError::NotFound("That does not exist (any more).".into()),
        404 => ApiError::NotFound(message),
        409 => ApiError::Conflict(message),
        _ if message.is_empty() => {
            ApiError::Server(format!("The server answered with an error ({status})."))
        }
        _ => ApiError::Server(message),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
    Patch,
    Delete,
}

impl Method {
    fn builder(self, url: &str) -> RequestBuilder {
        match self {
            Method::Get => Request::get(url),
            Method::Post => Request::post(url),
            Method::Patch => Request::patch(url),
            Method::Delete => Request::delete(url),
        }
    }

    fn mutating(self) -> bool {
        self != Method::Get
    }
}

/// The CSRF token, fetched once per page load.
pub async fn csrf() -> Result<String, ApiError> {
    if let Some(token) = CSRF.with(|c| c.borrow().clone()) {
        return Ok(token);
    }
    let response = Request::get(&format!("{BASE}/auth/csrf"))
        .send()
        .await
        .map_err(net)?;
    if !response.ok() {
        return Err(error_from(response).await);
    }
    let token = response.json::<CsrfToken>().await.map_err(net)?.token;
    CSRF.with(|c| *c.borrow_mut() = Some(token.clone()));
    Ok(token)
}

fn forget_csrf() {
    CSRF.with(|c| *c.borrow_mut() = None);
}

async fn attempt<B: Serialize>(
    method: Method,
    url: &str,
    body: Option<&B>,
) -> Result<Response, ApiError> {
    let mut builder = method.builder(url);
    if method.mutating() {
        builder = builder.header(CSRF_HEADER, &csrf().await?);
    }
    let request = match body {
        Some(body) => builder.json(body).map_err(net)?,
        None => builder.build().map_err(net)?,
    };
    request.send().await.map_err(net)
}

/// Sends a request with the session's rules: CSRF on mutations, one refresh
/// on a `401`, one fresh CSRF token on a `403`.
async fn send<B: Serialize>(
    method: Method,
    path: &str,
    body: Option<&B>,
) -> Result<Response, ApiError> {
    let url = format!("{BASE}{path}");
    let mut response = attempt(method, &url, body).await?;

    if response.status() == 401 {
        if crate::auth::refresh_session().await.is_ok() {
            response = attempt(method, &url, body).await?;
        }
        if response.status() == 401 {
            crate::auth::begin_login();
            return Err(ApiError::Unauthorized);
        }
    }

    if response.status() == 403 && method.mutating() {
        forget_csrf();
        response = attempt(method, &url, body).await?;
    }

    if response.ok() {
        Ok(response)
    } else {
        Err(error_from(response).await)
    }
}

async fn get<T: DeserializeOwned>(path: &str) -> Result<T, ApiError> {
    send::<()>(Method::Get, path, None)
        .await?
        .json::<T>()
        .await
        .map_err(|e| ApiError::Server(format!("The server's answer was unreadable: {e}")))
}

async fn write<B: Serialize, T: DeserializeOwned>(
    method: Method,
    path: &str,
    body: Option<&B>,
) -> Result<T, ApiError> {
    send(method, path, body)
        .await?
        .json::<T>()
        .await
        .map_err(|e| ApiError::Server(format!("The server's answer was unreadable: {e}")))
}

// ------------------------------------------------------------------ session

/// `GET /me`: who is asking and whether they may act. A server without
/// sign-in answers with [`Me::anonymous`] set; one from before that field
/// answered 204, read here as `None` (everything permitted).
pub async fn me() -> Result<Option<Me>, ApiError> {
    demo!(demo_store::me());
    let response = send::<()>(Method::Get, "/me", None).await?;
    if response.status() == 204 {
        return Ok(None);
    }
    response.json::<Me>().await.map(Some).map_err(net)
}

/// `POST /auth/refresh`, which re-issues the session cookie from the refresh
/// cookie. Not routed through [`send`]: it is what `send` calls on a `401`.
pub async fn refresh() -> Result<(), ApiError> {
    demo!(demo_store::refresh());
    let response = Request::post(&format!("{BASE}/auth/refresh"))
        .header(CSRF_HEADER, &csrf().await?)
        .send()
        .await
        .map_err(net)?;
    if response.ok() {
        Ok(())
    } else {
        Err(error_from(response).await)
    }
}

pub async fn logout() -> Result<(), ApiError> {
    demo!(Ok(()));
    send::<()>(Method::Post, "/auth/logout", None)
        .await
        .map(|_| ())
}

// ------------------------------------------------------------------ fleet and hosts

pub async fn fleet() -> Result<Fleet, ApiError> {
    demo!(demo_store::fleet());
    get("/fleet").await
}

/// The host with its facts, owned resources, recent runs, health results and
/// drift: everything `/hosts/{id}/facts|state|runs` serve separately.
pub async fn host(id: &str) -> Result<HostDetail, ApiError> {
    demo!(demo_store::host(id));
    get(&format!("/hosts/{}", enc(id))).await
}

/// `GET /hosts/{id}/runs`: the host's runs, newest first. Read as a page,
/// or as the bare list servers answered with before run reports.
pub async fn host_runs(id: &str) -> Result<Vec<RunSummary>, ApiError> {
    demo!(demo_store::host_runs(id));
    let text = send::<()>(
        Method::Get,
        &format!("/hosts/{}/runs?limit=50", enc(id)),
        None,
    )
    .await?
    .text()
    .await
    .map_err(net)?;
    // Told apart by their first character rather than an untagged enum,
    // whose buffering costs the bundle more than this whole route.
    let parsed = if text.trim_start().starts_with('[') {
        serde_json::from_str(&text)
    } else {
        serde_json::from_str::<Page<RunSummary>>(&text).map(|page| page.items)
    };
    parsed.map_err(|e| ApiError::Server(format!("The server's answer was unreadable: {e}")))
}

/// `GET /hosts/{id}/runs/{run}`: one run, module by module and step by step.
pub async fn host_run(id: &str, run: &str) -> Result<RunReport, ApiError> {
    demo!(demo_store::host_run(id, run));
    get(&format!("/hosts/{}/runs/{}", enc(id), enc(run))).await
}

pub async fn patch_host(id: &str, patch: &HostPatch) -> Result<HostSummary, ApiError> {
    demo!(demo_store::patch_host(id, patch));
    write(Method::Patch, &format!("/hosts/{}", enc(id)), Some(patch)).await
}

/// `approve` (a pending registration), `pause` or `resume`.
pub async fn host_action(id: &str, action: &str) -> Result<HostSummary, ApiError> {
    demo!(demo_store::host_action(id, action));
    write::<(), _>(
        Method::Post,
        &format!("/hosts/{}/{}", enc(id), enc(action)),
        None,
    )
    .await
}

/// Asks the host's agent to install `version` (or the fleet's desired agent
/// version) once idle, then restart into it.
pub async fn upgrade_host(id: &str, version: Option<String>) -> Result<HostSummary, ApiError> {
    demo!(demo_store::upgrade_host(id, version.as_deref()));
    write(
        Method::Post,
        &format!("/hosts/{}/upgrade", enc(id)),
        Some(&UpgradeRequest { version }),
    )
    .await
}

pub async fn forget_host(id: &str) -> Result<(), ApiError> {
    demo!(demo_store::forget_host(id));
    send::<()>(Method::Delete, &format!("/hosts/{}", enc(id)), None)
        .await
        .map(|_| ())
}

// ------------------------------------------------------------------ cluster targets

/// `POST /targets`: a cluster target buckle.yaml does not declare.
pub async fn create_target(request: &CreateTarget) -> Result<HostSummary, ApiError> {
    write(Method::Post, "/targets", Some(request)).await
}

/// `GET /targets/{id}/executor`: the job that runs a cluster's executor
/// inside it.
pub async fn target_executor(id: &str) -> Result<ExecutorSpec, ApiError> {
    get(&format!("/targets/{}/executor", enc(id))).await
}

// ------------------------------------------------------------------ joining hosts

/// `GET /join`: what the Add an agent wizard can offer.
pub async fn join_options() -> Result<JoinOptions, ApiError> {
    demo!(demo_store::join_options());
    get("/join").await
}

/// `POST /join`: a join command for one new host, ready or awaiting
/// Tailscale's consent.
pub async fn create_join(request: &JoinRequest) -> Result<JoinTicket, ApiError> {
    demo!(demo_store::create_join(request));
    write(Method::Post, "/join", Some(request)).await
}

/// `GET /join/{id}`: a join command this admin created.
pub async fn join_ticket(id: &str) -> Result<JoinTicket, ApiError> {
    demo!(demo_store::join_ticket(id));
    get(&format!("/join/{}", enc(id))).await
}

/// Which groups and rollout phase a label set would give the host.
pub async fn preview_host(id: &str, patch: &HostPatch) -> Result<HostPreview, ApiError> {
    demo!(demo_store::preview_host(id, patch));
    write(
        Method::Post,
        &format!("/hosts/{}/preview", enc(id)),
        Some(patch),
    )
    .await
}

// ------------------------------------------------------------------ revisions

pub async fn revisions(offset: u64) -> Result<Page<RevisionSummary>, ApiError> {
    demo!(demo_store::revisions(offset));
    get(&format!("/revisions?offset={offset}&limit=50")).await
}

pub async fn start_rollout(revision: &str, note: &str) -> Result<RolloutSummary, ApiError> {
    demo!(demo_store::start_rollout(revision, note));
    let body = StartRollout { note: note.into() };
    write(
        Method::Post,
        &format!("/revisions/{}/rollout", enc(revision)),
        Some(&body),
    )
    .await
}

// ------------------------------------------------------------------ rollouts

pub async fn rollouts(offset: u64) -> Result<Page<RolloutSummary>, ApiError> {
    demo!(demo_store::rollouts(offset));
    get(&format!("/rollouts?offset={offset}&limit=50")).await
}

pub async fn rollout(id: &str) -> Result<RolloutDetail, ApiError> {
    demo!(demo_store::rollout(id));
    get(&format!("/rollouts/{}", enc(id))).await
}

/// The deduplicated plans, with their diffs.
pub async fn rollout_plans(id: &str) -> Result<Vec<PlanGroup>, ApiError> {
    demo!(demo_store::rollout(id).map(|r| r.plans));
    get(&format!("/rollouts/{}/plans", enc(id))).await
}

pub async fn approve(id: &str, request: &ApproveRequest) -> Result<RolloutDetail, ApiError> {
    demo!(demo_store::approve(id, request));
    write(
        Method::Post,
        &format!("/rollouts/{}/approve", enc(id)),
        Some(request),
    )
    .await
}

/// Turns down a rollout that is awaiting approval; it ends aborted, with
/// `reason` in the audit log.
pub async fn reject(id: &str, reason: &str) -> Result<RolloutDetail, ApiError> {
    demo!(demo_store::reject(id, reason));
    let body = RejectRequest {
        reason: reason.into(),
    };
    write(
        Method::Post,
        &format!("/rollouts/{}/reject", enc(id)),
        Some(&body),
    )
    .await
}

/// `pause`, `resume`, `rollback` or `abort`.
pub async fn rollout_action(id: &str, action: &str, note: &str) -> Result<RolloutDetail, ApiError> {
    demo!(demo_store::rollout_action(id, action, note));
    let body = ActionRequest { note: note.into() };
    write(
        Method::Post,
        &format!("/rollouts/{}/{}", enc(id), enc(action)),
        Some(&body),
    )
    .await
}

/// `retry`, `skip` or `force` one host of a paused rollout.
pub async fn rollout_host_action(
    id: &str,
    host: &str,
    action: &str,
    note: &str,
) -> Result<RolloutDetail, ApiError> {
    demo!(demo_store::rollout_host_action(id, host, action, note));
    let body = ActionRequest { note: note.into() };
    write(
        Method::Post,
        &format!("/rollouts/{}/hosts/{}/{}", enc(id), enc(host), enc(action)),
        Some(&body),
    )
    .await
}

/// `GET /rollouts/{id}/hosts/{host}/report`: the host's run in this
/// rollout, live while it applies.
pub async fn rollout_host_report(id: &str, host: &str) -> Result<RunReport, ApiError> {
    demo!(demo_store::rollout_host_report(id, host));
    get(&format!("/rollouts/{}/hosts/{}/report", enc(id), enc(host))).await
}

/// Retries a host with the named failing checks waived for this rollout.
pub async fn continue_host(
    id: &str,
    host: &str,
    request: &ContinueRequest,
) -> Result<RolloutDetail, ApiError> {
    demo!(demo_store::continue_host(id, host, request));
    write(
        Method::Post,
        &format!("/rollouts/{}/hosts/{}/continue", enc(id), enc(host)),
        Some(request),
    )
    .await
}

// ------------------------------------------------------------------ audit

/// A page of the audit log, newest first, optionally filtered with a filt-rs
/// expression and continued from before an entry id. Without `progress`
/// the controller's progress transitions (`buckle_api::PROGRESS_ACTIONS`)
/// are left out (`progress=false`).
pub async fn audit(
    filter: &str,
    before: Option<u64>,
    progress: bool,
) -> Result<Page<AuditEntry>, ApiError> {
    demo!(demo_store::audit(filter, before, progress));
    let mut path = format!("/audit?limit=100&filter={}", enc(filter.trim()));
    if let Some(before) = before {
        path.push_str(&format!("&before={before}"));
    }
    if !progress {
        path.push_str("&progress=false");
    }
    get(&path).await
}
