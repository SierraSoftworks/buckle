//! The embedded UI with an SPA fallback, and `/robots.txt`.

use actix_web::{HttpRequest, HttpResponse, web};
use include_dir::{Dir, include_dir};

static UI: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../ui/dist");

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/robots.txt", web::get().to(robots))
        .default_service(web::to(asset));
}

async fn robots() -> HttpResponse {
    HttpResponse::Ok()
        .content_type("text/plain; charset=utf-8")
        .body("User-agent: *\nDisallow: /\n")
}

fn mime(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "wasm" => "application/wasm",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn asset(req: HttpRequest) -> HttpResponse {
    let path = req.path().trim_start_matches('/');
    if path.starts_with("api/") {
        return super::ApiError::not_found(format!("No API route {}.", req.path())).response();
    }
    if !path.is_empty()
        && let Some(file) = UI.get_file(path)
    {
        return HttpResponse::Ok()
            .content_type(mime(path))
            .insert_header(("cache-control", "public, max-age=3600"))
            .body(file.contents());
    }
    match UI.get_file("index.html") {
        Some(index) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .insert_header(("cache-control", "no-cache"))
            .body(index.contents()),
        None => HttpResponse::NotFound().body("The UI is not built into this binary."),
    }
}
