//! How requests reach a Nomad agent: HTTP(S) over TCP, or the task API's
//! Unix socket (`unix://$NOMAD_SECRETS_DIR/api.sock`) for an executor running
//! inside the cluster.

use async_trait::async_trait;
use buckle_core::Result;
use buckle_core::error::{user, wrap_user};
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

    /// Upgrades `request` (a `GET`) to a WebSocket, as Nomad's exec and
    /// action endpoints expect.
    async fn socket(&self, request: Request) -> Result<Box<dyn Socket>> {
        let _ = request;
        Err(user(
            "This Nomad transport cannot open WebSockets.",
            &["Run the action from Nomad (`nomad action`)."],
        ))
    }
}

/// A WebSocket a request was upgraded to, carrying text frames.
#[async_trait]
pub trait Socket: Send {
    async fn send(&mut self, text: String) -> Result<()>;
    /// The next text frame; `None` once the server has closed it.
    async fn recv(&mut self) -> Result<Option<String>>;
}

/// A WebSocket over an upgraded HTTP/1.1 connection.
struct WebSocket<S> {
    inner: tokio_tungstenite::WebSocketStream<S>,
    addr: String,
}

impl<S> WebSocket<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn over(stream: S, addr: &str) -> Box<dyn Socket> {
        let inner = tokio_tungstenite::WebSocketStream::from_raw_socket(
            stream,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        Box::new(WebSocket {
            inner,
            addr: addr.to_owned(),
        })
    }
}

#[async_trait]
impl<S> Socket for WebSocket<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, text: String) -> Result<()> {
        use futures::SinkExt;
        self.inner
            .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
            .await
            .map_err(|e| unreachable_err(&self.addr, e))
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        loop {
            return match self.inner.next().await {
                None | Some(Ok(Message::Close(_))) => Ok(None),
                Some(Ok(Message::Text(t))) => Ok(Some(t.as_str().to_owned())),
                Some(Ok(Message::Binary(b))) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
                Some(Ok(_)) => continue,
                Some(Err(e)) => Err(unreachable_err(&self.addr, e)),
            };
        }
    }
}

/// The headers that ask for a WebSocket, with the key the answer must
/// derive its accept header from.
fn upgrade_headers() -> (Vec<(String, String)>, String) {
    let key = tokio_tungstenite::tungstenite::handshake::client::generate_key();
    (
        vec![
            ("Connection".into(), "Upgrade".into()),
            ("Upgrade".into(), "websocket".into()),
            ("Sec-WebSocket-Version".into(), "13".into()),
            ("Sec-WebSocket-Key".into(), key.clone()),
        ],
        key,
    )
}

/// Checks a `101`'s accept header against the `key` the upgrade sent.
fn accepted(accept: Option<&str>, key: &str) -> Result<()> {
    let expected = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    match accept {
        Some(a) if a == expected => Ok(()),
        _ => Err(buckle_core::error::system(
            "Nomad accepted the WebSocket with a wrong Sec-WebSocket-Accept.",
            &["Check that nothing between the executor and Nomad rewrites the upgrade."],
        )),
    }
}

/// Nomad's answer to an upgrade other than `101` (a `403` without
/// `alloc-exec`).
fn refused(status: u16, body: &[u8]) -> buckle_core::Error {
    let text = String::from_utf8_lossy(body);
    let advice: &[&str] = if status == 403 {
        &[
            "The executor's Nomad policy needs `alloc-exec` (and `alloc-node-exec` for raw_exec tasks) in the job's namespace.",
        ]
    } else {
        &[]
    };
    user(
        format!("Nomad refused the WebSocket ({status}): {}", text.trim()),
        advice,
    )
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
    /// HTTP/1.1 only, without a deadline: WebSockets upgrade a 1.1
    /// connection and last as long as what they carry.
    sockets: reqwest::Client,
}

impl HttpTransport {
    pub fn new(base: &str, ca_pem: Option<&[u8]>) -> Result<Self> {
        let build = |builder: reqwest::ClientBuilder| {
            let mut builder = builder;
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
            builder.build().map_err(|e| {
                human_errors::wrap_system(e, "The Nomad HTTP client could not be built.", &[])
            })
        };
        Ok(Self {
            base: base.trim_end_matches('/').to_owned(),
            client: build(reqwest::Client::builder().timeout(REQUEST_TIMEOUT))?,
            sockets: build(
                reqwest::Client::builder()
                    .http1_only()
                    .connect_timeout(REQUEST_TIMEOUT),
            )?,
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

    async fn socket(&self, request: Request) -> Result<Box<dyn Socket>> {
        let (upgrade, key) = upgrade_headers();
        let mut req = self.sockets.get(format!("{}{}", self.base, request.path));
        for (k, v) in request.headers.iter().chain(&upgrade) {
            req = req.header(k, v);
        }
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, req.send())
            .await
            .map_err(|_| {
                buckle_core::error::system(
                    format!("The Nomad API at {} did not answer in time.", self.base),
                    &["Check that the Nomad agent is running."],
                )
            })?
            .map_err(|e| unreachable_err(&self.base, e))?;
        let status = resp.status().as_u16();
        let accept = resp
            .headers()
            .get("sec-websocket-accept")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if status != 101 {
            return Err(refused(status, &resp.bytes().await.unwrap_or_default()));
        }
        accepted(accept.as_deref(), &key)?;
        let upgraded = resp
            .upgrade()
            .await
            .map_err(|e| unreachable_err(&self.base, e))?;
        Ok(WebSocket::over(upgraded, &self.base).await)
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
                buckle_core::error::system(
                    format!("The Nomad API at {addr} did not answer in time."),
                    &["Check that the Nomad agent is running."],
                )
            })?
    }

    async fn socket(&self, request: Request) -> Result<Box<dyn Socket>> {
        use http_body_util::{BodyExt, Empty};
        let addr = format!("unix://{}", self.path.display());
        let (upgrade, key) = upgrade_headers();
        let call = async {
            let stream = tokio::net::UnixStream::connect(&self.path)
                .await
                .map_err(|e| unreachable_err(&addr, e))?;
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                    .await
                    .map_err(|e| unreachable_err(&addr, e))?;
            tokio::spawn(async move {
                let _ = conn.with_upgrades().await;
            });
            let mut builder = hyper::Request::builder()
                .method("GET")
                .uri(&request.path)
                .header("host", "localhost");
            for (k, v) in request.headers.iter().chain(&upgrade) {
                builder = builder.header(k, v);
            }
            let req = builder.body(Empty::<bytes::Bytes>::new()).map_err(|e| {
                human_errors::wrap_system(e, "A Nomad request could not be built.", &[])
            })?;
            sender
                .send_request(req)
                .await
                .map_err(|e| unreachable_err(&addr, e))
        };
        let resp = tokio::time::timeout(REQUEST_TIMEOUT, call)
            .await
            .map_err(|_| {
                buckle_core::error::system(
                    format!("The Nomad API at {addr} did not answer in time."),
                    &["Check that the Nomad agent is running."],
                )
            })??;
        let status = resp.status().as_u16();
        let accept = resp
            .headers()
            .get("sec-websocket-accept")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if status != 101 {
            let body = resp
                .into_body()
                .collect()
                .await
                .map(|b| b.to_bytes().to_vec())
                .unwrap_or_default();
            return Err(refused(status, &body));
        }
        accepted(accept.as_deref(), &key)?;
        let upgraded = hyper::upgrade::on(resp)
            .await
            .map_err(|e| unreachable_err(&addr, e))?;
        Ok(WebSocket::over(hyper_util::rt::TokioIo::new(upgraded), &addr).await)
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

    /// The upgrade carries the token, and frames flow both ways until the
    /// server closes.
    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::result_large_err)] // tungstenite's handshake callback type
    async fn unix_socket_upgrades_to_a_websocket() {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut seen = None;
            let mut ws = tokio_tungstenite::accept_hdr_async(
                stream,
                |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
                    seen = Some((
                        req.uri().to_string(),
                        req.headers()["X-Nomad-Token"].to_str().unwrap().to_owned(),
                    ));
                    Ok(resp)
                },
            )
            .await
            .unwrap();
            let got = ws.next().await.unwrap().unwrap();
            ws.send(Message::text(format!("echo {}", got.to_text().unwrap())))
                .await
                .unwrap();
            ws.close(None).await.unwrap();
            seen.unwrap()
        });
        let t = UnixTransport { path };
        let mut socket = t
            .socket(Request {
                method: "GET",
                path: "/v1/job/x/action?action=a".into(),
                headers: vec![("X-Nomad-Token".into(), "s3cret".into())],
                body: None,
            })
            .await
            .unwrap();
        socket.send("hi".into()).await.unwrap();
        assert_eq!(socket.recv().await.unwrap().as_deref(), Some("echo hi"));
        assert_eq!(socket.recv().await.unwrap(), None);
        assert_eq!(
            server.await.unwrap(),
            ("/v1/job/x/action?action=a".to_owned(), "s3cret".to_owned())
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refused_upgrade_says_what_nomad_answered() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await.unwrap();
            s.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 17\r\n\r\nPermission denied")
                .await
                .unwrap();
        });
        let err = UnixTransport { path }
            .socket(Request {
                method: "GET",
                path: "/v1/job/x/action".into(),
                ..Default::default()
            })
            .await
            .err()
            .expect("refused");
        let shown = format!("{err}");
        assert!(
            shown.contains("403") && shown.contains("Permission denied"),
            "{shown}"
        );
        assert!(shown.contains("alloc-exec"), "{shown}");
    }

    /// Over TCP the upgrade goes through the HTTP client (its CA and TLS).
    #[tokio::test]
    async fn http_upgrades_to_a_websocket() {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::text("{\"exited\":true}")).await.unwrap();
            ws.close(None).await.unwrap();
        });
        let t = HttpTransport::new(&format!("http://{addr}"), None).unwrap();
        let mut socket = t
            .socket(Request {
                method: "GET",
                path: "/v1/job/x/action".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(
            socket.recv().await.unwrap().as_deref(),
            Some("{\"exited\":true}")
        );
    }
}
