//! The reqwest-backed [`HttpClient`] that the binary attaches to
//! [`buckle_core::system::RealSystem`]. The core crate stays free of an HTTP
//! dependency; tasks and health checks only see [`HttpRequest`]/[`HttpResponse`].

use async_trait::async_trait;
use buckle_core::system::{HttpClient, HttpRequest, HttpResponse};
use std::sync::Arc;
use tokio::sync::OnceCell;

/// Two clients, built on first use: one that verifies TLS certificates and one
/// that accepts any certificate, for health checks against local self-signed
/// endpoints that set `tls.insecure`.
///
/// Construction is lazy so a host without a CA bundle (a minimal container, a
/// freshly imaged machine) can still plan and apply everything that does not
/// talk HTTPS; the failure surfaces on the task that needs the network.
#[derive(Debug, Clone, Default)]
pub struct ReqwestClient {
    verified: Arc<OnceCell<reqwest::Client>>,
    insecure: Arc<OnceCell<reqwest::Client>>,
}

impl ReqwestClient {
    pub fn new() -> Self {
        Self::default()
    }

    async fn client(&self, accept_invalid: bool) -> buckle_core::Result<&reqwest::Client> {
        let cell = if accept_invalid {
            &self.insecure
        } else {
            &self.verified
        };
        cell.get_or_try_init(|| async move {
            reqwest::Client::builder()
                .user_agent(format!("buckle/{}", crate::telemetry::version()))
                .danger_accept_invalid_certs(accept_invalid)
                .build()
                .map_err(|e| {
                    buckle_core::error::wrap_system(
                        e,
                        "The HTTP client could not be initialised.",
                        &[
                            "Install the system CA bundle (the `ca-certificates` package) so TLS roots can be loaded.",
                            "If the bundle is present, the TLS backend failed to load; please report it.",
                        ],
                    )
                })
        })
        .await
    }
}

#[async_trait]
impl HttpClient for ReqwestClient {
    async fn send(&self, request: HttpRequest) -> buckle_core::Result<HttpResponse> {
        let client = self.client(request.insecure).await?;
        let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!("'{}' is not a valid HTTP method.", request.method),
                &["Use GET, HEAD, POST, PUT, PATCH or DELETE."],
            )
        })?;
        let mut builder = client.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(timeout) = request.timeout {
            builder = builder.timeout(timeout);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        let response = builder.send().await.map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!("The HTTP request to {} failed.", request.url),
                &[
                    "Check that the URL is reachable from this host and that the service is up.",
                    "For a self-signed certificate on a health check, set `tls: { insecure: true }`.",
                ],
            )
        })?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned()))
            .collect();
        let body = response
            .bytes()
            .await
            .map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("The response from {} could not be read.", request.url),
                    &["The connection was interrupted; try again."],
                )
            })?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn construction_is_infallible_and_errors_surface_per_request() {
        let client = ReqwestClient::new();
        // `.invalid` never resolves (RFC 2606), so this fails offline and fast; the
        // point is that the failure names the request, not client initialisation.
        let err = client
            .send(HttpRequest::get("https://example.invalid/"))
            .await
            .expect_err("an unresolvable host fails the request");
        assert!(
            err.description().contains("example.invalid"),
            "the error names the request: {err}"
        );
        assert!(
            !err.description().contains("could not be initialised"),
            "construction problems are not reported for a plain request failure: {err}"
        );
    }
}
