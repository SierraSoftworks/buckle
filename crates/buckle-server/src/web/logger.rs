//! Request spans with redaction: `code`, `state`, `token` (and similar)
//! query parameters are masked, and `authorization`/`cookie` headers are
//! never recorded.

use actix_web::body::BoxBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use tracing::Instrument;

const REDACTED_PARAMS: &[&str] = &[
    "code",
    "state",
    "token",
    "id_token",
    "refresh_token",
    "access_token",
    "session_state",
];

/// Whether a raw (still percent-encoded) query key names a sensitive
/// parameter. The key is decoded first, so `%63ode` is `code`.
fn sensitive(raw_key: &str) -> bool {
    let key: String = url::form_urlencoded::parse(raw_key.as_bytes())
        .next()
        .map(|(k, _)| k.into_owned())
        .unwrap_or_default();
    REDACTED_PARAMS.contains(&key.trim().to_ascii_lowercase().as_str())
}

/// The query string with sensitive parameters masked.
pub fn redact_query(query: &str) -> String {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let key = pair.split_once('=').map_or(pair, |(k, _)| k);
            if sensitive(key) {
                format!("{key}=REDACTED")
            } else {
                pair.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Wraps every request in an `http.request` span.
pub async fn middleware(
    req: ServiceRequest,
    next: actix_web::middleware::Next<BoxBody>,
) -> Result<ServiceResponse<BoxBody>, actix_web::Error> {
    let query = redact_query(req.query_string());
    let span = tracing::info_span!(
        "http.request",
        http.method = %req.method(),
        http.route = %req.path(),
        http.query = %query,
        http.user_agent = req.headers().get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or(""),
        http.status_code = tracing::field::Empty,
    );
    let response = next.call(req).instrument(span.clone()).await?;
    span.record("http.status_code", response.status().as_u16());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case(
        "code=abc&state=xyz&return_to=/x",
        "code=REDACTED&state=REDACTED&return_to=/x"
    )]
    #[case("token=t", "token=REDACTED")]
    #[case("%63ode=abc&st%61te=xyz", "%63ode=REDACTED&st%61te=REDACTED")]
    #[case("ID_TOKEN=t", "ID_TOKEN=REDACTED")]
    #[case("refresh%5Ftoken=t", "refresh%5Ftoken=REDACTED")]
    #[case("limit=10", "limit=10")]
    #[case("", "")]
    fn sensitive_query_parameters_are_masked(#[case] raw: &str, #[case] expected: &str) {
        assert_eq!(redact_query(raw), expected);
    }
}
