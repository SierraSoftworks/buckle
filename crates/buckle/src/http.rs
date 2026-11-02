//! The reqwest-backed [`HttpClient`] that the binary attaches to
//! [`buckle_core::system::RealSystem`]. The core crate stays free of an HTTP
//! dependency; tasks and health checks only see [`HttpRequest`]/[`HttpResponse`].

use async_trait::async_trait;
use buckle_core::system::{HttpClient, HttpRequest, HttpResponse};

/// Two clients: one that verifies TLS certificates and one that accepts any
/// certificate, for health checks against local self-signed endpoints that
/// set `tls.insecure`.
#[derive(Debug, Clone)]
pub struct ReqwestClient {
    verified: reqwest::Client,
    insecure: reqwest::Client,
}

impl ReqwestClient {
    pub fn new() -> buckle_core::Result<Self> {
        let user_agent = format!("buckle/{}", crate::telemetry::version());
        let build = |accept_invalid: bool| {
            reqwest::Client::builder()
                .user_agent(user_agent.clone())
                .danger_accept_invalid_certs(accept_invalid)
                .build()
                .map_err(|e| {
                    buckle_core::error::wrap_system(
                        e,
                        "The HTTP client could not be initialised.",
                        &["This usually means the TLS backend failed to load; please report it."],
                    )
                })
        };
        Ok(Self {
            verified: build(false)?,
            insecure: build(true)?,
        })
    }
}

#[async_trait]
impl HttpClient for ReqwestClient {
    async fn send(&self, request: HttpRequest) -> buckle_core::Result<HttpResponse> {
        let client = if request.insecure {
            &self.insecure
        } else {
            &self.verified
        };
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
