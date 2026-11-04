//! `/.well-known/`: the identity issuer's discovery document, JWKS and
//! public keys, unauthenticated, for clusters that trust Buckle-issued tokens.

use super::AppState;
use crate::identity::Issuer;
use actix_web::{HttpResponse, web};

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route(
        "/.well-known/openid-configuration",
        web::get().to(discovery),
    )
    .route("/.well-known/jwks.json", web::get().to(jwks))
    .route("/.well-known/buckle-identity.pem", web::get().to(pem));
}

/// `render`'s document from the issuer, or 404 when the server has none.
fn serve(
    state: &AppState,
    content_type: &str,
    render: impl FnOnce(&Issuer) -> String,
) -> HttpResponse {
    match state.identity.as_deref() {
        Some(issuer) => HttpResponse::Ok()
            .content_type(content_type)
            // Short enough that a rotated key is seen well within a token TTL.
            .insert_header(("cache-control", "public, max-age=300"))
            .body(render(issuer)),
        None => HttpResponse::NotFound().body("This server does not issue identities."),
    }
}

async fn discovery(state: web::Data<AppState>) -> HttpResponse {
    serve(&state, "application/json", |i| i.discovery().to_string())
}

async fn jwks(state: web::Data<AppState>) -> HttpResponse {
    serve(&state, "application/jwk-set+json", |i| i.jwks().to_string())
}

async fn pem(state: web::Data<AppState>) -> HttpResponse {
    serve(&state, "application/x-pem-file", Issuer::public_pem)
}
