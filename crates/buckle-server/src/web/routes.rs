//! Route table and handlers for `/api/v1`.

use super::auth::{self, Administrative, Authenticated};
use super::{ApiError, AppState};
use crate::rollout::Command;
use crate::rollout::view;
use crate::store::{AuditFilter, NewAudit, kv::keys};
use actix_web::http::StatusCode;
use actix_web::{HttpMessage, HttpRequest, HttpResponse, web};
use buckle_api::{
    ActionRequest, ApproveRequest, ClusterInfo, ContinueRequest, CreateTarget, CsrfToken, Fleet,
    FleetCounts, HealthResultDto, HealthSummary, HostDetail, HostPatch, HostPreview, HostStatus,
    LiveEvent, Me, Page, RejectRequest, ResourceSummary, RolloutState, RunSummary, StartRollout,
    TargetKind, UpgradeRequest,
};
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

type Data = web::Data<AppState>;
type ApiResult = Result<HttpResponse, ApiError>;

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api/v1")
            .route("/me", web::get().to(me))
            .route("/csrf", web::get().to(csrf))
            .route("/auth/csrf", web::get().to(csrf))
            .route("/auth/login", web::get().to(login))
            .route("/auth/callback", web::get().to(callback))
            .route("/auth/refresh", web::post().to(refresh))
            .route("/auth/logout", web::post().to(logout))
            .route("/fleet", web::get().to(fleet))
            .route("/hosts", web::get().to(hosts))
            .route("/hosts/{id}", web::get().to(host_detail))
            .route("/hosts/{id}", web::patch().to(host_patch))
            .route("/hosts/{id}", web::delete().to(host_forget))
            .route("/hosts/{id}/facts", web::get().to(host_facts))
            .route("/hosts/{id}/state", web::get().to(host_state))
            .route("/hosts/{id}/health", web::get().to(host_health))
            .route("/hosts/{id}/runs", web::get().to(host_runs))
            .route("/hosts/{id}/runs/{run}", web::get().to(host_run))
            .route("/hosts/{id}/preview", web::post().to(host_preview))
            .route("/hosts/{id}/upgrade", web::post().to(host_upgrade))
            .route("/hosts/{id}/{action}", web::post().to(host_action))
            // Targets: hosts and cluster targets behind one set of routes.
            // The host routes above stay as the host-only view.
            .route("/targets", web::get().to(targets))
            .route("/targets", web::post().to(target_create))
            .route("/targets/{id}", web::get().to(host_detail))
            .route("/targets/{id}", web::patch().to(host_patch))
            .route("/targets/{id}", web::delete().to(host_forget))
            .route("/targets/{id}/executor", web::get().to(target_executor))
            .route("/targets/{id}/preview", web::post().to(host_preview))
            .route("/targets/{id}/{action}", web::post().to(host_action))
            .route("/join", web::get().to(join_options))
            .route("/join", web::post().to(join_create))
            .route("/join/tailscale/callback", web::get().to(join_callback))
            .route("/join/{id}", web::get().to(join_ticket))
            .route("/revisions", web::get().to(revisions))
            .route("/revisions/{id}", web::get().to(revision))
            .route("/revisions/{id}/rollout", web::post().to(start_rollout))
            .route("/rollouts", web::get().to(rollouts))
            .route("/rollouts/{id}", web::get().to(rollout))
            .route("/rollouts/{id}/plans", web::get().to(rollout_plans))
            .route(
                "/rollouts/{id}/hosts/{host}/report",
                web::get().to(rollout_host_report),
            )
            .route("/rollouts/{id}/approve", web::post().to(rollout_approve))
            .route("/rollouts/{id}/reject", web::post().to(rollout_reject))
            .route(
                "/rollouts/{id}/hosts/{host}/continue",
                web::post().to(rollout_continue),
            )
            .route(
                "/rollouts/{id}/hosts/{host}/{action}",
                web::post().to(rollout_host_action),
            )
            .route("/rollouts/{id}/{action}", web::post().to(rollout_action))
            .route("/events", web::get().to(events))
            .route("/catalog", web::get().to(catalog))
            .route("/audit", web::get().to(audit))
            .route("/webhooks/source", web::post().to(webhook)),
    );
}

/// Every route that requires a signed-in principal (for the route-table test).
#[cfg(test)]
pub const PROTECTED: &[(&str, &str)] = &[
    ("GET", "/api/v1/fleet"),
    ("GET", "/api/v1/hosts"),
    ("GET", "/api/v1/hosts/h"),
    ("PATCH", "/api/v1/hosts/h"),
    ("DELETE", "/api/v1/hosts/h"),
    ("GET", "/api/v1/hosts/h/facts"),
    ("GET", "/api/v1/hosts/h/state"),
    ("GET", "/api/v1/hosts/h/health"),
    ("GET", "/api/v1/hosts/h/runs"),
    ("POST", "/api/v1/hosts/h/preview"),
    ("POST", "/api/v1/hosts/h/approve"),
    ("POST", "/api/v1/hosts/h/pause"),
    ("POST", "/api/v1/hosts/h/resume"),
    ("GET", "/api/v1/targets"),
    ("POST", "/api/v1/targets"),
    ("GET", "/api/v1/targets/t"),
    ("PATCH", "/api/v1/targets/t"),
    ("DELETE", "/api/v1/targets/t"),
    ("POST", "/api/v1/targets/t/preview"),
    ("GET", "/api/v1/targets/t/executor"),
    ("POST", "/api/v1/targets/t/pause"),
    ("GET", "/api/v1/join"),
    ("POST", "/api/v1/join"),
    ("GET", "/api/v1/join/j"),
    ("GET", "/api/v1/join/tailscale/callback?state=s"),
    ("GET", "/api/v1/revisions"),
    ("GET", "/api/v1/revisions/r"),
    ("POST", "/api/v1/revisions/r/rollout"),
    ("GET", "/api/v1/rollouts"),
    ("GET", "/api/v1/rollouts/r"),
    ("GET", "/api/v1/rollouts/r/plans"),
    ("POST", "/api/v1/rollouts/r/approve"),
    ("POST", "/api/v1/rollouts/r/reject"),
    ("POST", "/api/v1/rollouts/r/pause"),
    ("POST", "/api/v1/rollouts/r/resume"),
    ("POST", "/api/v1/rollouts/r/rollback"),
    ("POST", "/api/v1/rollouts/r/abort"),
    ("POST", "/api/v1/rollouts/r/hosts/h/retry"),
    ("POST", "/api/v1/rollouts/r/hosts/h/skip"),
    ("POST", "/api/v1/rollouts/r/hosts/h/continue"),
    ("POST", "/api/v1/rollouts/r/hosts/h/force"),
    ("GET", "/api/v1/events"),
    ("GET", "/api/v1/catalog"),
    ("GET", "/api/v1/audit"),
    ("GET", "/api/v1/me"),
];

// ------------------------------------------------------------------ auth

/// `GET /me`: who the caller is and whether they may act (`is_admin`).
/// Without OIDC the caller is anonymous, and `is_admin` is what
/// `admin_acl` (absent: allowed) makes of the request's metadata.
async fn me(state: Data, req: HttpRequest) -> ApiResult {
    let p = auth::resolve(&state, &req).await?;
    if !p.authenticated {
        return Ok(HttpResponse::Ok().json(Me::anonymous(p.is_admin)));
    }
    Ok(HttpResponse::Ok().json(Me {
        name: p.name,
        email: p.email,
        subject: p.subject,
        is_admin: p.is_admin,
        anonymous: false,
    }))
}

async fn csrf(state: Data, req: HttpRequest) -> ApiResult {
    let token = req
        .cookie(auth::CSRF_COOKIE)
        .map(|c| c.value().to_owned())
        .filter(|v| v.len() >= 32)
        .unwrap_or_else(super::oidc::random_token);
    Ok(HttpResponse::Ok()
        .cookie(auth::cookie(
            &state,
            auth::CSRF_COOKIE,
            token.clone(),
            "/",
            true,
            None,
        ))
        .json(CsrfToken { token }))
}

fn peer_ip(state: &AppState, req: &HttpRequest) -> Option<std::net::IpAddr> {
    auth::client_ip(&state.web, req).and_then(|s| s.parse().ok())
}

/// `GET /auth/login?return_to=/rollouts/42`.
#[derive(Deserialize)]
struct LoginQuery {
    /// Where the browser lands after signing in: a local path (with its
    /// query and fragment), checked by [`auth::safe_return_to`]; anything
    /// else, or nothing, lands on `/`. This is the only spelling read.
    #[serde(default)]
    return_to: Option<String>,
}

/// Starts a sign-in: remembers PKCE, nonce and `return_to` for ten minutes
/// and redirects to the identity provider.
async fn login(state: Data, req: HttpRequest, q: web::Query<LoginQuery>) -> ApiResult {
    let Some(oidc) = state.oidc.clone() else {
        return Err(ApiError::not_found(
            "Sign-in is not configured on this server.",
        ));
    };
    if !state.limiter.allow(peer_ip(&state, &req)) {
        return Err(ApiError::too_many());
    }
    let pkce = super::oidc::pkce();
    let nonce = super::oidc::random_token();
    let state_secret = super::oidc::random_token();
    let state_param = super::oidc::sha256_b64(&state_secret);
    let binding = super::oidc::random_token();
    state.pending.insert(
        state_param.clone(),
        auth::PendingAuth {
            verifier: pkce.verifier.clone(),
            nonce: nonce.clone(),
            return_to: auth::safe_return_to(q.return_to.as_deref()),
            binding: super::oidc::sha256_b64(&binding),
            expires: std::time::Instant::now() + Duration::from_secs(600),
        },
    );
    let url = oidc
        .authorize_url(&state.redirect_uri(), &state_param, &nonce, &pkce.challenge)
        .await?;
    let ten = Some(Duration::from_secs(600));
    Ok(HttpResponse::Found()
        .insert_header(("location", url))
        .insert_header(("cache-control", "no-store"))
        .cookie(auth::cookie(
            &state,
            auth::STATE_COOKIE,
            state_secret,
            "/api/v1/auth",
            true,
            ten,
        ))
        .cookie(auth::cookie(
            &state,
            auth::binding_cookie_name(&state),
            binding,
            "/",
            true,
            ten,
        ))
        .finish())
}

fn sign_in_failed() -> ApiError {
    ApiError::bad_request("Sign-in failed. Start again from the sign-in page.")
}

async fn callback(
    state: Data,
    req: HttpRequest,
    q: web::Query<HashMap<String, String>>,
) -> ApiResult {
    let Some(oidc) = state.oidc.clone() else {
        return Err(ApiError::not_found(
            "Sign-in is not configured on this server.",
        ));
    };
    if !state.limiter.allow(peer_ip(&state, &req)) {
        return Err(ApiError::too_many());
    }
    let (Some(code), Some(state_param)) = (q.get("code"), q.get("state")) else {
        return Err(sign_in_failed());
    };
    let Some(state_cookie) = req.cookie(auth::STATE_COOKIE) else {
        return Err(sign_in_failed());
    };
    if !crate::grpc::auth::constant_time_eq(
        &super::oidc::sha256_b64(state_cookie.value()),
        state_param,
    ) {
        return Err(sign_in_failed());
    }
    let Some(pending) = state.pending.claim(state_param) else {
        return Err(sign_in_failed());
    };
    let binding = req
        .cookie(auth::binding_cookie_name(&state))
        .map(|c| c.value().to_owned())
        .unwrap_or_default();
    if !crate::grpc::auth::constant_time_eq(&super::oidc::sha256_b64(&binding), &pending.binding) {
        return Err(sign_in_failed());
    }
    let tokens = match oidc
        .exchange(code, &state.redirect_uri(), &pending.verifier)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "the authorization code could not be exchanged");
            return Err(sign_in_failed());
        }
    };
    let Some(id_token) = tokens.id_token.clone() else {
        return Err(sign_in_failed());
    };
    let claims = match oidc.validate(&id_token, Some(&pending.nonce)).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "the identity provider's id token was refused");
            return Err(sign_in_failed());
        }
    };
    let visible = super::oidc::filterable_claims(&claims);
    let (allowed, _) = auth::evaluate(&state.web, &req, Some(&visible));
    if !allowed {
        return Err(ApiError::forbidden("You are not allowed to use Buckle."));
    }
    let lifetime = claims
        .get("exp")
        .and_then(|v| v.as_i64())
        .map(|exp| (exp - chrono::Utc::now().timestamp()).max(0) as u64)
        .map(Duration::from_secs);
    let mut response = HttpResponse::Found();
    response
        .insert_header(("location", pending.return_to.clone()))
        .insert_header(("cache-control", "no-store"))
        .cookie(auth::cookie(
            &state,
            auth::SESSION_COOKIE,
            id_token,
            "/",
            true,
            lifetime,
        ))
        .cookie(auth::clear_cookie(
            &state,
            auth::STATE_COOKIE,
            "/api/v1/auth",
        ))
        .cookie(auth::clear_cookie(
            &state,
            auth::binding_cookie_name(&state),
            "/",
        ));
    if let Some(refresh) = tokens.refresh_token {
        response.cookie(auth::cookie(
            &state,
            auth::REFRESH_COOKIE,
            refresh,
            "/api/v1/auth",
            true,
            Some(Duration::from_secs(30 * 86400)),
        ));
    }
    Ok(response.finish())
}

async fn refresh(state: Data, req: HttpRequest) -> ApiResult {
    let Some(oidc) = state.oidc.clone() else {
        return Err(ApiError::not_found(
            "Sign-in is not configured on this server.",
        ));
    };
    if !state.limiter.allow(peer_ip(&state, &req)) {
        return Err(ApiError::too_many());
    }
    if !auth::csrf_ok(&req) {
        return Err(ApiError::csrf());
    }
    let Some(cookie) = req.cookie(auth::REFRESH_COOKIE) else {
        return Err(ApiError::unauthorized());
    };
    let tokens = oidc
        .refresh(cookie.value())
        .await
        .map_err(|_| ApiError::unauthorized())?;
    let id_token = tokens.id_token.ok_or_else(ApiError::unauthorized)?;
    oidc.validate(&id_token, None)
        .await
        .map_err(|_| ApiError::unauthorized())?;
    let mut response = HttpResponse::NoContent();
    response.cookie(auth::cookie(
        &state,
        auth::SESSION_COOKIE,
        id_token,
        "/",
        true,
        None,
    ));
    if let Some(r) = tokens.refresh_token {
        response.cookie(auth::cookie(
            &state,
            auth::REFRESH_COOKIE,
            r,
            "/api/v1/auth",
            true,
            Some(Duration::from_secs(30 * 86400)),
        ));
    }
    Ok(response.finish())
}

async fn logout(state: Data, req: HttpRequest) -> ApiResult {
    if !auth::csrf_ok(&req) {
        return Err(ApiError::csrf());
    }
    // The id token stays valid until it expires; revoke it here so a copy
    // (another tab's event stream, a stolen cookie) stops working too.
    if let Some(session) = req.cookie(auth::SESSION_COOKIE)
        && !session.value().is_empty()
    {
        state.revoked.revoke(session.value());
    }
    Ok(HttpResponse::NoContent()
        .cookie(auth::clear_cookie(&state, auth::SESSION_COOKIE, "/"))
        .cookie(auth::clear_cookie(
            &state,
            auth::REFRESH_COOKIE,
            "/api/v1/auth",
        ))
        .finish())
}

// ------------------------------------------------------------------ fleet and hosts

async fn audit_action(
    state: &AppState,
    actor: &str,
    action: &str,
    subject: &str,
    message: String,
    detail: Option<serde_json::Value>,
) {
    let entry = NewAudit {
        at: chrono::Utc::now(),
        actor: actor.to_owned(),
        action: action.to_owned(),
        outcome: "ok".into(),
        subject: Some(subject.to_owned()),
        message,
        detail,
        trace_id: None,
    };
    match state.store.call(move |db| db.audit().add(&entry)).await {
        Ok(e) => state.events.publish(LiveEvent::Audit {
            entry: buckle_api::AuditEntryRef {
                id: e.id,
                at: e.at,
                action: e.action,
                message: e.message,
            },
        }),
        Err(e) => tracing::error!(error = %e, "an audit row could not be written"),
    }
}

/// The summary rows of the targets of `kind` (`None`: every kind), with the
/// reference revision's declared clusters synced first.
async fn summaries_of(
    state: &AppState,
    kind: Option<TargetKind>,
) -> Result<Vec<buckle_api::HostSummary>, ApiError> {
    let fleet = crate::views::reference_revision_synced(&state.store, &state.revisions).await;
    let rows = state
        .store
        .call(move |db| {
            let records = match kind {
                Some(TargetKind::Host) => db.hosts().list()?,
                Some(TargetKind::Cluster) => db.hosts().clusters()?,
                None => db.hosts().targets()?,
            };
            let mut out = Vec::new();
            for h in records {
                let facts = db.hosts().latest_facts(&h.id)?.map(|(f, _)| f);
                out.push((h, facts.unwrap_or(serde_json::Value::Null)));
            }
            Ok(out)
        })
        .await?;
    Ok(rows
        .iter()
        .map(|(h, f)| {
            let mut s = crate::views::host_summary(h, f, fleet.as_deref());
            s.upgrade = state.upgrades.get(&h.id);
            s
        })
        .collect())
}

/// The summary rows of the hosts.
async fn summaries(state: &AppState) -> Result<Vec<buckle_api::HostSummary>, ApiError> {
    summaries_of(state, Some(TargetKind::Host)).await
}

async fn fleet(state: Data, _: Authenticated) -> ApiResult {
    let mut hosts = summaries_of(&state, None).await?;
    let clusters: Vec<_> = hosts
        .extract_if(.., |h| h.kind == TargetKind::Cluster)
        .collect();
    let (stable, active) = state
        .store
        .call(|db| {
            let stable = db.kv().get(keys::STABLE_REVISION)?;
            let active = db
                .rollouts()
                .list(50, 0)?
                .into_iter()
                .find(|r| r.is_active())
                .map(|r| r.id);
            Ok((stable, active))
        })
        .await?;
    let mut counts = FleetCounts {
        total: hosts.len() as u32,
        clusters: clusters.len() as u32,
        ..Default::default()
    };
    for h in &hosts {
        counts.online += u32::from(h.connection == buckle_api::Connection::Online);
        counts.healthy += u32::from(h.health == HealthSummary::Healthy);
        counts.failing += u32::from(h.health == HealthSummary::Failing);
        counts.drifted += u32::from(h.drifted);
        counts.pending += u32::from(h.status == HostStatus::Pending);
        counts.paused += u32::from(h.status == HostStatus::Paused);
    }
    Ok(HttpResponse::Ok().json(Fleet {
        hosts,
        clusters,
        stable_revision: stable,
        active_rollout: active,
        counts,
        server_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        desired_agent_version: Some(state.upgrades.desired().to_owned()),
    }))
}

async fn hosts(state: Data, _: Authenticated) -> ApiResult {
    Ok(HttpResponse::Ok().json(summaries(&state).await?))
}

/// `GET /targets`: every target, hosts and clusters alike.
async fn targets(state: Data, _: Authenticated) -> ApiResult {
    Ok(HttpResponse::Ok().json(summaries_of(&state, None).await?))
}

/// `POST /targets`: creates a cluster target. Hosts register themselves,
/// so only `kind: cluster` is accepted.
async fn target_create(
    state: Data,
    admin: Administrative,
    body: web::Json<CreateTarget>,
) -> ApiResult {
    let create = body.into_inner();
    let id = create.id.trim().to_owned();
    if id.is_empty() || id.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(ApiError::bad_request(
            "A target id must be a non-empty name without whitespace or '/'.",
        ));
    }
    if create.kind != TargetKind::Cluster {
        return Err(ApiError::bad_request(
            "Only cluster targets are created through the API; a host registers itself through its agent.",
        ));
    }
    let Some(spec) = create.cluster else {
        return Err(ApiError::bad_request(
            "A cluster target needs `cluster` (its kind, nodes, executors and identity).",
        ));
    };
    let fleet = crate::views::reference_revision_synced(&state.store, &state.revisions).await;
    let existing = state.store.call(|db| db.hosts().targets()).await?;
    if existing.iter().any(|t| t.id == id) {
        return Err(ApiError::conflict(format!(
            "A target named '{id}' already exists."
        )));
    }
    crate::targets::check_spec(&id, &spec, &existing, fleet.as_deref())
        .map_err(ApiError::bad_request)?;
    let actor = admin.0.actor();
    let detail = serde_json::to_value(&spec).ok();
    let new = crate::store::NewCluster {
        id: id.clone(),
        cluster: ClusterInfo {
            spec,
            declared: false,
        },
        agent_labels: create.labels,
        labels: std::collections::BTreeMap::new(),
        notes: create.notes,
        status: HostStatus::Active,
        at: chrono::Utc::now(),
    };
    state
        .store
        .call(move |db| db.hosts().create_cluster(&new))
        .await?;
    audit_action(
        &state,
        &actor,
        "target.create",
        &id,
        format!("{actor} created the cluster target {id}"),
        detail,
    )
    .await;
    let summary = host_changed(&state, &id).await?;
    Ok(HttpResponse::Created().json(summary))
}

/// `GET /targets/{id}/executor`: the job that runs an executor inside the
/// cluster, with its ACL policy, setup commands and the trust it needs.
async fn target_executor(state: Data, _: Administrative, path: web::Path<String>) -> ApiResult {
    let id = path.into_inner();
    let lookup = id.clone();
    let record = state
        .store
        .call(move |db| db.hosts().target(&lookup))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("No target '{id}'.")))?;
    let spec = record.cluster.map(|c| c.spec).ok_or_else(|| {
        ApiError::bad_request(format!(
            "'{id}' is a host; only a cluster has an executor job."
        ))
    })?;
    let rendered =
        crate::executor_job::render(&id, &spec, state.joins.settings(), &state.server_version)
            .map_err(ApiError::bad_request)?;
    Ok(HttpResponse::Ok().json(rendered))
}

async fn summary_or_404(state: &AppState, id: &str) -> Result<buckle_api::HostSummary, ApiError> {
    let mut summary = crate::views::load_summary(&state.store, &state.revisions, id)
        .await
        .ok_or_else(|| ApiError::not_found(format!("No target '{id}'.")))?;
    summary.upgrade = state.upgrades.get(id);
    Ok(summary)
}

/// The audit noun for a target: `host.*` actions for hosts, `target.*` for
/// cluster targets.
fn audit_noun(kind: TargetKind) -> &'static str {
    match kind {
        TargetKind::Host => "host",
        TargetKind::Cluster => "target",
    }
}

fn resources(row: Option<crate::store::RolloutHostRecord>) -> Vec<ResourceSummary> {
    let Some(row) = row else { return vec![] };
    let Some(plan) = row
        .plan
        .clone()
        .and_then(|p| serde_json::from_value::<buckle_core::plan::Plan>(p).ok())
    else {
        return vec![];
    };
    plan.steps()
        .map(|s| ResourceSummary {
            module: s.resource.module.clone(),
            kind: s.resource.kind.clone(),
            key: s.resource.key.clone(),
            revision: row.runtime.revision.clone(),
            applied_at: row.updated_at,
            removal: format!("{:?}", s.removal).to_lowercase(),
        })
        .collect()
}

fn run_dto(state: &AppState, r: crate::store::RunRecord) -> RunSummary {
    let running = r.is_running();
    RunSummary {
        state: Some(crate::views::run_state(&r, true)),
        id: r.id,
        trace_url: state.trace_url(r.trace_id.as_deref()),
        revision: r.revision,
        plan_digest: r.plan_digest,
        started_at: r.started_at,
        // A run in progress has no duration yet: the UI reads
        // finished_at <= started_at as such.
        finished_at: if running { r.started_at } else { r.finished_at },
        outcome: r.outcome,
        changes: r.changes,
        removals: r.removals,
        error: r.error,
        trace_id: r.trace_id,
    }
}

fn health_dto(h: crate::store::HealthEventRecord) -> HealthResultDto {
    HealthResultDto {
        scope: h.scope,
        module: h.module,
        check: h.check,
        kind: h.kind,
        required: h.required,
        status: h.status,
        reason: h.reason,
        at: h.at,
        rollout_id: h.rollout,
    }
}

/// The most health results a run report carries: the newest of its window,
/// which for the host's latest run is open-ended.
const RUN_HEALTH_LIMIT: u32 = 2000;

/// The report of a stored run: its rollout's plan (when the run follows
/// one), its events, its outcome and the health results recorded from its
/// start until the host's next run (bake, pre-phase and drift readings).
async fn stored_report(
    state: &AppState,
    run: crate::store::RunRecord,
) -> Result<buckle_api::RunReport, ApiError> {
    let r = run.clone();
    let (events, plan, health) = state
        .store
        .call(move |db| {
            let plan = match &r.rollout {
                Some(rollout) => db
                    .rollouts()
                    .host(rollout, &r.host)?
                    .filter(|h| h.plan_digest.as_deref() == Some(r.plan_digest.as_str()))
                    .and_then(|h| h.plan),
                None => None,
            };
            let until = db.runs().next_after(&r)?.map(|n| n.started_at);
            Ok((
                db.runs().events(&r.id)?,
                plan,
                db.health()
                    .between(&r.host, r.started_at, until, RUN_HEALTH_LIMIT)?,
            ))
        })
        .await?;
    use serde::Deserialize as _;
    let plan = plan.and_then(|p| buckle_core::plan::Plan::deserialize(&p).ok());
    let outcome: Option<buckle_core::engine::ApplyOutcome> = run
        .outcome_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok());
    // Baseline readings judge the revision before this one.
    let health: Vec<HealthResultDto> = health
        .into_iter()
        .filter(|h| matches!(h.scope.as_str(), "bake" | "rollout" | "drift"))
        .map(health_dto)
        .collect();
    let mut report =
        crate::views::run_report(&run, plan.as_ref(), &events, outcome.as_ref(), &health);
    report.trace_url = state.trace_url(report.trace_id.as_deref());
    Ok(report)
}

async fn host_detail(state: Data, _: Authenticated, path: web::Path<String>) -> ApiResult {
    let id = path.into_inner();
    let summary = summary_or_404(&state, &id).await?;
    let (facts, row, runs, health, checks, drift, latest) = state
        .store
        .call(move |db| {
            Ok((
                db.hosts().latest_facts(&id)?,
                db.rollouts().latest_healthy_for_host(&id)?,
                // Newest first, the run in progress included.
                db.runs().page(&id, None, 20)?.0,
                db.health().for_host(&id, 100)?,
                db.health().checks(&id)?,
                db.drift().for_host(&id)?,
                db.runs().latest_for_host(&id)?,
            ))
        })
        .await?;
    let latest_run = match latest {
        Some(run) => Some(stored_report(&state, run).await?),
        None => None,
    };
    Ok(HttpResponse::Ok().json(HostDetail {
        summary,
        facts: facts
            .as_ref()
            .map(|(f, _)| f.clone())
            .unwrap_or(serde_json::Value::Null),
        facts_at: facts.map(|(_, at)| at),
        resources: resources(row),
        drift,
        recent_runs: runs.into_iter().map(|r| run_dto(&state, r)).collect(),
        health_results: health.into_iter().map(health_dto).collect(),
        health_checks: checks,
        latest_run,
    }))
}

async fn host_facts(state: Data, _: Authenticated, path: web::Path<String>) -> ApiResult {
    let id = path.into_inner();
    summary_or_404(&state, &id).await?;
    let facts = state
        .store
        .call(move |db| db.hosts().latest_facts(&id))
        .await?;
    Ok(HttpResponse::Ok().json(facts.map(|(f, _)| f).unwrap_or(serde_json::Value::Null)))
}

async fn host_state(state: Data, _: Authenticated, path: web::Path<String>) -> ApiResult {
    let id = path.into_inner();
    summary_or_404(&state, &id).await?;
    let row = state
        .store
        .call(move |db| db.rollouts().latest_healthy_for_host(&id))
        .await?;
    Ok(HttpResponse::Ok().json(resources(row)))
}

async fn host_health(state: Data, _: Authenticated, path: web::Path<String>) -> ApiResult {
    let id = path.into_inner();
    summary_or_404(&state, &id).await?;
    let rows = state
        .store
        .call(move |db| db.health().for_host(&id, 200))
        .await?;
    Ok(HttpResponse::Ok().json(rows.into_iter().map(health_dto).collect::<Vec<_>>()))
}

#[derive(Deserialize)]
struct RunsQuery {
    /// A run id: the page starts after it (older runs).
    #[serde(default)]
    before: Option<String>,
    #[serde(default)]
    limit: Option<u32>,
}

/// A page of the host's runs, the running one included, newest first.
async fn host_runs(
    state: Data,
    _: Authenticated,
    path: web::Path<String>,
    q: web::Query<RunsQuery>,
) -> ApiResult {
    let id = path.into_inner();
    summary_or_404(&state, &id).await?;
    let q = q.into_inner();
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let (rows, total) = state
        .store
        .call(move |db| db.runs().page(&id, q.before.as_deref(), limit))
        .await?;
    let next = (rows.len() as u32 == limit)
        .then(|| rows.last().map(|r| r.id.clone()))
        .flatten();
    let items: Vec<RunSummary> = rows.into_iter().map(|r| run_dto(&state, r)).collect();
    Ok(HttpResponse::Ok().json(Page { items, next, total }))
}

/// One run of a host, module by module and step by step.
async fn host_run(state: Data, _: Authenticated, path: web::Path<(String, String)>) -> ApiResult {
    let (id, run) = path.into_inner();
    summary_or_404(&state, &id).await?;
    let (h, r) = (id.clone(), run.clone());
    let found = state.store.call(move |db| db.runs().get(&h, &r)).await?;
    let run =
        found.ok_or_else(|| ApiError::not_found(format!("No run '{run}' on host '{id}'.")))?;
    Ok(HttpResponse::Ok().json(stored_report(&state, run).await?))
}

async fn host_changed(state: &AppState, id: &str) -> Result<buckle_api::HostSummary, ApiError> {
    state.controller.host_changed(id);
    let summary = summary_or_404(state, id).await?;
    state.events.publish(LiveEvent::HostChanged {
        host: summary.clone(),
    });
    Ok(summary)
}

async fn host_patch(
    state: Data,
    admin: Administrative,
    path: web::Path<String>,
    body: web::Json<HostPatch>,
) -> ApiResult {
    let id = path.into_inner();
    let current = summary_or_404(&state, &id).await?;
    let patch = body.into_inner();
    let cluster = match (&patch.cluster, &current.cluster) {
        (None, _) => None,
        (Some(_), None) => {
            return Err(ApiError::bad_request(format!(
                "'{id}' is a host; only a cluster target has a `cluster` spec."
            )));
        }
        (Some(_), Some(info)) if info.declared => {
            return Err(ApiError::conflict(format!(
                "The cluster target '{id}' is declared in buckle.yaml; change it there."
            )));
        }
        (Some(spec), Some(_)) => {
            let fleet =
                crate::views::reference_revision_synced(&state.store, &state.revisions).await;
            let existing = state.store.call(|db| db.hosts().targets()).await?;
            crate::targets::check_spec(&id, spec, &existing, fleet.as_deref())
                .map_err(ApiError::bad_request)?;
            Some(ClusterInfo {
                spec: spec.clone(),
                declared: false,
            })
        }
    };
    let (i2, p2, seed) = (id.clone(), patch.clone(), current.agent_labels.clone());
    state
        .store
        .call(move |db| {
            if let Some(labels) = &p2.labels {
                db.hosts().set_labels(&i2, labels)?;
            }
            if let Some(notes) = &p2.notes {
                db.hosts().set_notes(&i2, notes)?;
            }
            if let Some(info) = &cluster {
                db.hosts().set_cluster(&i2, info, &seed)?;
            }
            Ok(())
        })
        .await?;
    let actor = admin.0.actor();
    audit_action(
        &state,
        &actor,
        &format!("{}.update", audit_noun(current.kind)),
        &id,
        format!("{actor} updated {id}"),
        serde_json::to_value(&patch).ok(),
    )
    .await;
    Ok(HttpResponse::Ok().json(host_changed(&state, &id).await?))
}

async fn host_forget(state: Data, admin: Administrative, path: web::Path<String>) -> ApiResult {
    let id = path.into_inner();
    let i2 = id.clone();
    let current = state
        .store
        .call(move |db| db.hosts().target(&i2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("No target '{id}'.")))?;
    if current.cluster.as_ref().is_some_and(|c| c.declared) {
        return Err(ApiError::conflict(format!(
            "The cluster target '{id}' is declared in buckle.yaml; remove it there."
        )));
    }
    let i2 = id.clone();
    if !state.store.call(move |db| db.hosts().delete(&i2)).await? {
        return Err(ApiError::not_found(format!("No target '{id}'.")));
    }
    state.upgrades.forget(&id);
    let actor = admin.0.actor();
    audit_action(
        &state,
        &actor,
        &format!("{}.forget", audit_noun(current.kind)),
        &id,
        format!("{actor} forgot {id}"),
        None,
    )
    .await;
    // A forgotten host keeps no session: its agent reconnects and, being
    // unknown, registers afresh.
    if state.agent_sessions.close(&id) {
        tracing::info!(buckle.host = %id, "closed the session of a forgotten host");
    }
    state.controller.host_changed(&id);
    Ok(HttpResponse::NoContent().finish())
}

async fn host_action(
    state: Data,
    admin: Administrative,
    path: web::Path<(String, String)>,
) -> ApiResult {
    let (id, action) = path.into_inner();
    let current = summary_or_404(&state, &id).await?;
    let target = match (action.as_str(), current.status) {
        ("approve", HostStatus::Pending) => HostStatus::Active,
        ("pause", HostStatus::Active | HostStatus::Pending) => HostStatus::Paused,
        ("resume", HostStatus::Paused) => HostStatus::Active,
        ("approve" | "pause" | "resume", s) => {
            return Err(ApiError::conflict(format!(
                "Target '{id}' is {s:?}; it cannot be {action}d."
            )));
        }
        _ => return Err(ApiError::not_found(format!("No host action '{action}'."))),
    };
    let i2 = id.clone();
    state
        .store
        .call(move |db| db.hosts().set_status(&i2, target))
        .await?;
    let actor = admin.0.actor();
    audit_action(
        &state,
        &actor,
        &format!("{}.{action}", audit_noun(current.kind)),
        &id,
        format!("{actor} {action}d {id}"),
        None,
    )
    .await;
    Ok(HttpResponse::Ok().json(host_changed(&state, &id).await?))
}

/// Asks the host's agent to install a release once idle and restart into it.
async fn host_upgrade(
    state: Data,
    admin: Administrative,
    path: web::Path<String>,
    body: Option<web::Json<UpgradeRequest>>,
) -> ApiResult {
    use crate::upgrades::RequestError;
    let id = path.into_inner();
    let current = summary_or_404(&state, &id).await?;
    let version = body.and_then(|b| b.into_inner().version);
    let actor = admin.0.actor();
    let upgrade = state
        .upgrades
        .request(
            &id,
            current.agent_version.as_deref(),
            version.as_deref(),
            &actor,
            chrono::Utc::now(),
        )
        .map_err(|e| match e {
            RequestError::InvalidVersion(v) => ApiError::bad_request(format!(
                "'{v}' is not a release version; use MAJOR.MINOR.PATCH, such as 2.3.0."
            )),
            RequestError::Current(v) => {
                ApiError::conflict(format!("{id}'s agent already runs {v}."))
            }
            RequestError::Offline => ApiError::conflict(format!(
                "{id} is offline; its agent can be upgraded once it reconnects."
            )),
        })?;
    tracing::info!(buckle.host = %id, version = %upgrade.version, "asked the agent to upgrade");
    audit_action(
        &state,
        &actor,
        "host.upgrade",
        &id,
        format!(
            "{actor} asked {id} to upgrade its agent from {} to {}",
            current
                .agent_version
                .as_deref()
                .unwrap_or("an unknown version"),
            upgrade.version
        ),
        None,
    )
    .await;
    // Nothing about the host's record changed, so the controller is not told.
    let summary = summary_or_404(&state, &id).await?;
    state.events.publish(LiveEvent::HostChanged {
        host: summary.clone(),
    });
    Ok(HttpResponse::Ok().json(summary))
}

fn join_error(e: crate::join::JoinError) -> ApiError {
    use crate::join::JoinError;
    match e {
        JoinError::Invalid(m) => ApiError::bad_request(m),
        JoinError::TooMany => ApiError::too_many_because(e.to_string()),
        JoinError::NotFound => ApiError::not_found(e.to_string()),
    }
}

async fn join_options(state: Data, _: Administrative) -> ApiResult {
    Ok(HttpResponse::Ok().json(state.joins.settings().options()))
}

async fn join_create(
    state: Data,
    admin: Administrative,
    body: web::Json<buckle_api::JoinRequest>,
) -> ApiResult {
    let actor = admin.0.actor();
    let ticket = state
        .joins
        .create(body.into_inner(), &actor, chrono::Utc::now())
        .await
        .map_err(join_error)?;
    let host = ticket.host_id.as_deref().unwrap_or("a new host");
    tracing::info!(buckle.host = %host, method = ?ticket.method, "created a join command");
    audit_action(
        &state,
        &actor,
        "join.created",
        &ticket.id,
        format!(
            "{actor} created a join command for {host} ({}, {})",
            ticket.platform.label(),
            match ticket.method {
                buckle_api::JoinMethod::Tailscale => "over Tailscale",
                buckle_api::JoinMethod::Token => "with a join token",
            }
        ),
        None,
    )
    .await;
    Ok(HttpResponse::Created().json(ticket))
}

async fn join_ticket(state: Data, admin: Administrative, path: web::Path<String>) -> ApiResult {
    let ticket = state
        .joins
        .get(&path.into_inner(), &admin.0.actor(), chrono::Utc::now())
        .map_err(join_error)?;
    Ok(HttpResponse::Ok().json(ticket))
}

#[derive(Deserialize)]
struct TailscaleCallback {
    state: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

/// Where Tailscale sends the admin's browser after consent: back to the
/// hosts page, which reopens the wizard on the ticket.
async fn join_callback(
    state: Data,
    admin: Administrative,
    query: web::Query<TailscaleCallback>,
) -> ApiResult {
    let q = query.into_inner();
    let location = match state
        .joins
        .complete_tailscale(
            &q.state,
            q.code.as_deref(),
            q.error.as_deref(),
            &admin.0.actor(),
            chrono::Utc::now(),
        )
        .await
    {
        Ok(id) => format!("/hosts?join={id}"),
        Err(_) => "/hosts?join=expired".to_owned(),
    };
    Ok(HttpResponse::SeeOther()
        .insert_header(("location", location))
        .insert_header(("cache-control", "no-store"))
        .finish())
}

async fn host_preview(
    state: Data,
    _: Authenticated,
    path: web::Path<String>,
    body: web::Json<HostPatch>,
) -> ApiResult {
    let id = path.into_inner();
    let i2 = id.clone();
    let (host, facts) = state
        .store
        .call(move |db| {
            let h = db.hosts().target(&i2)?;
            let f = db.hosts().latest_facts(&i2)?.map(|(f, _)| f);
            Ok((h, f.unwrap_or(serde_json::Value::Null)))
        })
        .await?;
    let mut host = host.ok_or_else(|| ApiError::not_found(format!("No target '{id}'.")))?;
    if let Some(labels) = body.into_inner().labels {
        host.labels = labels;
    }
    let fleet = crate::views::reference_revision(&state.store, &state.revisions).await;
    let (groups, phase) = match &fleet {
        Some(rev) => {
            let ctx = crate::fleet::record_context(&host, &facts, &rev.fleet);
            let phases = crate::fleet::effective_phases(&rev.fleet.rollout);
            let phase = crate::fleet::phase_for(&ctx, &phases).map(|i| phases[i].name.clone());
            (ctx.groups, phase)
        }
        None => (vec![], None),
    };
    Ok(HttpResponse::Ok().json(HostPreview {
        effective_labels: host.effective_labels(),
        groups,
        phase,
    }))
}

// ------------------------------------------------------------------ revisions

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    limit: Option<u32>,
    #[serde(default)]
    offset: Option<u32>,
}

async fn revision_summary(
    state: &AppState,
    rec: &crate::store::RevisionRecord,
) -> Result<buckle_api::RevisionSummary, ApiError> {
    let id = rec.id.clone();
    let (stable, rollout) = state
        .store
        .call(move |db| {
            Ok((
                db.kv().get(keys::STABLE_REVISION)?,
                db.rollouts().latest_for_revision(&id)?.map(|r| r.id),
            ))
        })
        .await?;
    Ok(crate::source::summary(
        rec,
        stable.as_deref() == Some(rec.id.as_str()),
        rollout,
    ))
}

async fn revisions(state: Data, _: Authenticated, q: web::Query<PageQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let (rows, total) = state
        .store
        .call(move |db| Ok((db.revisions().list(limit, offset)?, db.revisions().count()?)))
        .await?;
    let mut items = Vec::new();
    for r in &rows {
        items.push(revision_summary(&state, r).await?);
    }
    let next = ((offset as u64 + items.len() as u64) < total)
        .then(|| (offset + items.len() as u32).to_string());
    Ok(HttpResponse::Ok().json(Page { items, next, total }))
}

async fn revision(state: Data, _: Authenticated, path: web::Path<String>) -> ApiResult {
    let id = path.into_inner();
    let i2 = id.clone();
    let rec = state
        .store
        .call(move |db| db.revisions().get(&i2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("No revision '{id}'.")))?;
    Ok(HttpResponse::Ok().json(revision_summary(&state, &rec).await?))
}

async fn start_rollout(
    state: Data,
    admin: Administrative,
    path: web::Path<String>,
    body: Option<web::Json<StartRollout>>,
) -> ApiResult {
    let revision = path.into_inner();
    let note = body.map(|b| b.into_inner().note).unwrap_or_default();
    let id = state
        .controller
        .command(&admin.0.actor(), Command::Start { revision, note })
        .await?;
    let detail = rollout_detail(&state, &id).await?;
    Ok(HttpResponse::build(StatusCode::CREATED).json(detail.summary))
}

// ------------------------------------------------------------------ rollouts

/// The id of the rollout a path names, by id or by name (`word.word.word`,
/// see [`buckle_api::RolloutName::parse`]). A key that is neither passes
/// through for the handler's own "no rollout" answer.
async fn resolve_rollout(state: &AppState, key: String) -> Result<String, ApiError> {
    let Some(name) = buckle_api::RolloutName::parse(&key) else {
        return Ok(key);
    };
    let k = key.clone();
    let (exists, ids) = state
        .store
        .call(move |db| {
            Ok((
                db.rollouts().get(&k)?.is_some(),
                db.rollouts().ids_named(&name)?,
            ))
        })
        .await?;
    match ids.as_slice() {
        _ if exists => Ok(key),
        [id] => Ok(id.clone()),
        [] => Err(ApiError::not_found(format!("No rollout named '{name}'."))),
        many => Err(ApiError::conflict(format!(
            "{} rollouts are named '{name}'; use one of their ids: {}.",
            many.len(),
            many.join(", ")
        ))),
    }
}

async fn rollout_detail(state: &AppState, id: &str) -> Result<buckle_api::RolloutDetail, ApiError> {
    let i2 = id.to_owned();
    let found = state
        .store
        .call(move |db| {
            let Some(r) = db.rollouts().get(&i2)? else {
                return Ok(None);
            };
            Ok(Some((
                r,
                db.rollouts().hosts(&i2)?,
                db.approvals().for_rollout(&i2)?,
                db.audit().for_subject(&i2, 200)?,
            )))
        })
        .await?;
    let (r, hosts, approvals, audit) =
        found.ok_or_else(|| ApiError::not_found(format!("No rollout '{id}'.")))?;
    let i3 = id.to_owned();
    let run_ids = state
        .store
        .call(move |db| db.runs().ids_for_rollout(&i3))
        .await?;
    let mut detail = view::detail(&r, &hosts, &approvals, &audit);
    for h in detail.phases.iter_mut().flat_map(|p| p.hosts.iter_mut()) {
        if h.run_id.is_none() {
            h.run_id = run_ids.get(&h.host).cloned();
        }
    }
    detail.summary = state.link_rollout(detail.summary);
    // The audit trail comes newest first; the timeline reads oldest first,
    // so live entries append.
    detail.events.reverse();
    // A group takes its first host's copy of the plan; when that host holds
    // none, any other host with the digest will do.
    for g in detail.plans.iter_mut().filter(|g| g.plan.is_none()) {
        g.plan = hosts
            .iter()
            .filter(|h| h.plan_digest.as_deref() == Some(g.digest.as_str()))
            .find_map(|h| crate::views::stored_plan_view(h.plan.as_ref()));
    }
    detail.fleet = Some(fleet_plan(state, &r.revision, &hosts).await?);
    fill_health_checks(state, &r.revision, &hosts, &mut detail).await?;
    Ok(detail)
}

/// The union of the hosts' plans of `revision`, with each host's latest
/// facts and labels for fact placeholders. Plans of another revision (the
/// stragglers stage plans the stable one) count as unplanned.
async fn fleet_plan(
    state: &AppState,
    revision: &str,
    hosts: &[crate::store::RolloutHostRecord],
) -> Result<buckle_api::FleetPlan, ApiError> {
    let ids: Vec<String> = hosts.iter().map(|h| h.host.clone()).collect();
    let contexts = state
        .store
        .call(move |db| {
            let mut out = HashMap::new();
            for id in ids {
                let facts = db.hosts().latest_facts(&id)?.map(|(f, _)| f);
                let labels = db.hosts().get(&id)?.map(|h| h.effective_labels());
                out.insert(
                    id.clone(),
                    serde_json::json!({
                        "id": id,
                        "labels": labels.unwrap_or_default(),
                        "facts": facts.unwrap_or(serde_json::Value::Null),
                    }),
                );
            }
            Ok(out)
        })
        .await?;
    let input: Vec<buckle_api::FleetHost> = hosts
        .iter()
        .map(|h| buckle_api::FleetHost {
            host: h.host.clone(),
            plan: crate::views::stored_plan_view(h.plan.as_ref())
                .filter(|p| p.revision.is_empty() || p.revision == revision),
            context: contexts.get(&h.host).cloned().unwrap_or_default(),
        })
        .collect();
    Ok(buckle_api::FleetPlan::build(&input))
}

/// Fills each phase's health gate: the required checks of the modules its
/// hosts apply, each host judged against the revision it is moving to (the
/// rollout's, or the stable one in the stragglers stage).
async fn fill_health_checks(
    state: &AppState,
    rollout_revision: &str,
    hosts: &[crate::store::RolloutHostRecord],
    detail: &mut buckle_api::RolloutDetail,
) -> Result<(), ApiError> {
    let revision_of = |h: &crate::store::RolloutHostRecord| {
        if h.runtime.revision.is_empty() {
            rollout_revision.to_owned()
        } else {
            h.runtime.revision.clone()
        }
    };
    let mut revisions = HashMap::new();
    for id in hosts.iter().map(revision_of) {
        if !revisions.contains_key(&id)
            && let Ok(rev) = state.revisions.load(&id).await
        {
            revisions.insert(id, rev);
        }
    }
    let plans: HashMap<&str, Option<buckle_api::PlanView>> = hosts
        .iter()
        .map(|h| {
            (
                h.host.as_str(),
                crate::views::stored_plan_view(h.plan.as_ref()),
            )
        })
        .collect();
    // Hosts without a plan are judged by their labels and facts.
    let unplanned: Vec<String> = plans
        .iter()
        .filter(|(_, p)| p.is_none())
        .map(|(h, _)| (*h).to_owned())
        .collect();
    let contexts = state
        .store
        .call(move |db| {
            let mut out = HashMap::new();
            for id in unplanned {
                if let Some(h) = db.hosts().get(&id)? {
                    let facts = db.hosts().latest_facts(&id)?.map(|(f, _)| f);
                    out.insert(id, (h, facts.unwrap_or(serde_json::Value::Null)));
                }
            }
            Ok(out)
        })
        .await?;
    for phase in &mut detail.phases {
        let mut checks: Vec<String> = Vec::new();
        for entry in &phase.hosts {
            let Some(record) = hosts.iter().find(|h| h.host == entry.host) else {
                continue;
            };
            let Some(revision) = revisions.get(&revision_of(record)) else {
                continue;
            };
            let selected = crate::views::selected_modules(
                revision,
                plans.get(record.host.as_str()).and_then(Option::as_ref),
                contexts.get(&record.host).map(|(h, f)| (h, f)),
            );
            for check in
                crate::views::required_checks(revision, selected.iter().map(String::as_str))
            {
                if !checks.contains(&check) {
                    checks.push(check);
                }
            }
        }
        phase.health_checks = checks;
    }
    Ok(())
}

async fn rollouts(state: Data, _: Authenticated, q: web::Query<PageQuery>) -> ApiResult {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let (items, total) = state
        .store
        .call(move |db| {
            let mut out = Vec::new();
            for r in db.rollouts().list(limit, offset)? {
                let hosts = db.rollouts().hosts(&r.id)?;
                out.push(view::summary(&r, &hosts));
            }
            Ok((out, db.rollouts().count()?))
        })
        .await?;
    let items: Vec<_> = items.into_iter().map(|r| state.link_rollout(r)).collect();
    let next = ((offset as u64 + items.len() as u64) < total)
        .then(|| (offset + items.len() as u32).to_string());
    Ok(HttpResponse::Ok().json(Page { items, next, total }))
}

async fn rollout(state: Data, _: Authenticated, path: web::Path<String>) -> ApiResult {
    let id = resolve_rollout(&state, path.into_inner()).await?;
    Ok(HttpResponse::Ok().json(rollout_detail(&state, &id).await?))
}

/// The host's run in a rollout: the plan as pending before it starts, live
/// while it applies, final once its result arrived.
async fn rollout_host_report(
    state: Data,
    _: Authenticated,
    path: web::Path<(String, String)>,
) -> ApiResult {
    let (rollout, host) = path.into_inner();
    let rollout = resolve_rollout(&state, rollout).await?;
    let (r, h) = (rollout.clone(), host.clone());
    let (row, run) = state
        .store
        .call(move |db| {
            Ok((
                db.rollouts().host(&r, &h)?,
                db.runs().latest_for_rollout_host(&r, &h)?,
            ))
        })
        .await?;
    let row = row
        .ok_or_else(|| ApiError::not_found(format!("No host '{host}' in rollout '{rollout}'.")))?;
    use serde::Deserialize as _;
    let plan = row
        .plan
        .as_ref()
        .filter(|p| !p.is_null())
        .and_then(|p| buckle_core::plan::Plan::deserialize(p).ok());
    let report = match (run, plan) {
        // The run of the host's current plan (live or finished).
        (Some(run), Some(plan)) if run.plan_digest == plan.digest.to_string() => {
            stored_report(&state, run).await?
        }
        // A plan the host has not started applying yet.
        (_, Some(plan)) => crate::views::pending_report(
            &host,
            &row.runtime.revision,
            Some(&rollout),
            &plan,
            row.updated_at,
        ),
        (Some(run), None) => stored_report(&state, run).await?,
        (None, None) => {
            return Err(ApiError::not_found(format!(
                "Host '{host}' has no plan in rollout '{rollout}' yet."
            )));
        }
    };
    Ok(HttpResponse::Ok().json(report))
}

async fn rollout_plans(state: Data, _: Authenticated, path: web::Path<String>) -> ApiResult {
    let id = resolve_rollout(&state, path.into_inner()).await?;
    Ok(HttpResponse::Ok().json(rollout_detail(&state, &id).await?.plans))
}

async fn rollout_approve(
    state: Data,
    admin: Administrative,
    path: web::Path<String>,
    body: web::Json<ApproveRequest>,
) -> ApiResult {
    let id = resolve_rollout(&state, path.into_inner()).await?;
    let b = body.into_inner();
    state
        .controller
        .command(
            &admin.0.actor(),
            Command::Approve {
                rollout: id.clone(),
                phase: b.phase,
                digests: b.plan_digests,
                note: b.note,
            },
        )
        .await?;
    Ok(HttpResponse::Ok().json(rollout_detail(&state, &id).await?))
}

/// `POST /rollouts/{id}/reject`: valid only while the rollout awaits
/// approval (409 otherwise). The controller's reject command checks the
/// state and ends the rollout `aborted` with the reason in one step (no
/// check-then-abort race), auditing `rollout.reject` with the reason.
async fn rollout_reject(
    state: Data,
    admin: Administrative,
    path: web::Path<String>,
    body: web::Json<RejectRequest>,
) -> ApiResult {
    let id = resolve_rollout(&state, path.into_inner()).await?;
    let reason = body.into_inner().reason.trim().to_owned();
    if reason.is_empty() {
        return Err(ApiError::bad_request(
            "Say why the rollout is rejected: the reason is recorded in the audit log.",
        ));
    }
    let i2 = id.clone();
    let record = state
        .store
        .call(move |db| db.rollouts().get(&i2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("No rollout '{id}'.")))?;
    // A finished rollout is not live in the controller (it would answer 404);
    // the controller re-checks the state atomically.
    if record.state != RolloutState::AwaitingApproval {
        return Err(ApiError::conflict(format!(
            "Rollout {id} is not awaiting approval, so there is nothing to reject; abort it instead."
        )));
    }
    state
        .controller
        .command(
            &admin.0.actor(),
            Command::Reject {
                rollout: id.clone(),
                reason,
            },
        )
        .await?;
    Ok(HttpResponse::Ok().json(rollout_detail(&state, &id).await?))
}

async fn rollout_action(
    state: Data,
    admin: Administrative,
    path: web::Path<(String, String)>,
    body: Option<web::Json<ActionRequest>>,
) -> ApiResult {
    let (id, action) = path.into_inner();
    let id = resolve_rollout(&state, id).await?;
    let note = body.map(|b| b.into_inner().note).unwrap_or_default();
    let rollout = id.clone();
    let command = match action.as_str() {
        "pause" => Command::Pause { rollout, note },
        "resume" => Command::Resume { rollout, note },
        "retry" => Command::RetryFailed { rollout, note },
        "skip-bake" => Command::SkipBake {
            rollout,
            host: None,
            note,
        },
        "rollback" => Command::Rollback { rollout, note },
        "abort" => Command::Abort { rollout, note },
        _ => {
            return Err(ApiError::not_found(format!(
                "No rollout action '{action}'."
            )));
        }
    };
    state.controller.command(&admin.0.actor(), command).await?;
    Ok(HttpResponse::Ok().json(rollout_detail(&state, &id).await?))
}

async fn rollout_host_action(
    state: Data,
    admin: Administrative,
    path: web::Path<(String, String, String)>,
    body: Option<web::Json<ActionRequest>>,
) -> ApiResult {
    let (id, host, action) = path.into_inner();
    let id = resolve_rollout(&state, id).await?;
    let note = body.map(|b| b.into_inner().note).unwrap_or_default();
    let rollout = id.clone();
    let command = match action.as_str() {
        "retry" => Command::Retry {
            rollout,
            host,
            note,
        },
        "skip" => Command::Skip {
            rollout,
            host,
            note,
        },
        "force" => Command::Force {
            rollout,
            host,
            note,
        },
        "skip-bake" => Command::SkipBake {
            rollout,
            host: Some(host),
            note,
        },
        _ => return Err(ApiError::not_found(format!("No host action '{action}'."))),
    };
    state.controller.command(&admin.0.actor(), command).await?;
    Ok(HttpResponse::Ok().json(rollout_detail(&state, &id).await?))
}

async fn rollout_continue(
    state: Data,
    admin: Administrative,
    path: web::Path<(String, String)>,
    body: web::Json<ContinueRequest>,
) -> ApiResult {
    let (id, host) = path.into_inner();
    let id = resolve_rollout(&state, id).await?;
    let b = body.into_inner();
    state
        .controller
        .command(
            &admin.0.actor(),
            Command::Continue {
                rollout: id.clone(),
                host,
                waive: b.waive_checks,
                note: b.note,
            },
        )
        .await?;
    Ok(HttpResponse::Ok().json(rollout_detail(&state, &id).await?))
}

// ------------------------------------------------------------------ events, catalog, audit, webhook

/// An event as a client sees it: links filled in where the server knows
/// them (the controller publishes rollouts without).
fn for_client(state: &AppState, event: LiveEvent) -> LiveEvent {
    match event {
        LiveEvent::RolloutChanged { rollout } => LiveEvent::RolloutChanged {
            rollout: state.link_rollout(rollout),
        },
        other => other,
    }
}

fn sse_frame(event: &LiveEvent) -> web::Bytes {
    let json = serde_json::to_string(event).unwrap_or_else(|_| "{}".into());
    let kind = serde_json::to_value(event)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "message".into());
    web::Bytes::from(format!("event: {kind}\ndata: {json}\n\n"))
}

/// One open event stream: what it reads from and the request it re-checks.
struct EventStream {
    rx: tokio::sync::broadcast::Receiver<LiveEvent>,
    state: Data,
    req: HttpRequest,
    next_check: tokio::time::Instant,
    ended: bool,
}

async fn events(state: Data, req: HttpRequest, _: Authenticated) -> HttpResponse {
    let rx = state.events.subscribe();
    let first = futures::stream::once(async {
        Ok::<_, actix_web::Error>(web::Bytes::from_static(b": connected\n\n"))
    });
    let recheck = state.sse_recheck;
    let keepalive = Duration::from_secs(15);
    let stream = EventStream {
        rx,
        next_check: tokio::time::Instant::now() + recheck,
        state: state.clone(),
        req,
        ended: false,
    };
    // The session is re-checked every `sse_recheck` (expiry, sign-out and
    // the ACLs as they are now); a stream that no longer passes says so and
    // ends, and the browser's reconnect then meets the usual 401/403.
    let rest = futures::stream::unfold(stream, move |mut s| async move {
        if s.ended {
            return None;
        }
        let now = tokio::time::Instant::now();
        if now >= s.next_check {
            if auth::resolve(&s.state, &s.req).await.is_err() {
                s.ended = true;
                let bye = web::Bytes::from_static(b"event: session_ended\ndata: {}\n\n");
                return Some((Ok::<_, actix_web::Error>(bye), s));
            }
            s.next_check = tokio::time::Instant::now() + recheck;
        }
        let wait = keepalive.min(s.next_check.saturating_duration_since(now));
        let frame = match tokio::time::timeout(wait, s.rx.recv()).await {
            Ok(Ok(ev)) => sse_frame(&for_client(&s.state, ev)),
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                web::Bytes::from_static(b"event: lagged\ndata: {}\n\n")
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => return None,
            Err(_) => web::Bytes::from_static(b": keepalive\n\n"),
        };
        Some((Ok::<_, actix_web::Error>(frame), s))
    });
    use futures::StreamExt as _;
    HttpResponse::Ok()
        .content_type("text/event-stream")
        .insert_header(("cache-control", "no-cache"))
        .insert_header(("x-accel-buffering", "no"))
        .streaming(first.chain(rest))
}

async fn catalog(state: Data, _: Authenticated) -> HttpResponse {
    HttpResponse::Ok().json(&state.catalog)
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    before: Option<u64>,
    #[serde(default)]
    limit: Option<u32>,
    /// `false` leaves out controller progress transitions
    /// (`buckle_api::PROGRESS_ACTIONS`); absent or `true` keeps them.
    #[serde(default)]
    progress: Option<bool>,
}

async fn audit(state: Data, _: Authenticated, q: web::Query<AuditQuery>) -> ApiResult {
    let q = q.into_inner();
    let filter = match q.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        Some(expr) => Some(
            buckle_core::Filter::new(expr)
                .map_err(|e| ApiError::bad_request(format!("The filter is not valid: {e}")))?,
        ),
        None => None,
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let page = state
        .store
        .call(move |db| {
            db.audit().page(&AuditFilter {
                filter,
                before: q.before,
                limit,
                hide_progress: q.progress == Some(false),
            })
        })
        .await?;
    Ok(HttpResponse::Ok().json(Page {
        items: page.items,
        next: page.next.map(|n| n.to_string()),
        total: page.total,
    }))
}

async fn webhook(state: Data, req: HttpRequest, body: web::Bytes) -> ApiResult {
    let Some(secret) = &state.webhook_secret else {
        return Err(ApiError::not_found("No webhook secret is configured."));
    };
    let signature = req
        .headers()
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !crate::source::verify_webhook_signature(secret, &body, signature) {
        return Err(ApiError {
            status: StatusCode::UNAUTHORIZED,
            body: buckle_api::ErrorBody {
                error: "The webhook signature is not valid.".into(),
                advice: vec![
                    "Configure the webhook with the same secret as source.git.webhook_secret."
                        .into(),
                ],
            },
        });
    }
    if let Some(t) = &state.source_trigger {
        t.notify_one();
    }
    Ok(HttpResponse::Accepted().finish())
}

#[allow(dead_code)]
fn principal(req: &HttpRequest) -> Option<auth::Principal> {
    req.extensions().get::<auth::Principal>().cloned()
}
