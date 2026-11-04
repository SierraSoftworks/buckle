//! The HTTP front (design §12): the operator API, live events, sign-in and
//! the embedded UI, on actix-web.

pub mod auth;
pub mod catalog;
mod error;
pub mod logger;
pub mod oidc;
mod routes;
pub mod timeline;
mod ui;
mod wellknown;

pub use error::ApiError;

use crate::config::WebConfig;
use crate::events::EventBus;
use crate::rollout::ControllerHandle;
use crate::source::cache::RevisionCache;
use crate::store::Store;
use actix_web::body::MessageBody;
use actix_web::dev::{ServiceFactory, ServiceRequest, ServiceResponse};
use actix_web::middleware::from_fn;
use actix_web::{App, web};
use std::sync::Arc;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// Everything the handlers need.
pub struct AppState {
    pub store: Store,
    pub revisions: Arc<RevisionCache>,
    pub controller: ControllerHandle,
    pub events: EventBus,
    pub web: WebConfig,
    pub oidc: Option<Arc<oidc::OidcClient>>,
    pub pending: auth::PendingAuths,
    /// Sessions signed out before their token expired.
    pub revoked: auth::RevokedSessions,
    pub limiter: auth::RateLimiter,
    pub webhook_secret: Option<String>,
    pub source_trigger: Option<Arc<Notify>>,
    pub catalog: buckle_api::Catalog,
    /// Cookies carry `Secure` (and the binding cookie `__Host-`).
    pub secure_cookies: bool,
    pub server_version: String,
    /// Live agent sessions, so forgetting a host closes its stream.
    pub agent_sessions: Arc<crate::grpc::LiveSessions>,
    /// Agent upgrades requested here, sent on the hosts' sessions.
    pub upgrades: Arc<crate::upgrades::AgentUpgrades>,
    /// Join commands for new hosts, shared with the gRPC enrolment.
    pub joins: Arc<crate::join::Joins>,
    /// How often an event stream re-checks the session it was opened with.
    pub sse_recheck: std::time::Duration,
    /// `telemetry.trace_url_template`: turns a trace id into a link.
    pub trace_url_template: Option<String>,
    /// The identity issuer whose keys `/.well-known/` publishes.
    pub identity: Option<Arc<crate::identity::Issuer>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState").finish_non_exhaustive()
    }
}

impl AppState {
    /// State with defaults derived from `web` (OIDC client, cookie security).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        store: Store,
        revisions: Arc<RevisionCache>,
        controller: ControllerHandle,
        events: EventBus,
        web: WebConfig,
        webhook_secret: Option<String>,
        source_trigger: Option<Arc<Notify>>,
        server_version: String,
    ) -> Self {
        let secure_cookies = web
            .base_url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"));
        Self {
            oidc: web.oidc.clone().map(|c| Arc::new(oidc::OidcClient::new(c))),
            store,
            revisions,
            controller,
            events,
            web,
            pending: auth::PendingAuths::default(),
            revoked: auth::RevokedSessions::default(),
            limiter: auth::RateLimiter::new(30, std::time::Duration::from_secs(60)),
            webhook_secret,
            source_trigger,
            catalog: catalog::build(&server_version),
            secure_cookies,
            server_version,
            agent_sessions: Default::default(),
            upgrades: Default::default(),
            joins: Default::default(),
            sse_recheck: std::time::Duration::from_secs(30),
            trace_url_template: None,
            identity: None,
        }
    }

    /// The link to `trace_id` in the tracing UI, when a template is set.
    pub fn trace_url(&self, trace_id: Option<&str>) -> Option<String> {
        let (template, id) = (self.trace_url_template.as_deref()?, trace_id?);
        (!id.is_empty()).then(|| crate::config::trace_url(template, id))
    }

    /// Fills `trace_url` on a rollout summary from its `trace_id`.
    pub fn link_rollout(
        &self,
        mut summary: buckle_api::RolloutSummary,
    ) -> buckle_api::RolloutSummary {
        if summary.trace_url.is_none() {
            summary.trace_url = self.trace_url(summary.trace_id.as_deref());
        }
        summary
    }

    /// The OIDC redirect URI.
    pub fn redirect_uri(&self) -> String {
        format!(
            "{}/api/v1/auth/callback",
            self.web
                .base_url
                .as_deref()
                .unwrap_or("")
                .trim_end_matches('/')
        )
    }
}

/// The actix application.
pub fn app(
    state: web::Data<AppState>,
) -> App<
    impl ServiceFactory<
        ServiceRequest,
        Config = (),
        Response = ServiceResponse<impl MessageBody>,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(state)
        .app_data(
            web::JsonConfig::default()
                .limit(1 << 20)
                .error_handler(|err, _| {
                    ApiError::bad_request(format!("The request body is not valid: {err}")).into()
                }),
        )
        .app_data(web::QueryConfig::default().error_handler(|err, _| {
            ApiError::bad_request(format!("The query string is not valid: {err}")).into()
        }))
        .wrap(from_fn(auth::middleware))
        .wrap(from_fn(logger::middleware))
        .configure(routes::configure)
        .configure(wellknown::configure)
        .configure(ui::configure)
}

/// Serves the HTTP API on `listener` until `cancel` trips.
pub async fn serve(
    state: AppState,
    listener: std::net::TcpListener,
    cancel: CancellationToken,
) -> buckle_core::Result<()> {
    for warning in state.web.startup_warnings() {
        tracing::warn!("{warning}");
    }
    let relay = timeline::spawn(state.store.clone(), state.events.clone(), cancel.clone());
    let state = web::Data::new(state);
    let server = actix_web::HttpServer::new(move || app(state.clone()))
        .disable_signals()
        .shutdown_timeout(10)
        .listen(listener)
        .map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                "The HTTP listener could not be started.",
                &["Check listen.http in server.yaml."],
            )
        })?
        .run();
    let handle = server.handle();
    tokio::spawn(async move {
        cancel.cancelled().await;
        handle.stop(true).await;
    });
    let served = server.await;
    relay.abort();
    served.map_err(|e| {
        buckle_core::error::wrap_system(
            e,
            "The HTTP server stopped unexpectedly.",
            &["Check the server logs."],
        )
    })
}

#[cfg(test)]
mod tests;
