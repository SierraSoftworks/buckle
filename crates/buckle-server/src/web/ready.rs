//! `GET /readyz`: whether this server can do its work, for whatever runs
//! it (a scheduler's readiness check decides if an upgrade took).
//!
//! Ready means the store answers, the rollout controller is running and
//! the agent listener is serving. Anything else is a 503 naming what is
//! not, so a deployment of a server that cannot serve its fleet fails
//! instead of being taken for healthy because its process is up.

use super::AppState;
use actix_web::{HttpResponse, web};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// How long the store may take to answer.
const STORE_TIMEOUT: Duration = Duration::from_secs(2);

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/readyz", web::get().to(ready));
}

async fn ready(state: web::Data<AppState>) -> HttpResponse {
    let store = tokio::time::timeout(
        STORE_TIMEOUT,
        state.store.call(|db| {
            db.conn()
                .query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
                .map_err(crate::store::db_error)
        }),
    )
    .await
    .is_ok_and(|r| r.is_ok());
    let checks = [
        ("store", store),
        ("controller", state.controller.is_running()),
        (
            "agents",
            state
                .agents_serving
                .as_ref()
                .is_none_or(|s| s.load(Ordering::Relaxed)),
        ),
    ];
    let failing: Vec<&str> = checks.iter().filter(|c| !c.1).map(|c| c.0).collect();
    let body = serde_json::json!({
        "ready": failing.is_empty(),
        "version": state.server_version,
        "failing": failing,
    });
    if failing.is_empty() {
        HttpResponse::Ok().json(body)
    } else {
        HttpResponse::ServiceUnavailable().json(body)
    }
}
