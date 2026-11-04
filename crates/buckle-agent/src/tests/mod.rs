//! Behavioural tests of the agent against an in-process fake server.
//!
//! Most scenarios run the real gRPC transport over in-memory pipes with tokio
//! time paused, so heartbeats, drift intervals, bake windows, backoff and the
//! local rollback watchdog run in virtual time; enrolment runs over a real
//! localhost TCP port.

mod fake_server;
mod hardening;
mod reboot;
mod scenarios;

use crate::config::AgentConfig;
use crate::session::{Agent, AgentOptions};
use crate::store::Store;
use crate::testing::{FakeRunner, FakeUpdater};
use crate::transport::Transport;
use crate::transport::grpc::GrpcTransport;
use buckle_proto::v1 as pb;
use fake_server::{FakeFleet, Sessions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// A minimal buckle.yaml with the rollout policy the watchdog reads.
pub const FLEET_YAML: &str = "version: 2\nrollout:\n  settle: 2m\n  local_rollback: { enabled: true, after: 5m, on_health: true }\n";

/// A revision bundle with `yaml` as its buckle.yaml.
pub fn bundle(yaml: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("buckle.yaml"), yaml).expect("write");
    crate::bundle::pack_dir(dir.path()).expect("pack")
}

pub struct Harness {
    pub fleet: FakeFleet,
    pub sessions: Sessions,
    pub runner: Arc<FakeRunner>,
    pub updater: Arc<FakeUpdater>,
    pub dir: tempfile::TempDir,
    pub shutdown: CancellationToken,
    pub handle: Option<JoinHandle<buckle_core::Result<i32>>>,
}

/// Prepares the state directory before the agent starts.
pub type Prepare = Box<dyn FnOnce(&Path)>;

pub struct Setup {
    pub yaml: String,
    pub runner: Arc<FakeRunner>,
    pub tcp: bool,
    /// Prepares the state directory before the agent starts.
    pub prepare: Option<Prepare>,
    /// Builds a runner to use instead of `runner`, given the state
    /// directory (`runner` is then only reachable if the wrapper delegates).
    pub custom_runner: Option<RunnerFactory>,
}

/// Builds a runner for a state directory.
pub type RunnerFactory = Box<dyn FnOnce(&Path) -> Arc<dyn crate::runner::Runner>>;

impl Default for Setup {
    fn default() -> Self {
        Self {
            yaml: String::new(),
            runner: FakeRunner::new(),
            tcp: false,
            prepare: None,
            custom_runner: None,
        }
    }
}

impl Harness {
    pub async fn start(setup: Setup) -> Self {
        let (fleet, sessions) = FakeFleet::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let state_dir = dir.path().join("state");
        if let Some(prepare) = setup.prepare {
            prepare(&state_dir);
        }
        let server = if setup.tcp {
            fleet.serve_tcp().await
        } else {
            "http://fake.buckle".to_owned()
        };
        let yaml = format!(
            "server: {server}\nhost_id: test-host\nstate_dir: {}\ntls: {{ insecure_plaintext: true }}\nlabels: {{ role: worker }}\n{}",
            state_dir.display(),
            setup.yaml
        );
        let config: AgentConfig = serde_yaml::from_str(&yaml).expect("config");
        let transport: Arc<dyn Transport> = if setup.tcp {
            Arc::new(GrpcTransport::new(&config).expect("transport"))
        } else {
            Arc::new(GrpcTransport::from_channel(
                fleet.in_memory_channel(),
                Store::new(&state_dir),
                config.bootstrap_token.clone(),
            ))
        };
        let updater = Arc::new(FakeUpdater::default());
        let options = AgentOptions {
            agent_version: "2.0.0-test".into(),
            backoff_jitter: false,
            ..Default::default()
        };
        let runner: Arc<dyn crate::runner::Runner> = match setup.custom_runner {
            Some(build) => build(&state_dir),
            None => setup.runner.clone(),
        };
        let agent = Agent::new(config, transport, runner, updater.clone(), options).expect("agent");
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(agent.run(shutdown.clone()));
        Self {
            fleet,
            sessions,
            runner: setup.runner,
            updater,
            dir,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    pub async fn stop(mut self) -> buckle_core::Result<i32> {
        self.shutdown.cancel();
        self.join().await
    }

    pub async fn join(&mut self) -> buckle_core::Result<i32> {
        let handle = self.handle.take().expect("running");
        tokio::time::timeout(fake_server::WAIT, handle)
            .await
            .expect("the agent stopped")
            .expect("the agent did not panic")
    }
}

pub fn welcome(stable: Option<&str>, issued_token: Option<&str>) -> pb::server_message::Msg {
    pb::server_message::Msg::Welcome(pb::Welcome {
        server_version: "2.0.0".into(),
        session_id: "s1".into(),
        stable_revision: stable.unwrap_or_default().into(),
        issued_token: issued_token.unwrap_or_default().into(),
        status: "active".into(),
        ..Default::default()
    })
}

pub fn assign(revision: &str, tree_digest: &str) -> pb::server_message::Msg {
    pb::server_message::Msg::Assign(pb::Assign {
        revision: revision.into(),
        tree_digest: tree_digest.into(),
        ..Default::default()
    })
}

pub fn request_plan(revision: &str) -> pb::server_message::Msg {
    pb::server_message::Msg::Plan(pb::RequestPlan {
        revision: revision.into(),
        select: String::new(),
    })
}

pub fn apply(revision: &str, digest: &str) -> pb::Apply {
    pb::Apply {
        revision: revision.into(),
        plan_digest: digest.into(),
        ..Default::default()
    }
}

pub fn plan_result(msg: &pb::agent_message::Msg) -> Option<pb::PlanResult> {
    match msg {
        pb::agent_message::Msg::Plan(p) => Some(p.clone()),
        _ => None,
    }
}

pub fn apply_result(msg: &pb::agent_message::Msg) -> Option<pb::ApplyResult> {
    match msg {
        pb::agent_message::Msg::Result(r) => Some(r.clone()),
        _ => None,
    }
}

pub fn health_report(msg: &pb::agent_message::Msg) -> Option<pb::HealthReport> {
    match msg {
        pb::agent_message::Msg::Health(h) => Some(h.clone()),
        _ => None,
    }
}
