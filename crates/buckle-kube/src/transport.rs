//! How requests reach a Kubernetes API server: HTTPS (or HTTP) over TCP,
//! trusting the cluster's CA and, for a kubeconfig that says so, presenting
//! a client certificate.

use async_trait::async_trait;
use buckle_core::Result;
use buckle_core::error::{user, wrap_user};
use secrecy::{ExposeSecret, SecretBox};
use std::time::Duration;

/// One HTTP request, relative to the API server's address.
#[derive(Clone, Default)]
pub struct Request {
    pub method: &'static str,
    /// Path and query, starting with `/api` or `/apis`.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub content_type: Option<&'static str>,
    /// The body, already serialised. Bodies that carry secret values are
    /// opened by [`buckle_core::secrets::sinks::ApiSink::open`] at the call
    /// that builds the request.
    pub body: Option<Vec<u8>>,
}

impl std::fmt::Debug for Request {
    /// Header values and bodies never appear (a token, a Secret's data).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("path", &self.path)
            .field(
                "headers",
                &self.headers.iter().map(|(k, _)| k).collect::<Vec<_>>(),
            )
            .field("body_bytes", &self.body.as_ref().map(Vec::len))
            .finish()
    }
}

/// A response: its status and body.
#[derive(Clone, Default)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Sends requests to one API server.
#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    async fn send(&self, request: Request) -> Result<Response>;
}

/// The TLS material for an API server.
#[derive(Default)]
pub struct Tls {
    /// CAs to trust beside the system roots (the cluster's own CA).
    pub ca_pem: Vec<Vec<u8>>,
    /// A client certificate and its key, as PEM (a k3s kubeconfig's user).
    pub identity_pem: Option<SecretBox<Vec<u8>>>,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tls")
            .field("ca_pem", &self.ca_pem.len())
            .field("identity_pem", &self.identity_pem.is_some())
            .finish()
    }
}

/// The longest a single API call may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// HTTP(S) over TCP.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    base: String,
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(base: &str, tls: &Tls) -> Result<Self> {
        let base = base.trim().trim_end_matches('/');
        if !base.starts_with("https://") && !base.starts_with("http://") {
            return Err(user(
                format!("'{base}' is not a Kubernetes API address."),
                &["Use https://host:6443 (or the address in the cluster's kubeconfig)."],
            ));
        }
        let mut builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT);
        for pem in &tls.ca_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem).map_err(|e| {
                wrap_user(
                    e,
                    "The Kubernetes CA certificate could not be parsed.",
                    &["Provide the CA as PEM (`certificate-authority-data` in a kubeconfig)."],
                )
            })?;
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        if let Some(identity) = &tls.identity_pem {
            let identity = reqwest::Identity::from_pem(identity.expose_secret()).map_err(|e| {
                wrap_user(
                    e,
                    "The kubeconfig's client certificate could not be used.",
                    &["Check `client-certificate-data` and `client-key-data` in the kubeconfig."],
                )
            })?;
            builder = builder.identity(identity);
        }
        let client = builder.build().map_err(|e| {
            human_errors::wrap_system(e, "The Kubernetes HTTP client could not be built.", &[])
        })?;
        Ok(Self {
            base: base.to_owned(),
            client,
        })
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&self, request: Request) -> Result<Response> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|e| human_errors::wrap_system(e, "An invalid HTTP method was used.", &[]))?;
        let mut req = self
            .client
            .request(method, format!("{}{}", self.base, request.path))
            .header("accept", "application/json");
        for (k, v) in &request.headers {
            req = req.header(k, v);
        }
        if let Some(body) = request.body {
            req = req
                .header(
                    "content-type",
                    request.content_type.unwrap_or("application/json"),
                )
                .body(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| unreachable_err(&self.base, e))?;
        let status = resp.status().as_u16();
        let body = resp
            .bytes()
            .await
            .map_err(|e| unreachable_err(&self.base, e))?
            .to_vec();
        Ok(Response { status, body })
    }
}

fn unreachable_err(
    addr: &str,
    e: impl std::error::Error + Send + Sync + 'static,
) -> buckle_core::Error {
    wrap_user(
        e,
        format!("The Kubernetes API at {addr} could not be reached."),
        &[
            "Check that the cluster's api address is right and that this executor can reach it.",
            "A certificate error means the executor does not trust the API server's CA: run the executor on a node with the cluster's kubeconfig, or in the cluster.",
        ],
    )
}
