//! How requests reach a Nomad agent: HTTP(S) over TCP, or the task API's
//! Unix socket (`unix://$NOMAD_SECRETS_DIR/api.sock`) for an executor running
//! inside the cluster.

use async_trait::async_trait;
use buckle_core::Result;
use buckle_core::error::{system, user, wrap_user};
use std::time::Duration;

/// One HTTP request, relative to the agent's address.
#[derive(Clone, Default)]
pub struct Request {
    pub method: &'static str,
    /// Path and query, starting with `/v1/`.
    pub path: String,
    pub headers: Vec<(String, String)>,
    /// The body, already serialised. Bodies that carry secret values are
    /// opened by [`buckle_core::secrets::sinks::ApiSink::open`] at the call
    /// that builds the request.
    pub body: Option<Vec<u8>>,
}

impl std::fmt::Debug for Request {
    /// Header values and bodies never appear (a token, a variable's items).
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

/// A response: the status, the headers Buckle reads, and the body.
#[derive(Clone, Default)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
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

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Sends requests to one Nomad agent.
#[async_trait]
pub trait Transport: Send + Sync + std::fmt::Debug {
    async fn send(&self, request: Request) -> Result<Response>;
}

/// Where the agent listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// `http://host:4646` or `https://…`.
    Http(String),
    /// `unix:///path/to/api.sock`.
    Unix(std::path::PathBuf),
}

impl Address {
    /// Parses `http://`, `https://` or `unix://` addresses (`NOMAD_ADDR`'s forms).
    pub fn parse(addr: &str) -> Result<Address> {
        let addr = addr.trim();
        if let Some(path) = addr.strip_prefix("unix://") {
            return Ok(Address::Unix(path.into()));
        }
        if addr.starts_with("http://") || addr.starts_with("https://") {
            return Ok(Address::Http(addr.trim_end_matches('/').to_owned()));
        }
        Err(user(
            format!("'{addr}' is not a Nomad address."),
            &["Use http://host:4646, https://host:4646 or unix:///path/to/api.sock."],
        ))
    }
}

/// Builds the transport for `address`. `ca_pem` pins a CA for HTTPS.
pub fn connect(address: &Address, ca_pem: Option<&[u8]>) -> Result<std::sync::Arc<dyn Transport>> {
    Ok(match address {
        Address::Http(base) => std::sync::Arc::new(HttpTransport::new(base, ca_pem)?),
        #[cfg(unix)]
        Address::Unix(path) => std::sync::Arc::new(UnixTransport { path: path.clone() }),
        #[cfg(not(unix))]
        Address::Unix(_) => {
            return Err(user(
                "Unix socket Nomad addresses are not supported on this platform.",
                &["Use an http:// or https:// address."],
            ));
        }
    })
}

/// The longest a single API call may take. Long-polling calls (blocking
/// queries) pass a shorter `wait` than this.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// HTTP(S) over TCP.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    base: String,
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new(base: &str, ca_pem: Option<&[u8]>) -> Result<Self> {
        let mut builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT);
        if let Some(pem) = ca_pem {
            let cert = reqwest::Certificate::from_pem(pem).map_err(|e| {
                wrap_user(
                    e,
                    "The Nomad CA certificate could not be parsed.",
                    &["Provide the CA as a PEM-encoded certificate."],
                )
            })?;
            builder = builder.add_root_certificate(cert);
        }
        let client = builder.build().map_err(|e| {
            human_errors::wrap_system(e, "The Nomad HTTP client could not be built.", &[])
        })?;
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
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
            .request(method, format!("{}{}", self.base, request.path));
        for (k, v) in &request.headers {
            req = req.header(k, v);
        }
        if let Some(body) = request.body {
            req = req.header("content-type", "application/json").body(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| unreachable_err(&self.base, e))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
            .collect();
        let body = resp
            .bytes()
            .await
            .map_err(|e| unreachable_err(&self.base, e))?
            .to_vec();
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

fn unreachable_err(
    addr: &str,
    e: impl std::error::Error + Send + Sync + 'static,
) -> buckle_core::Error {
    wrap_user(
        e,
        format!("The Nomad API at {addr} could not be reached."),
        &[
            "Check that the cluster's api address is right and that this executor can reach it.",
            "A Nomad server restarting or electing a leader answers again within seconds.",
        ],
    )
}

/// HTTP/1.1 over a Unix socket: the task API Nomad gives a workload.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct UnixTransport {
    pub path: std::path::PathBuf,
}

#[cfg(unix)]
#[async_trait]
impl Transport for UnixTransport {
    async fn send(&self, request: Request) -> Result<Response> {
        use http_body_util::{BodyExt, Full};
        let addr = format!("unix://{}", self.path.display());
        let call = async {
            let stream = tokio::net::UnixStream::connect(&self.path)
                .await
                .map_err(|e| unreachable_err(&addr, e))?;
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                    .await
                    .map_err(|e| unreachable_err(&addr, e))?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let mut builder = hyper::Request::builder()
                .method(request.method)
                .uri(&request.path)
                .header("host", "localhost");
            for (k, v) in &request.headers {
                builder = builder.header(k, v);
            }
            let body = match request.body {
                Some(b) => {
                    builder = builder.header("content-type", "application/json");
                    bytes::Bytes::from(b)
                }
                None => bytes::Bytes::new(),
            };
            let req = builder.body(Full::new(body)).map_err(|e| {
                human_errors::wrap_system(e, "A Nomad request could not be built.", &[])
            })?;
            let resp = sender
                .send_request(req)
                .await
                .map_err(|e| unreachable_err(&addr, e))?;
            let status = resp.status().as_u16();
            let headers = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
                .collect();
            let body = resp
                .into_body()
                .collect()
                .await
                .map_err(|e| unreachable_err(&addr, e))?
                .to_bytes()
                .to_vec();
            Ok(Response {
                status,
                headers,
                body,
            })
        };
        tokio::time::timeout(REQUEST_TIMEOUT, call)
            .await
            .map_err(|_| {
                system(
                    format!("The Nomad API at {addr} did not answer in time."),
                    &["Check that the Nomad agent is running."],
                )
            })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case("http://127.0.0.1:4646/", Address::Http("http://127.0.0.1:4646".into()))]
    #[case("https://nomad.example:4646", Address::Http("https://nomad.example:4646".into()))]
    #[case("unix:///secrets/api.sock", Address::Unix("/secrets/api.sock".into()))]
    fn addresses_parse(#[case] input: &str, #[case] expected: Address) {
        assert_eq!(Address::parse(input).unwrap(), expected);
    }

    #[test]
    fn other_schemes_are_rejected() {
        assert!(Address::parse("127.0.0.1:4646").is_err());
    }

    #[test]
    fn request_debug_hides_header_values_and_bodies() {
        let r = Request {
            method: "PUT",
            path: "/v1/var/x".into(),
            headers: vec![("X-Nomad-Token".into(), "s3cret".into())],
            body: Some(b"{\"Items\":{\"k\":\"hunter2\"}}".to_vec()),
        };
        let shown = format!("{r:?}");
        assert!(
            !shown.contains("s3cret") && !shown.contains("hunter2"),
            "{shown}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_requests_round_trip() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(head.starts_with("GET /v1/status/leader HTTP/1.1"), "{head}");
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 16\r\nX-Nomad-Index: 7\r\n\r\n\"127.0.0.1:4647\"")
                .await
                .unwrap();
        });
        let t = UnixTransport { path };
        let resp = t
            .send(Request {
                method: "GET",
                path: "/v1/status/leader".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.header("x-nomad-index"), Some("7"));
        assert_eq!(resp.body, b"\"127.0.0.1:4647\"");
    }
}
