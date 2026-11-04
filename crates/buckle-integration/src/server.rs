//! One real `buckle-server` instance, in process: path source over a
//! directory, TLS `auto`, token auth with a bootstrap token, no OIDC (so the
//! API is open) and `admin_acl: 'true'` (so every request may act).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The bootstrap token every test server accepts.
pub const BOOTSTRAP_TOKEN: &str = "integration-bootstrap-token";

/// Server knobs that differ between scenarios.
#[derive(Debug, Clone)]
pub struct ServerSettings {
    /// `auto` or `approve`.
    pub registration: &'static str,
    /// How long an in-flight host may stay disconnected before it fails.
    pub disconnect_timeout: Duration,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self {
            registration: "auto",
            disconnect_timeout: Duration::from_secs(60),
        }
    }
}

/// Where a server keeps its state and reads its configuration.
#[derive(Debug, Clone)]
pub struct ServerPaths {
    /// `data_dir`: SQLite, revision cache, install key, TLS certificate.
    pub data: PathBuf,
    /// The watched configuration directory.
    pub source: PathBuf,
    /// The file secret provider's root.
    pub secrets: PathBuf,
    /// Held for writing while the source directory is rewritten; the
    /// server's scans hold it for reading, so none sees a half-written tree.
    pub source_lock: Arc<tokio::sync::RwLock<()>>,
}

/// A running server.
pub struct TestServer {
    /// The HTTP API.
    pub http: SocketAddr,
    /// The gRPC listener agents reach (through the proxy).
    pub grpc: SocketAddr,
    /// `sha256:` fingerprint of the auto-generated certificate.
    pub fingerprint: String,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<buckle_core::Result<i32>>,
}

fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("a path serialises")
}

impl TestServer {
    /// Starts a server over `paths` and waits for its API to answer.
    ///
    /// The listeners bind port 0 and the server reports what it bound, so no
    /// other test can take a port between it being picked and bound.
    pub async fn start(paths: &ServerPaths, settings: &ServerSettings) -> Self {
        crate::init();
        std::fs::create_dir_all(&paths.secrets).expect("create the secrets root");
        let yaml = format!(
            "listen: {{ grpc: \"127.0.0.1:0\", http: \"127.0.0.1:0\", tls: {{ mode: auto }} }}\n\
             data_dir: {data}\n\
             source: {{ path: {source} }}\n\
             agents:\n  auth: [token]\n  bootstrap_token: {BOOTSTRAP_TOKEN}\n  registration: {registration}\n  minimum_agent_version: \"2.0.0-dev\"\n  disconnect_timeout: {timeout}ms\n\
             web: {{ admin_acl: 'true' }}\n\
             secrets: {{ providers: {{ file: {{ root: {secrets} }} }} }}\n",
            data = quoted(&paths.data),
            source = quoted(&paths.source),
            secrets = quoted(&paths.secrets),
            registration = settings.registration,
            timeout = settings.disconnect_timeout.as_millis(),
        );
        let config = buckle_server::ServerConfig::parse(&yaml).expect("the server config parses");
        let fingerprint = buckle_server::fingerprint(&config)
            .expect("the certificate loads")
            .expect("auto TLS has a fingerprint");
        let shutdown = CancellationToken::new();
        let (listening, mut bound) = tokio::sync::watch::channel(None);
        let hooks = buckle_server::Hooks {
            source_lock: Some(paths.source_lock.clone()),
            listening: Some(Arc::new(listening)),
        };
        let mut task = tokio::spawn(buckle_server::run_with(config, shutdown.clone(), hooks));
        let addresses = tokio::select! {
            r = bound.wait_for(Option::is_some) => r.ok().and_then(|l| *l),
            r = &mut task => panic!("the server exited during start-up: {}", match r {
                Ok(Ok(code)) => format!("run returned exit code {code}"),
                Ok(Err(e)) => e.to_string(),
                Err(e) => format!("the server task panicked: {e}"),
            }),
            _ = tokio::time::sleep(Duration::from_secs(20)) => None,
        }
        .expect("the server binds its listeners within 20s");
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .expect("an HTTP client");
        for _ in 0..200 {
            if let Ok(r) = client
                .get(format!("http://{}/api/v1/fleet", addresses.http))
                .send()
                .await
                && r.status().is_success()
            {
                return Self {
                    http: addresses.http,
                    grpc: addresses.grpc,
                    fingerprint: format!("sha256:{fingerprint}"),
                    shutdown,
                    task,
                };
            }
            assert!(!task.is_finished(), "the server exited during start-up");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        shutdown.cancel();
        panic!("the server's API did not come up within 5s");
    }

    /// Requests shutdown and waits for `run` to return its exit code.
    pub async fn stop(self) -> i32 {
        self.shutdown.cancel();
        match tokio::time::timeout(Duration::from_secs(15), self.task).await {
            Ok(Ok(Ok(code))) => code,
            Ok(Ok(Err(e))) => panic!("the server failed: {e}"),
            Ok(Err(e)) => panic!("the server task panicked: {e}"),
            Err(_) => panic!("the server did not stop within 15s"),
        }
    }
}
