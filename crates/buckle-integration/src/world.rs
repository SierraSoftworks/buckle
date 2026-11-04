//! A test fleet: one server over a watched configuration directory, a proxy
//! in front of its gRPC port, and any number of agents.

use crate::agent::{AgentSpec, AgentView, Reports, RunningAgent};
use crate::api::{Api, Journal, Sse};
use crate::fleet::{self, Fleet};
use crate::proxy::Proxy;
use crate::server::{ServerPaths, ServerSettings, TestServer};
use crate::{WAIT, eventually};
use buckle_api::{
    Fleet as FleetView, HostRolloutState, HostSummary, Page, RevisionSummary, RolloutDetail,
    RolloutHost, RolloutPhase, RolloutSummary,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The host's row in a rollout, whichever phase it is in.
pub fn rollout_host<'a>(detail: &'a RolloutDetail, host: &str) -> Option<&'a RolloutHost> {
    detail
        .phases
        .iter()
        .flat_map(|p| p.hosts.iter())
        .find(|h| h.host == host)
}

/// A phase of a rollout by name.
pub fn phase<'a>(detail: &'a RolloutDetail, name: &str) -> Option<&'a RolloutPhase> {
    detail.phases.iter().find(|p| p.name == name)
}

/// A compact rendering of a rollout for failure messages.
pub fn describe(detail: &RolloutDetail) -> String {
    let mut out = format!(
        "rollout {} {:?} phase={:?} pause={:?}\n",
        &detail.summary.id[..8],
        detail.summary.state,
        detail.summary.current_phase,
        detail.summary.pause_reason
    );
    for p in &detail.phases {
        out.push_str(&format!("  phase {} {:?}\n", p.name, p.state));
        for h in &p.hosts {
            out.push_str(&format!(
                "    {} {:?} digest={:?} needs_approval={} error={:?} failed_checks={:?}\n",
                h.host,
                h.state,
                h.plan_digest.as_deref().map(|d| &d[..d.len().min(8)]),
                h.needs_approval,
                h.error,
                h.failed_checks
            ));
        }
    }
    out
}

/// The fleet under test.
pub struct World {
    /// Holds every directory of the test.
    pub dir: tempfile::TempDir,
    /// The server's directories (reused across restarts).
    pub paths: ServerPaths,
    settings: ServerSettings,
    proxy: Proxy,
    /// The pinned certificate fingerprint (the same across restarts).
    fingerprint: String,
    server: Option<TestServer>,
    api: Api,
    journal: Journal,
    agents: BTreeMap<String, RunningAgent>,
    stopped: BTreeMap<String, AgentSpec>,
    reports: BTreeMap<String, Reports>,
}

impl World {
    /// Starts a server with default settings over `fleet` and waits for the
    /// first revision to be promoted (there are no hosts yet, so its rollout
    /// completes at once and leaves its stragglers stage open).
    pub async fn start(fleet: Fleet) -> Self {
        Self::start_with(fleet, ServerSettings::default()).await
    }

    /// [`World::start`] with `settings`.
    pub async fn start_with(fleet: Fleet, settings: ServerSettings) -> Self {
        crate::init();
        let dir = tempfile::tempdir().expect("a temporary directory");
        let paths = ServerPaths {
            data: dir.path().join("data"),
            source: dir.path().join("config"),
            secrets: dir.path().join("secrets"),
            source_lock: Default::default(),
        };
        fleet.write(&paths.source);
        let proxy = Proxy::start().await;
        let journal: Journal = Arc::new(Mutex::new(Vec::new()));
        let server = TestServer::start(&paths, &settings).await;
        proxy.point_at(server.grpc);
        let api = Api::new(server.http, journal.clone());
        let world = Self {
            dir,
            paths,
            settings,
            proxy,
            fingerprint: server.fingerprint.clone(),
            server: Some(server),
            api,
            journal,
            agents: BTreeMap::new(),
            stopped: BTreeMap::new(),
            reports: BTreeMap::new(),
        };
        let first = world.source_revision();
        world
            .wait_fleet("the first revision to become stable", |f| {
                f.stable_revision.as_deref() == Some(first.as_str())
            })
            .await;
        world
    }

    /// The API of the current server instance.
    pub fn api(&self) -> &Api {
        &self.api
    }

    /// Every API response body so far, across server restarts.
    pub fn journal(&self) -> String {
        self.journal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .join("\n")
    }

    /// Subscribes to the live event stream of the current server.
    pub async fn sse(&self) -> Sse {
        Sse::connect(&self.api).await
    }

    /// Writes the value of a `{ file: NAME }` secret into the server's
    /// secrets root (read afresh on every resolution).
    pub fn write_secret(&self, name: &str, value: &str) {
        std::fs::create_dir_all(&self.paths.secrets).expect("create the secrets root");
        std::fs::write(self.paths.secrets.join(name), format!("{value}\n"))
            .expect("write a secret");
    }

    /// The revision id of the tree currently in the source directory.
    pub fn source_revision(&self) -> String {
        fleet::revision_id(&self.paths.source)
    }

    /// Writes `fleet` into the source directory and waits for the server to
    /// store the revision it produces (valid or rejected). Returns its id.
    pub async fn publish(&self, fleet: &Fleet) -> RevisionSummary {
        {
            // A scan sees the tree before or after, never half written.
            let _writing = self.paths.source_lock.write().await;
            fleet.write(&self.paths.source);
        }
        let id = self.source_revision();
        eventually(
            &format!("revision {} to be stored", &id[..8]),
            WAIT,
            async || {
                self.api
                    .try_get::<RevisionSummary>(&format!("/api/v1/revisions/{id}"))
                    .await
                    .ok_or_else(|| "not yet".to_owned())
            },
        )
        .await
    }

    // ------------------------------------------------------------ agents

    /// Starts an agent; its state lives under the test directory. Agents are
    /// named by their state directory (the host id unless
    /// [`AgentSpec::state_dir`] says otherwise).
    pub fn spawn(&mut self, spec: AgentSpec) -> AgentView {
        let name = spec.state.clone().unwrap_or_else(|| spec.id.clone());
        let state_dir = self.dir.path().join("agents").join(&name);
        let reports = self
            .reports
            .entry(name.clone())
            .or_insert_with(|| Arc::new(Mutex::new(Vec::new())))
            .clone();
        let running = RunningAgent::start(
            spec,
            &state_dir,
            self.proxy.addr(),
            &self.fingerprint,
            reports,
        );
        let view = running.view.clone();
        assert!(
            self.agents.insert(name.clone(), running).is_none(),
            "agent '{name}' is already running"
        );
        view
    }

    /// The view of a running (or stopped) agent.
    pub fn agent(&self, id: &str) -> AgentView {
        self.agents
            .get(id)
            .map(|a| a.view.clone())
            .unwrap_or_else(|| panic!("no agent '{id}'"))
    }

    /// Whether the agent's session loop has returned on its own.
    pub fn agent_exited(&self, id: &str) -> bool {
        self.agents.get(id).is_some_and(RunningAgent::exited)
    }

    /// Stops an agent, returning what its session loop returned.
    pub async fn stop_agent(&mut self, id: &str) -> buckle_core::Result<i32> {
        let running = self
            .agents
            .remove(id)
            .unwrap_or_else(|| panic!("no agent '{id}'"));
        let spec = running.spec.clone();
        let result = running.stop().await;
        // Keep the spec so the agent can be restarted over the same state.
        self.stopped.insert(id.to_owned(), spec);
        result
    }

    /// Starts a stopped agent again over the same state directory and host.
    pub fn restart_agent(&mut self, id: &str) -> AgentView {
        let spec = self
            .stopped
            .remove(id)
            .unwrap_or_else(|| panic!("agent '{id}' is not stopped"));
        self.spawn(spec)
    }

    // ------------------------------------------------------------ the server

    /// Cuts every agent off the server (the server keeps running).
    pub fn cut(&self) {
        self.proxy.cut();
    }

    /// A handle that cuts every agent off the server, as [`World::cut`].
    pub fn cutter(&self) -> crate::proxy::Cutter {
        self.proxy.cutter()
    }

    /// Lets agents reach the current server again.
    pub fn reconnect(&self) {
        if let Some(s) = &self.server {
            self.proxy.point_at(s.grpc);
        }
    }

    /// Cuts the agents off and stops the server. Returns its exit code.
    pub async fn stop_server(&mut self) -> i32 {
        self.proxy.cut();
        let server = self.server.take().expect("a running server");
        server.stop().await
    }

    /// Starts a fresh server instance over the same data and source
    /// directories (new ports) and lets the agents reach it.
    pub async fn start_server(&mut self) {
        assert!(self.server.is_none(), "a server is already running");
        let server = TestServer::start(&self.paths, &self.settings).await;
        assert_eq!(
            server.fingerprint, self.fingerprint,
            "the restarted server reuses its certificate"
        );
        self.api = Api::new(server.http, self.journal.clone());
        self.proxy.point_at(server.grpc);
        self.server = Some(server);
    }

    /// [`World::stop_server`] then [`World::start_server`].
    pub async fn restart_server(&mut self) {
        let code = self.stop_server().await;
        assert_eq!(code, 0, "the server exits cleanly");
        self.start_server().await;
    }

    // ------------------------------------------------------------ observing

    /// Waits until `/api/v1/fleet` satisfies `pred`.
    pub async fn wait_fleet(&self, what: &str, pred: impl Fn(&FleetView) -> bool) -> FleetView {
        eventually(what, WAIT, async || {
            let fleet = self
                .api
                .try_get::<FleetView>("/api/v1/fleet")
                .await
                .ok_or("the fleet view is unavailable")?;
            if pred(&fleet) {
                Ok(fleet)
            } else {
                Err(format!(
                    "stable={:?} hosts={:?}",
                    fleet.stable_revision,
                    fleet
                        .hosts
                        .iter()
                        .map(|h| (&h.id, h.status, h.connection, &h.applied_revision))
                        .collect::<Vec<_>>()
                ))
            }
        })
        .await
    }

    /// Waits until the host's summary satisfies `pred`.
    pub async fn wait_host(
        &self,
        host: &str,
        what: &str,
        pred: impl Fn(&HostSummary) -> bool,
    ) -> HostSummary {
        eventually(&format!("{host}: {what}"), WAIT, async || {
            let h = self
                .api
                .try_get::<HostSummary>(&format!("/api/v1/hosts/{host}"))
                .await
                .ok_or("the host is not known yet")?;
            if pred(&h) {
                Ok(h)
            } else {
                Err(format!("{h:?}"))
            }
        })
        .await
    }

    /// The newest rollout of `revision`, waiting for it to exist.
    pub async fn rollout_for(&self, revision: &str) -> RolloutDetail {
        let id = eventually(
            &format!("a rollout of {}", &revision[..8]),
            WAIT,
            async || {
                let page = self
                    .api
                    .try_get::<Page<RolloutSummary>>("/api/v1/rollouts")
                    .await
                    .ok_or("the rollout list is unavailable")?;
                page.items
                    .into_iter()
                    .find(|r| r.revision == revision)
                    .map(|r| r.id)
                    .ok_or_else(|| "no rollout yet".to_owned())
            },
        )
        .await;
        self.api.get(&format!("/api/v1/rollouts/{id}")).await
    }

    /// Waits until the rollout satisfies `pred`.
    pub async fn wait_rollout(
        &self,
        id: &str,
        what: &str,
        pred: impl Fn(&RolloutDetail) -> bool,
    ) -> RolloutDetail {
        self.wait_rollout_for(id, what, WAIT, pred).await
    }

    /// [`World::wait_rollout`] with an explicit timeout.
    pub async fn wait_rollout_for(
        &self,
        id: &str,
        what: &str,
        timeout: Duration,
        pred: impl Fn(&RolloutDetail) -> bool,
    ) -> RolloutDetail {
        eventually(&format!("rollout: {what}"), timeout, async || {
            let d = self
                .api
                .try_get::<RolloutDetail>(&format!("/api/v1/rollouts/{id}"))
                .await
                .ok_or("the rollout is unavailable")?;
            if pred(&d) { Ok(d) } else { Err(describe(&d)) }
        })
        .await
    }

    /// Waits until the host's row in the rollout is in `state`.
    pub async fn wait_rollout_host(
        &self,
        id: &str,
        host: &str,
        state: HostRolloutState,
    ) -> RolloutDetail {
        self.wait_rollout(id, &format!("{host} {state:?}"), |d| {
            rollout_host(d, host).is_some_and(|h| h.state == state)
        })
        .await
    }

    /// Waits until every host in `hosts` is healthy on `revision`: its row
    /// in the revision's rollout is healthy and the server records the
    /// revision as applied.
    pub async fn converge(&self, revision: &str, hosts: &[&str]) -> RolloutDetail {
        let rollout = self.rollout_for(revision).await;
        let detail = self
            .wait_rollout(
                &rollout.summary.id,
                &format!("{hosts:?} healthy on {}", &revision[..8]),
                |d| {
                    hosts.iter().all(|h| {
                        rollout_host(d, h).is_some_and(|r| r.state == HostRolloutState::Healthy)
                    })
                },
            )
            .await;
        for h in hosts {
            self.wait_host(h, "applied the revision", |s| {
                s.applied_revision.as_deref() == Some(revision)
            })
            .await;
        }
        detail
    }

    /// Approves `phase` of a rollout with the plan digests its hosts have
    /// now. Returns the digests approved.
    pub async fn approve(&self, rollout: &str, phase_name: &str) -> Vec<String> {
        let detail: RolloutDetail = self.api.get(&format!("/api/v1/rollouts/{rollout}")).await;
        let mut digests: Vec<String> = phase(&detail, phase_name)
            .unwrap_or_else(|| panic!("no phase {phase_name}"))
            .hosts
            .iter()
            .filter_map(|h| h.plan_digest.clone())
            .collect();
        digests.sort();
        digests.dedup();
        self.api
            .post_ok(
                &format!("/api/v1/rollouts/{rollout}/approve"),
                serde_json::json!({ "phase": phase_name, "plan_digests": digests, "note": "integration test" }),
            )
            .await;
        digests
    }

    /// Stops every agent and the server.
    pub async fn shutdown(mut self) {
        let ids: Vec<String> = self.agents.keys().cloned().collect();
        for id in ids {
            if let Some(a) = self.agents.remove(&id) {
                let _ = a.stop().await;
            }
        }
        if let Some(server) = self.server.take() {
            self.proxy.cut();
            let _ = server.stop().await;
        }
    }
}
