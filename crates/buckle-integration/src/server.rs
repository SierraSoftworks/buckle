//! One real `buckle-server` instance, in process: path source over a
//! directory, TLS `auto`, token auth with a bootstrap token, no OIDC (so the
//! API is open) and `admin_acl: 'true'` (so every request may act).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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

fn free_port() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("an ephemeral port")
}

fn quoted(path: &Path) -> String {
    serde_json::to_string(&path.display().to_string()).expect("a path serialises")
}

impl TestServer {
    /// Starts a server over `paths` and waits for its API to answer.
    pub async fn start(paths: &ServerPaths, settings: &ServerSettings) -> Self {
        crate::init();
        std::fs::create_dir_all(&paths.secrets).expect("create the secrets root");
        let (grpc, http) = (free_port(), free_port());
        let yaml = format!(
            "listen: {{ grpc: \"{grpc}\", http: \"{http}\", tls: {{ mode: auto }} }}\n\
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
        let task = tokio::spawn(buckle_server::run(config, shutdown.clone()));
        let client = reqwest::Client::builder()
            .pool_max_idle_per_host(0)
            .build()
            .expect("an HTTP client");
        let mut up = false;
        for _ in 0..200 {
            if let Ok(r) = client
                .get(format!("http://{http}/api/v1/fleet"))
                .send()
                .await
                && r.status().is_success()
            {
                up = true;
                break;
            }
            assert!(!task.is_finished(), "the server exited during start-up");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(up, "the server's API did not come up");
        Self {
            http,
            grpc,
            fingerprint: format!("sha256:{fingerprint}"),
            shutdown,
            task,
        }
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
