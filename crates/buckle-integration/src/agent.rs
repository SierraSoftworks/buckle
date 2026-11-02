//! Real `buckle-agent` sessions in process.
//!
//! Each agent is the production [`Agent`] session loop over the production
//! [`GrpcTransport`] (TLS pinned to the server's certificate, through the
//! test proxy) and the production [`EngineRunner`], whose host is a
//! [`FakeSystem`] instead of the real machine. The transport is wrapped so
//! that every report the agent sends is also recorded for assertions.

use async_trait::async_trait;
use buckle_agent::config::AgentConfig;
use buckle_agent::runner::EngineRunner;
use buckle_agent::session::{Agent, AgentOptions};
use buckle_agent::store::{AgentState, Store};
use buckle_agent::transport::grpc::GrpcTransport;
use buckle_agent::transport::{Hello, Report, RevisionSource, Session, Transport, WireSecret};
use buckle_agent::update::{UpdateOutcome, Updater};
use buckle_core::plan::RevisionId;
use buckle_core::system::{FilePerms, System};
use buckle_core::testing::FakeSystem;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// How an agent is configured.
#[derive(Clone)]
pub struct AgentSpec {
    /// `host_id`.
    pub id: String,
    /// Seed labels from agent.yaml.
    pub labels: Vec<(String, String)>,
    /// The host the engine drives.
    pub system: FakeSystem,
    /// `drift.interval`.
    pub drift_interval: Duration,
    /// `drift.correct`.
    pub drift_correct: bool,
    /// The bootstrap token presented on first contact.
    pub bootstrap_token: String,
    /// The state directory's name under the test directory (default: the id).
    pub state: Option<String>,
}

impl AgentSpec {
    /// An agent called `id` on a fresh fake host (hostname `id`) with no
    /// labels, drift effectively off and the server's bootstrap token.
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            labels: Vec::new(),
            system: FakeSystem::new().with_hostname(id),
            drift_interval: Duration::from_secs(3600),
            drift_correct: true,
            bootstrap_token: crate::server::BOOTSTRAP_TOKEN.to_owned(),
            state: None,
        }
    }

    /// Adds a seed label.
    pub fn label(mut self, key: &str, value: &str) -> Self {
        self.labels.push((key.to_owned(), value.to_owned()));
        self
    }

    /// Uses `system` as the host.
    pub fn system(mut self, system: FakeSystem) -> Self {
        self.system = system;
        self
    }

    /// Re-plans every `interval`, correcting drift when `correct`.
    pub fn drift(mut self, interval: Duration, correct: bool) -> Self {
        self.drift_interval = interval;
        self.drift_correct = correct;
        self
    }

    /// Presents `token` instead of the server's bootstrap token.
    pub fn bootstrap_token(mut self, token: &str) -> Self {
        self.bootstrap_token = token.to_owned();
        self
    }

    /// Keeps state in a directory called `name` instead of the id (a second
    /// machine claiming the same id).
    pub fn state_dir(mut self, name: &str) -> Self {
        self.state = Some(name.to_owned());
        self
    }
}

/// Every report an agent sent, oldest first.
pub type Reports = Arc<Mutex<Vec<Report>>>;

/// A [`Transport`] that records every report on its way to the server.
struct Recording {
    inner: GrpcTransport,
    reports: Reports,
}

#[async_trait]
impl Transport for Recording {
    async fn connect(&self, hello: Hello) -> buckle_core::Result<Session> {
        let session = self.inner.connect(hello).await?;
        let upstream = session.reports;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Report>(256);
        let reports = self.reports.clone();
        tokio::spawn(async move {
            loop {
                let report = tokio::select! {
                    _ = upstream.closed() => break,
                    r = rx.recv() => match r {
                        Some(r) => r,
                        None => break,
                    },
                };
                reports
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(report.clone());
                if upstream.send(report).await.is_err() {
                    break;
                }
            }
            // Dropping `rx` makes the agent's next send fail, as the real
            // session sink would once the stream ended.
        });
        Ok(Session {
            instructions: session.instructions,
            reports: tx,
        })
    }

    async fn fetch_revision(
        &self,
        revision: &RevisionId,
        tree_digest: Option<&str>,
    ) -> buckle_core::Result<RevisionSource> {
        self.inner.fetch_revision(revision, tree_digest).await
    }

    async fn fetch_secrets(
        &self,
        revision: &RevisionId,
        names: &[String],
    ) -> buckle_core::Result<Vec<WireSecret>> {
        self.inner.fetch_secrets(revision, names).await
    }
}

/// Never upgrades.
struct NoUpdates;

#[async_trait]
impl Updater for NoUpdates {
    async fn update(&self, _version: Option<&str>) -> buckle_core::Result<UpdateOutcome> {
        Ok(UpdateOutcome::UpToDate {
            version: "2.0.0".into(),
        })
    }
}

/// What a test holds on to for one agent. Cheap to clone; the running
/// process is owned by [`crate::World`].
#[derive(Clone)]
pub struct AgentView {
    /// The host id.
    pub id: String,
    /// The fake host (shared with the running engine).
    pub system: FakeSystem,
    /// The agent's state directory.
    pub state_dir: PathBuf,
    reports: Reports,
}

impl AgentView {
    /// Every report sent so far.
    pub fn reports(&self) -> Vec<Report> {
        self.reports
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The persisted session state (labels and status from Welcome,
    /// watchdog, unreported local rollbacks).
    pub fn agent_state(&self) -> AgentState {
        Store::new(&self.state_dir).load_agent_state()
    }

    /// The per-host token the agent stored after enrolment.
    pub fn token(&self) -> Option<String> {
        Store::new(&self.state_dir).read_token().ok().flatten()
    }

    /// The revision the agent's state records as applied.
    pub fn applied_revision(&self) -> Option<String> {
        Store::new(&self.state_dir)
            .load_state()
            .ok()
            .and_then(|s| s.applied_revision)
            .map(|r| r.0)
    }

    /// A file on the fake host, as text.
    pub fn file(&self, path: &str) -> Option<String> {
        self.system.file_string(path)
    }

    /// Writes a file on the fake host, as a hand edit would.
    pub fn write_file(&self, path: &str, content: &str) {
        let _ = self
            .system
            .clone()
            .with_file(path, content, FilePerms::mode(0o644));
    }

    /// Deletes a file on the fake host.
    pub async fn remove_file(&self, path: &str) {
        let _ = self.system.remove(Path::new(path), false).await;
    }
}

/// A running agent.
pub(crate) struct RunningAgent {
    pub spec: AgentSpec,
    pub view: AgentView,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<buckle_core::Result<i32>>,
}

impl RunningAgent {
    /// Starts the agent for `spec` with state in `state_dir`, connecting to
    /// `server` (the proxy) and pinning `fingerprint`.
    pub fn start(
        spec: AgentSpec,
        state_dir: &Path,
        server: SocketAddr,
        fingerprint: &str,
        reports: Reports,
    ) -> Self {
        let labels: String = spec
            .labels
            .iter()
            .map(|(k, v)| format!("{k}: {}", serde_json::to_string(v).unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(", ");
        let yaml = format!(
            "server: https://localhost:{port}\nhost_id: {id}\nstate_dir: {state}\nbootstrap_token: {token}\n\
             tls: {{ ca_fingerprint: \"{fingerprint}\" }}\nlabels: {{ {labels} }}\n\
             drift: {{ interval: {interval}ms, correct: {correct} }}\n",
            port = server.port(),
            id = spec.id,
            state = serde_json::to_string(&state_dir.display().to_string()).unwrap_or_default(),
            token = spec.bootstrap_token,
            interval = spec.drift_interval.as_millis(),
            correct = spec.drift_correct,
        );
        let config: AgentConfig = serde_yaml::from_str(&yaml).expect("the agent config parses");
        let transport = Arc::new(Recording {
            inner: GrpcTransport::new(&config).expect("the transport builds"),
            reports: reports.clone(),
        });
        let state = Arc::new(Store::new(state_dir).state_store());
        let runner = Arc::new(EngineRunner::new(Arc::new(spec.system.clone()), state));
        let options = AgentOptions {
            agent_version: "2.0.0".into(),
            heartbeat: Duration::from_secs(1),
            facts_interval: Duration::from_secs(3600),
            backoff_min: Duration::from_millis(100),
            backoff_max: Duration::from_millis(300),
            backoff_jitter: false,
            rejected_backoff_max: Duration::from_secs(1),
            event_spacing: Duration::from_millis(10),
            shutdown_grace: Duration::from_secs(5),
        };
        let agent = Agent::new(config, transport, runner, Arc::new(NoUpdates), options)
            .expect("the agent starts");
        let cancel = CancellationToken::new();
        let task = tokio::spawn(agent.run(cancel.clone()));
        let view = AgentView {
            id: spec.id.clone(),
            system: spec.system.clone(),
            state_dir: state_dir.to_path_buf(),
            reports,
        };
        Self {
            spec,
            view,
            cancel,
            task,
        }
    }

    /// Whether the session loop has returned.
    pub fn exited(&self) -> bool {
        self.task.is_finished()
    }

    /// Stops the agent and returns what `run` returned.
    pub async fn stop(self) -> buckle_core::Result<i32> {
        self.cancel.cancel();
        match tokio::time::timeout(Duration::from_secs(10), self.task).await {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => panic!("the agent task panicked: {e}"),
            Err(_) => panic!("the agent did not stop within 10s"),
        }
    }
}
