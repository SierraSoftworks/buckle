//! The operator API as a test sees it: a reqwest client that sends the
//! double-submit CSRF token on mutating requests and journals every response
//! body (so leak checks can search everything the API ever returned), and an
//! SSE reader for `GET /api/v1/events`.

use buckle_api::LiveEvent;
use futures::StreamExt;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Every response body the API returned during a test, oldest first.
pub type Journal = Arc<Mutex<Vec<String>>>;

const CSRF: &str = "integration-test-csrf-token-0123456789abcdef";

/// A client for one server instance.
#[derive(Clone)]
pub struct Api {
    base: String,
    client: reqwest::Client,
    journal: Journal,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").field("base", &self.base).finish()
    }
}

impl Api {
    /// A client for the server listening on `http`, journaling into `journal`.
    pub fn new(http: SocketAddr, journal: Journal) -> Self {
        let client = reqwest::Client::builder()
            // No idle keep-alive connections: a server stopping gracefully
            // would otherwise wait for them.
            .pool_max_idle_per_host(0)
            .timeout(Duration::from_secs(10))
            .build()
            .expect("an HTTP client");
        Self {
            base: format!("http://{http}"),
            client,
            journal,
        }
    }

    /// The server's base URL.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Sends a request; `None` when the server could not be reached.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Option<(StatusCode, String)> {
        let mut req = self
            .client
            .request(method.clone(), format!("{}{path}", self.base));
        if method != Method::GET {
            req = req
                .header("x-csrf-token", CSRF)
                .header("cookie", format!("buckle_csrf={CSRF}"));
        }
        if let Some(body) = body {
            req = req.json(&body);
        }
        let response = req.send().await.ok()?;
        let status = response.status();
        let text = response.text().await.ok()?;
        self.journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{method} {path} -> {status}\n{text}"));
        Some((status, text))
    }

    /// `GET path` parsed as `T`, or `None` on any failure (for polling).
    pub async fn try_get<T: DeserializeOwned>(&self, path: &str) -> Option<T> {
        let (status, text) = self.request(Method::GET, path, None).await?;
        if !status.is_success() {
            return None;
        }
        serde_json::from_str(&text).ok()
    }

    /// `GET path` parsed as `T`; panics on failure.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> T {
        let (status, text) = self
            .request(Method::GET, path, None)
            .await
            .unwrap_or_else(|| panic!("GET {path}: the server did not answer"));
        assert!(status.is_success(), "GET {path} -> {status}: {text}");
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("GET {path}: the body does not parse ({e}): {text}"))
    }

    /// `POST path` with a JSON body; returns the status and body.
    pub async fn post(&self, path: &str, body: serde_json::Value) -> (StatusCode, String) {
        self.request(Method::POST, path, Some(body))
            .await
            .unwrap_or_else(|| panic!("POST {path}: the server did not answer"))
    }

    /// `POST path`, asserting success.
    pub async fn post_ok(&self, path: &str, body: serde_json::Value) -> String {
        let (status, text) = self.post(path, body).await;
        assert!(status.is_success(), "POST {path} -> {status}: {text}");
        text
    }

    /// `PATCH path`, asserting success.
    pub async fn patch_ok(&self, path: &str, body: serde_json::Value) -> String {
        let (status, text) = self
            .request(Method::PATCH, path, Some(body))
            .await
            .unwrap_or_else(|| panic!("PATCH {path}: the server did not answer"));
        assert!(status.is_success(), "PATCH {path} -> {status}: {text}");
        text
    }
}

/// A live subscription to `GET /api/v1/events`, recording the raw stream.
pub struct Sse {
    raw: Arc<Mutex<String>>,
    task: tokio::task::JoinHandle<()>,
}

impl Sse {
    /// Subscribes and waits for the server's `: connected` comment, so
    /// everything published afterwards is seen.
    pub async fn connect(api: &Api) -> Self {
        let raw = Arc::new(Mutex::new(String::new()));
        let client = reqwest::Client::new();
        let response = client
            .get(format!("{}/api/v1/events", api.base()))
            .send()
            .await
            .expect("the event stream opens");
        assert!(response.status().is_success(), "GET /api/v1/events");
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let sink = raw.clone();
        let task = tokio::spawn(async move {
            let mut ready = Some(ready_tx);
            let mut stream = response.bytes_stream();
            while let Some(Ok(chunk)) = stream.next().await {
                let text = String::from_utf8_lossy(&chunk).into_owned();
                let connected = text.contains(": connected");
                sink.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(&text);
                if connected && let Some(tx) = ready.take() {
                    let _ = tx.send(());
                }
            }
        });
        let _ = tokio::time::timeout(Duration::from_secs(5), ready_rx).await;
        Self { raw, task }
    }

    /// The raw stream so far.
    pub fn raw(&self) -> String {
        self.raw.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The events so far, parsed from complete `data:` lines.
    pub fn events(&self) -> Vec<LiveEvent> {
        self.raw()
            .split("\n\n")
            .flat_map(|frame| frame.lines())
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|json| serde_json::from_str(json).ok())
            .collect()
    }

    /// Closes the subscription (a server stopping gracefully waits for open streams).
    pub fn close(self) {}
}

impl Drop for Sse {
    fn drop(&mut self) {
        self.task.abort();
    }
}
