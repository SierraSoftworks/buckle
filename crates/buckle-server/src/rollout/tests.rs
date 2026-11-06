//! Controller scenarios driven by scripted fake hosts on tokio's paused clock.

use super::controller::{CHECK_DRIFT_CAPABILITY, TARGETS_CAPABILITY, WORKLOADS_CAPABILITY};
use super::model::{HostPhase, RolloutHostRecord, RolloutRecord, STRAGGLERS};
use super::*;
use crate::clock::{Clock, TokioClock};
use crate::events::EventBus;
use crate::secrets::ServerSecrets;
use crate::source::cache::RevisionCache;
use crate::store::{NewHost, RevisionRecord, Store};
use buckle_api::{HostStatus, RevisionStatus, RolloutState, TargetKind};
use buckle_core::secrets::InstallKey;
use buckle_proto::v1::{
    self as pb, ApplyResult, HealthReport, HealthScope, LocalRollback, PlanResult,
    agent_message::Msg as AMsg, server_message::Msg as SMsg,
};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Default)]
struct Script {
    digest: String,
    destructive: bool,
    baseline_fail: bool,
    apply_fail: Option<Vec<String>>,
    bake_fail: bool,
    /// Bake checks of cluster targets fail (as well as with `bake_fail`).
    target_bake_fail: bool,
    gate_fail: bool,
    silent_apply: bool,
    /// On the next Apply, answer as the agent does: a PlanResult whose error
    /// starts with "stale plan" carrying this fresh digest.
    stale_plan_result: Option<String>,
    /// Refuse every Apply as stale, each time with a new fresh digest (a
    /// plan that is never deterministic).
    stale_plan_always: bool,
    local_rollback_on_bake: bool,
    /// The local rollback itself fails.
    local_rollback_fails: bool,
    /// Never answer baseline checks (the test answers them).
    silent_baseline: bool,
    /// The bake rollback is module-scoped (these modules).
    rollback_modules: Vec<String>,
    /// The apply ends with blocked steps (needs attention).
    apply_blocked: bool,
    /// Never answer pre-phase gate checks.
    silent_gate: bool,
    /// Never answer bake checks.
    silent_bake: bool,
    /// Planning fails with this error.
    plan_error: Option<String>,
    /// Never answer a Rollback.
    silent_rollback: bool,
    /// The plan defers these modules to apply time.
    deferred: Vec<String>,
    /// The intent digest the agent reports with its plans; an Apply binding
    /// it runs whatever the fresh plan (`digest`) is, as a current agent does.
    intent: String,
    /// The agent announces no capabilities (it predates force apply).
    legacy_agent: bool,
    /// On the next Apply, leave these deferred modules alone: their real
    /// plan diverged from the approved preview.
    stale_deferred: Vec<String>,
    /// The boot the agent says it runs in.
    boot_id: Option<String>,
    /// Applies ask for this reboot.
    reboot: Option<pb::Reboot>,
    /// The agent does not announce the `targets` capability (it runs no
    /// pipeline per target), so it is never picked as an executor.
    no_targets: bool,
    /// The agent does not announce the `workloads` capability (it plans a
    /// cluster whole), so clusters it could execute for are rolled as one
    /// row.
    no_workloads: bool,
    /// Never answer an Apply aimed at a cluster target (the agent's own
    /// applies are answered).
    silent_target_apply: bool,
    /// The modules a successful apply changed; None: `web`.
    changed: Option<Vec<String>>,
    /// The digest of plans for a cluster target; None: `digest`.
    target_digest: Option<String>,
    /// Planning a cluster target fails with this error.
    target_plan_error: Option<String>,
    /// Actions fail.
    action_fail: bool,
    /// The agent's own plans change nothing (and so do its applies).
    unchanged: bool,
    /// Plans for a cluster target change these workloads (a real plan).
    target_changes: Option<Vec<String>>,
    /// Every health report for a cluster target has this workload failing.
    failing_workload: Option<String>,
}

/// Report ids for the fake agents' durable action results.
static NEXT_REPORT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

type Log = Arc<Mutex<Vec<(DateTime<Utc>, String, SMsg)>>>;

struct Harness {
    _data: tempfile::TempDir,
    store: Store,
    sessions: Arc<Sessions>,
    revisions: Arc<RevisionCache>,
    clock: Arc<TokioClock>,
    handle: ControllerHandle,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    scripts: BTreeMap<String, Arc<Mutex<Script>>>,
    log: Log,
    session_ids: BTreeMap<String, u64>,
}

const KEY: [u8; 32] = [9; 32];

/// `buckle.yaml` with the tier groups, `clusters` (a `fleet.clusters` block,
/// or empty) and the `rollout` section.
fn fleet_yaml_with(rollout: &str, clusters: &str) -> String {
    format!(
        "version: 2\nfleet:\n  groups:\n    canary: 'host.labels.tier == \"canary\"'\n    web: 'host.labels.tier == \"web\"'\n    db: 'host.labels.tier == \"db\"'\n    nomad: 'host.labels.tier == \"nomad\"'\n{clusters}rollout:\n{rollout}"
    )
}

async fn start_controller(
    store: &Store,
    sessions: &Arc<Sessions>,
    revisions: &Arc<RevisionCache>,
    clock: &Arc<TokioClock>,
) -> (
    ControllerHandle,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let cancel = CancellationToken::new();
    let deps = ControllerDeps {
        store: store.clone(),
        link: sessions.clone(),
        clock: clock.clone(),
        events: EventBus::default(),
        revisions: revisions.clone(),
        secrets: ServerSecrets::new(&Default::default(), InstallKey::from_bytes(KEY)),
        approval_key: KEY,
        options: ControllerOptions::default(),
    };
    let (handle, task) = spawn(deps, cancel.clone())
        .await
        .expect("controller starts");
    (handle, cancel, task)
}

impl Harness {
    async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let store = Store::in_memory().await.unwrap();
        let sessions = Arc::new(Sessions::new());
        let revisions = Arc::new(RevisionCache::new(data.path()));
        let clock = Arc::new(TokioClock::new());
        let (handle, cancel, task) = start_controller(&store, &sessions, &revisions, &clock).await;
        Self {
            _data: data,
            store,
            sessions,
            revisions,
            clock,
            handle,
            cancel,
            task: Some(task),
            scripts: BTreeMap::new(),
            log: Arc::new(Mutex::new(Vec::new())),
            session_ids: BTreeMap::new(),
        }
    }

    /// Stops the controller and starts a fresh one from the store.
    async fn restart(&mut self) {
        self.cancel.cancel();
        if let Some(t) = self.task.take() {
            t.await.unwrap();
        }
        for (host, id) in std::mem::take(&mut self.session_ids) {
            self.sessions.detach(&host, id);
        }
        let (handle, cancel, task) =
            start_controller(&self.store, &self.sessions, &self.revisions, &self.clock).await;
        self.handle = handle;
        self.cancel = cancel;
        self.task = Some(task);
    }

    async fn revision(&self, id: &str, rollout: &str) {
        self.revision_with(id, rollout, "", &[]).await;
    }

    /// A revision with `clusters` declared and `files` (modules, workloads)
    /// beside `buckle.yaml`.
    async fn revision_with(&self, id: &str, rollout: &str, clusters: &str, files: &[(&str, &str)]) {
        let dir = self.revisions.dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("buckle.yaml"), fleet_yaml_with(rollout, clusters)).unwrap();
        for (path, content) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let now = self.clock.now();
        let rec = RevisionRecord {
            id: id.into(),
            commit: id.into(),
            author: "test".into(),
            message: format!("revision {id}"),
            committed_at: now,
            created_at: now,
            tarball_digest: Some("t".into()),
            status: RevisionStatus::Valid,
            errors: vec![],
            module_count: 0,
            source: "path".into(),
            validated_by: None,
            workload_count: None,
        };
        self.store
            .call(move |db| db.revisions().upsert(&rec))
            .await
            .unwrap();
    }

    async fn enrol(&mut self, host: &str, tier: &str) {
        let now = self.clock.now();
        let (h, t) = (host.to_owned(), tier.to_owned());
        self.store
            .call(move |db| {
                db.hosts().create(&NewHost {
                    id: h.clone(),
                    status: HostStatus::Active,
                    token_hash: None,
                    auth_method: "token".into(),
                    at: now,
                })?;
                let mut labels = BTreeMap::new();
                labels.insert("tier".to_string(), t);
                db.hosts().set_labels(&h, &labels)?;
                Ok(())
            })
            .await
            .unwrap();
        self.scripts.insert(
            host.to_owned(),
            Arc::new(Mutex::new(Script {
                digest: "plan-a".into(),
                ..Default::default()
            })),
        );
    }

    /// An in-cluster executor of `cluster`, enrolled by workload identity.
    async fn enrol_executor(&mut self, host: &str, cluster: &str) {
        let now = self.clock.now();
        let (h, method) = (
            host.to_owned(),
            format!("{}{cluster}", crate::store::WORKLOAD_AUTH_PREFIX),
        );
        self.store
            .call(move |db| {
                db.hosts()
                    .create(&NewHost {
                        id: h,
                        status: HostStatus::Active,
                        token_hash: None,
                        auth_method: method,
                        at: now,
                    })
                    .map(|_| ())
            })
            .await
            .unwrap();
        self.scripts.insert(
            host.to_owned(),
            Arc::new(Mutex::new(Script {
                digest: "plan-a".into(),
                ..Default::default()
            })),
        );
    }

    fn script(&self, host: &str, f: impl FnOnce(&mut Script)) {
        f(&mut self.scripts[host].lock().unwrap());
    }

    fn connect(&mut self, host: &str, applied: Option<&str>) {
        self.attach(host);
        self.connect_with(host, applied, vec![]);
    }

    /// Attaches a scripted session for `host` without announcing it.
    fn attach(&mut self, host: &str) {
        let (id, mut rx) = self.sessions.attach(host);
        self.session_ids.insert(host.to_owned(), id);
        let handle = self.handle.clone();
        let script = self.scripts[host].clone();
        let log = self.log.clone();
        let clock = self.clock.clone();
        let name = host.to_owned();
        tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                let Some(m) = m.msg else { continue };
                log.lock()
                    .unwrap()
                    .push((clock.now(), name.clone(), m.clone()));
                let s = script.lock().unwrap().clone();
                let reply = |msg: AMsg| handle.agent_message(&name, msg);
                match m {
                    SMsg::Plan(p)
                        if s.plan_error.is_some()
                            || (!p.target.is_empty() && s.target_plan_error.is_some()) =>
                    {
                        reply(AMsg::Plan(PlanResult {
                            target: p.target.clone(),
                            workload: p.workload.clone(),
                            revision: p.revision,
                            error: s
                                .plan_error
                                .clone()
                                .or(s.target_plan_error.clone())
                                .unwrap_or_default(),
                            ..Default::default()
                        }))
                    }
                    SMsg::Plan(p) => reply(AMsg::Plan(PlanResult {
                        digest: match &s.target_digest {
                            Some(d) if !p.target.is_empty() => d.clone(),
                            _ => s.digest.clone(),
                        },
                        changes: match &s.target_changes {
                            Some(w) if !p.target.is_empty() => w.len() as u32,
                            _ => u32::from(!(s.unchanged && p.target.is_empty())),
                        },
                        plan_json: match &s.target_changes {
                            Some(w) if !p.target.is_empty() => plan_json_changing(w),
                            _ => b"{}".to_vec(),
                        },
                        target: p.target.clone(),
                        workload: p.workload.clone(),
                        revision: p.revision,
                        destructive: s.destructive,
                        removals: u32::from(s.destructive),
                        deferred_modules: s.deferred.clone(),
                        approval_digest: s.intent.clone(),
                        ..Default::default()
                    })),
                    SMsg::Health(h) => {
                        let scope = HealthScope::try_from(h.scope).unwrap();
                        if scope == HealthScope::Baseline && s.silent_baseline
                            || scope == HealthScope::Rollout && s.silent_gate
                            || scope == HealthScope::Bake && s.silent_bake
                        {
                            continue;
                        }
                        let fail = match scope {
                            HealthScope::Baseline => s.baseline_fail,
                            HealthScope::Bake => {
                                s.bake_fail || (s.target_bake_fail && !h.target.is_empty())
                            }
                            HealthScope::Rollout => s.gate_fail,
                            _ => false,
                        };
                        if scope == HealthScope::Bake && s.local_rollback_on_bake {
                            reply(AMsg::Rollback(LocalRollback {
                                modules: s.rollback_modules.clone(),
                                from_revision: String::new(),
                                to_revision: "stable".into(),
                                reason: "server unreachable and health failing".into(),
                                succeeded: !s.local_rollback_fails,
                                rollback_error: if s.local_rollback_fails {
                                    "apt is locked".into()
                                } else {
                                    String::new()
                                },
                                ..Default::default()
                            }));
                            continue;
                        }
                        let check = |module: &str, fail: bool| pb::HealthResult {
                            module: module.into(),
                            check: "http".into(),
                            kind: "http".into(),
                            required: true,
                            passed: !fail,
                            reason: if fail { "503".into() } else { String::new() },
                            ..Default::default()
                        };
                        let mut results = vec![check("web", fail)];
                        if let Some(w) = s
                            .failing_workload
                            .as_deref()
                            .filter(|_| !h.target.is_empty())
                        {
                            results.push(check(w, true));
                        }
                        reply(AMsg::Health(HealthReport {
                            target: h.target.clone(),
                            workload: h.workload.clone(),
                            scope: h.scope,
                            request_id: h.request_id,
                            revision: String::new(),
                            results,
                        }));
                    }
                    SMsg::Apply(mut a) => {
                        if s.silent_apply || (s.silent_target_apply && !a.target.is_empty()) {
                            continue;
                        }
                        // A force apply, or an Apply binding the plan's
                        // unchanged intent, runs the fresh plan: the agent
                        // reports it, then its result.
                        let bound_intent =
                            !a.approval_digest.is_empty() && a.approval_digest == s.intent;
                        if a.force_plan || (bound_intent && a.plan_digest != s.digest) {
                            reply(AMsg::Plan(PlanResult {
                                target: a.target.clone(),
                                workload: a.workload.clone(),
                                revision: a.revision.clone(),
                                digest: s.digest.clone(),
                                changes: 1,
                                plan_json: b"{}".to_vec(),
                                approval_digest: s.intent.clone(),
                                ..Default::default()
                            }));
                            a.plan_digest = s.digest.clone();
                        }
                        let fresh = s
                            .stale_plan_result
                            .clone()
                            .or_else(|| s.stale_plan_always.then(|| format!("{}-fresh", s.digest)))
                            .filter(|_| !a.force_plan && !bound_intent);
                        if let Some(fresh) = fresh {
                            {
                                let mut sc = script.lock().unwrap();
                                sc.stale_plan_result = None;
                                sc.digest = fresh.clone();
                            }
                            reply(AMsg::Plan(PlanResult {
                                target: a.target.clone(),
                                workload: a.workload.clone(),
                                revision: a.revision,
                                digest: fresh.clone(),
                                changes: 3,
                                error: format!(
                                    "stale plan: the approved plan was {} but the host now plans {fresh}",
                                    a.plan_digest
                                ),
                                ..Default::default()
                            }));
                            continue;
                        }
                        if !s.stale_deferred.is_empty() {
                            script.lock().unwrap().stale_deferred.clear();
                            reply(AMsg::Result(ApplyResult {
                                target: a.target.clone(),
                                workload: a.workload.clone(),
                                revision: a.revision.clone(),
                                plan_digest: a.plan_digest,
                                succeeded: false,
                                stale_modules: s.stale_deferred.clone(),
                                error: "module 'web': not applied".into(),
                                ..Default::default()
                            }));
                            continue;
                        }
                        if s.apply_blocked {
                            reply(AMsg::Result(ApplyResult {
                                target: a.target.clone(),
                                workload: a.workload.clone(),
                                revision: a.revision.clone(),
                                plan_digest: a.plan_digest,
                                succeeded: false,
                                needs_attention: true,
                                error: "module 'web': step waits for a lock".into(),
                                ..Default::default()
                            }));
                            continue;
                        }
                        let failed = s.apply_fail.clone();
                        let changed = if s.unchanged && a.target.is_empty() {
                            vec![]
                        } else {
                            s.changed.clone().unwrap_or_else(|| vec!["web".into()])
                        };
                        reply(AMsg::Result(ApplyResult {
                            target: a.target.clone(),
                            workload: a.workload.clone(),
                            revision: a.revision.clone(),
                            plan_digest: a.plan_digest,
                            succeeded: failed.is_none(),
                            applied_revision: a.revision,
                            failed_module: if failed.is_some() {
                                "web".into()
                            } else {
                                String::new()
                            },
                            failed_checks: failed.clone().unwrap_or_default(),
                            error: if failed.is_some() {
                                "health check failed".into()
                            } else {
                                String::new()
                            },
                            error_detail: failed.as_ref().map(|_| buckle_proto::v1::ErrorDetail {
                                kind: "user".into(),
                                message: "health check failed".into(),
                                causes: vec![],
                                advice: vec!["Look at web's health checks.".into()],
                            }),
                            reboot: s.reboot.clone(),
                            changes: changed.len() as u32,
                            outcome_json: outcome_json(&changed),
                            ..Default::default()
                        }));
                    }
                    // An action's result is durable, as the agent's is.
                    SMsg::Action(a) => {
                        let id = NEXT_REPORT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let failed = s.action_fail;
                        // The ack is not awaited: the controller reacts either way.
                        drop(handle.durable_report(
                            &name,
                            id,
                            AMsg::Action(pb::ActionResult {
                                action_id: a.action_id,
                                module: a.module,
                                action: a.action,
                                revision: a.revision,
                                target: a.target,
                                // An agent predating placements echoes none.
                                workload: if s.no_workloads {
                                    String::new()
                                } else {
                                    a.workload
                                },
                                report_id: id,
                                succeeded: !failed,
                                error: if failed {
                                    "apt is locked".into()
                                } else {
                                    String::new()
                                },
                                changes: 1,
                                reboot: s.reboot.clone(),
                                ..Default::default()
                            }),
                        ));
                    }
                    SMsg::Rollback(_) if s.silent_rollback => {}
                    SMsg::Rollback(r) => reply(AMsg::Result(ApplyResult {
                        target: r.target.clone(),
                        workload: r.workload.clone(),
                        revision: r.revision.clone(),
                        succeeded: true,
                        applied_revision: r.revision,
                        ..Default::default()
                    })),
                    _ => {}
                }
            }
        });
    }

    /// Connects `host` with a Hello naming `unreported` apply revisions.
    fn connect_with(&mut self, host: &str, applied: Option<&str>, unreported: Vec<String>) {
        let (legacy, no_targets, no_workloads) = {
            let s = self.scripts[host].lock().unwrap();
            (s.legacy_agent, s.no_targets, s.no_workloads)
        };
        self.handle.connected(Connected {
            host: host.to_owned(),
            applied_revision: applied.map(str::to_owned),
            last_plan_digest: None,
            unreported_apply_revisions: unreported,
            new_session: true,
            capabilities: if legacy {
                vec![]
            } else if no_targets {
                vec!["force_plan".into(), "approval_digest".into()]
            } else if no_workloads {
                vec![
                    "force_plan".into(),
                    "approval_digest".into(),
                    TARGETS_CAPABILITY.into(),
                ]
            } else {
                vec![
                    "force_plan".into(),
                    "approval_digest".into(),
                    TARGETS_CAPABILITY.into(),
                    WORKLOADS_CAPABILITY.into(),
                    CHECK_DRIFT_CAPABILITY.into(),
                ]
            },
            boot_id: self.scripts[host].lock().unwrap().boot_id.clone(),
        });
    }

    fn disconnect(&mut self, host: &str) {
        if let Some(id) = self.session_ids.remove(host) {
            self.sessions.detach(host, id);
        }
        self.handle.disconnected(host);
    }

    async fn start(&self, revision: &str) -> String {
        self.handle
            .command(
                "alice",
                Command::Start {
                    revision: revision.into(),
                    note: String::new(),
                },
            )
            .await
            .expect("rollout starts")
    }

    async fn snapshot(&self, id: &str) -> (RolloutRecord, Vec<RolloutHostRecord>) {
        let id = id.to_owned();
        self.store
            .call(move |db| {
                let r = db.rollouts().get(&id)?.expect("rollout exists");
                Ok((r, db.rollouts().hosts(&id)?))
            })
            .await
            .unwrap()
    }

    /// The cluster's steward as its record shows it.
    async fn steward(&self, target: &str) -> Option<String> {
        let t = target.to_owned();
        self.store
            .call(move |db| Ok(db.hosts().target(&t)?.and_then(|r| r.steward)))
            .await
            .unwrap()
    }

    async fn wait(
        &self,
        id: &str,
        what: &str,
        pred: impl Fn(&RolloutRecord, &[RolloutHostRecord]) -> bool,
    ) -> (RolloutRecord, Vec<RolloutHostRecord>) {
        for _ in 0..20_000 {
            let (r, h) = self.snapshot(id).await;
            if pred(&r, &h) {
                return (r, h);
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let (r, h) = self.snapshot(id).await;
        panic!(
            "timed out waiting for {what}: state {:?} pause {:?}\n{:#?}",
            r.state,
            r.runtime.pause,
            h.iter()
                .map(|h| (&h.host, &h.phase, h.state, &h.runtime.error))
                .collect::<Vec<_>>()
        );
    }

    async fn wait_state(
        &self,
        id: &str,
        state: RolloutState,
    ) -> (RolloutRecord, Vec<RolloutHostRecord>) {
        self.wait(id, &format!("{state:?}"), |r, _| r.state == state)
            .await
    }

    /// Waits until `host` is applying and its Apply has reached the host's
    /// session, and returns that Apply. The controller saves the Applying
    /// state before it sends the message, so the stored state alone can be
    /// seen while the Apply is still on its way.
    async fn wait_apply(&self, id: &str, host_id: &str) -> pb::Apply {
        self.wait(id, "applying", |_, hs| {
            host(hs, host_id).state == HostPhase::Applying
        })
        .await;
        for _ in 0..600 {
            if let Some((_, _, a)) = self.applies().into_iter().find(|a| a.1 == host_id) {
                return a;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        panic!("no Apply reached {host_id}");
    }

    /// Lets the controller settle for `secs` virtual seconds.
    async fn idle(&self, secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    fn sent(&self, pred: impl Fn(&str, &SMsg) -> bool) -> Vec<(DateTime<Utc>, String, SMsg)> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, h, m)| pred(h, m))
            .cloned()
            .collect()
    }

    fn applies(&self) -> Vec<(DateTime<Utc>, String, pb::Apply)> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(t, h, m)| match m {
                SMsg::Apply(a) => Some((*t, h.clone(), a.clone())),
                _ => None,
            })
            .collect()
    }

    async fn command(&self, cmd: Command) -> Result<String, CommandError> {
        self.handle.command("alice", cmd).await
    }

    /// Makes `revision` the stable one (the controller reads it on start).
    async fn stable(&mut self, revision: &str) {
        let r = revision.to_owned();
        self.store
            .call(move |db| db.kv().set("stable_revision", &r))
            .await
            .unwrap();
        self.restart().await;
    }

    /// Starts an action rollout of `common/upgrade` on `targets` (each
    /// running `r1`).
    async fn start_action(&self, targets: &[&str], confirm: bool) -> Result<String, CommandError> {
        self.command(Command::StartAction {
            action: RolloutAction {
                module: "common".into(),
                action: "upgrade".into(),
                label: "Upgrade packages".into(),
                confirm,
                selector: None,
            },
            targets: targets
                .iter()
                .map(|t| (t.to_string(), "r1".to_string()))
                .collect(),
            note: String::new(),
        })
        .await
    }
}

/// A plan's JSON changing one resource of each of `modules`.
fn plan_json_changing(modules: &[String]) -> Vec<u8> {
    let modules: Vec<_> = modules
        .iter()
        .map(|m| {
            serde_json::json!({"module": m, "steps": [{
                "task": {"module": m, "id": "job", "kind": "nomad.job"},
                "resource": {"module": m, "kind": "nomad.job", "key": m},
                "planned": {"outcome": "change", "diff": {"kind": "opaque", "summary": "update"}},
            }]})
        })
        .collect();
    serde_json::to_vec(
        &serde_json::json!({"revision": "r1", "host": "c", "digest": "d", "modules": modules}),
    )
    .unwrap()
}

/// An agent's `ApplyOutcome` JSON for a run that changed `changed`.
fn outcome_json(changed: &[String]) -> Vec<u8> {
    let modules: Vec<_> = changed
        .iter()
        .map(|m| serde_json::json!({"module": m, "status": "applied", "changes": 1, "duration_ms": 0}))
        .collect();
    let json = serde_json::json!({
        "plan_digest": "d", "succeeded": true, "changes": changed.len(), "removals": 0,
        "duration_ms": 0, "module_results": modules, "rolled_back_modules": [], "not_started": [],
    });
    serde_json::from_value::<buckle_core::engine::ApplyOutcome>(json.clone())
        .expect("the outcome parses as an agent's");
    serde_json::to_vec(&json).unwrap()
}

fn host<'a>(hosts: &'a [RolloutHostRecord], id: &str) -> &'a RolloutHostRecord {
    hosts
        .iter()
        .find(|h| h.host == id)
        .expect("host in rollout")
}

const TWO_PHASES: &str = "  settle: 2m\n  health: { bake: 1m }\n  phases:\n    - { name: canary, select: 'host.groups contains \"canary\"', approval: required }\n    - { name: web, select: 'host.groups contains \"web\"', parallel: 2 }\n";

const GATED_SECOND: &str = "  settle: 2m\n  health: { bake: 5m }\n  phases:\n    - { name: canary, select: 'host.groups contains \"canary\"' }\n    - { name: web, select: 'host.groups contains \"web\"', approval: required }\n";

#[tokio::test(start_paused = true)]
async fn a_phase_is_approved_only_once_the_phases_before_it_finish() {
    let mut h = Harness::new().await;
    h.revision("r1", GATED_SECOND).await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.connect("c1", None);
    h.connect("w1", None);
    let id = h.start("r1").await;
    h.wait(&id, "web planned while the canary runs", |_, hs| {
        host(hs, "w1").plan_digest.is_some() && host(hs, "c1").state != HostPhase::Healthy
    })
    .await;
    let approve = || Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    };
    let early = h.command(approve()).await.unwrap_err();
    assert_eq!(early.kind, CommandErrorKind::Conflict);
    assert!(
        early.error.to_string().contains("canary"),
        "{}",
        early.error
    );

    h.wait_state(&id, RolloutState::AwaitingApproval).await;
    h.command(approve()).await.unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
}

#[tokio::test(start_paused = true)]
async fn approval_gates_the_canary_and_waves_respect_parallel() {
    let mut h = Harness::new().await;
    h.revision("r1", TWO_PHASES).await;
    h.enrol("c1", "canary").await;
    for w in ["w1", "w2", "w3", "w4"] {
        h.enrol(w, "web").await;
    }
    for id in ["c1", "w1", "w2", "w3", "w4"] {
        h.connect(id, None);
    }
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(&id, "approval with every plan in", |r, hs| {
            r.state == RolloutState::AwaitingApproval && hs.iter().all(|x| x.plan_digest.is_some())
        })
        .await;
    assert!(
        hosts
            .iter()
            .all(|x| x.plan_digest.as_deref() == Some("plan-a")),
        "plans everywhere first"
    );
    h.idle(600).await;
    assert!(h.applies().is_empty(), "nothing applies before approval");

    let refused = h
        .command(Command::Approve {
            rollout: id.clone(),
            phase: "canary".into(),
            digests: vec!["other".into()],
            note: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.kind, CommandErrorKind::Conflict);

    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "canary".into(),
        digests: vec!["plan-a".into()],
        note: "lgtm".into(),
    })
    .await
    .unwrap();
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));

    let applies = h.applies();
    assert_eq!(applies[0].1, "c1", "the canary goes first");
    let web: Vec<_> = applies.iter().filter(|a| a.1.starts_with('w')).collect();
    assert_eq!(web.len(), 4);
    assert!(
        web[0].0 >= applies[0].0 + chrono::Duration::minutes(1),
        "web waits for the canary to bake"
    );
    assert!(
        web[2].0 >= web[0].0 + chrono::Duration::minutes(1),
        "the third web host waits for a slot (parallel 2)"
    );
    let mut waves: Vec<u32> = hosts
        .iter()
        .filter(|x| x.phase == "web")
        .filter_map(|x| x.wave)
        .collect();
    waves.sort();
    assert_eq!(waves, vec![1, 1, 2, 2]);
    assert!(!applies[0].2.approval_token.is_empty());
    assert_eq!(
        applies[0].2.approval_token,
        approval_token(&KEY, &id, "canary", "plan-a")
    );
    // Promoted.
    let stable = h
        .store
        .call(|db| db.kv().get("stable_revision"))
        .await
        .unwrap();
    assert_eq!(stable.as_deref(), Some("r1"));
    assert!(r.runtime.stragglers_open);
}

#[tokio::test(start_paused = true)]
async fn if_destructive_gates_only_destructive_plans() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', parallel: 2, approval: if_destructive }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.script("a", |s| {
        s.destructive = true;
        s.digest = "plan-destroy".into();
    });
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(&id, "b healthy", |_, hs| {
            host(hs, "b").state == HostPhase::Healthy
        })
        .await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Planned);
    let (r, _) = h.wait_state(&id, RolloutState::AwaitingApproval).await;
    let detail = super::view::detail(&r, &hosts, &[], &[]);
    let a = detail.phases[0]
        .hosts
        .iter()
        .find(|x| x.host == "a")
        .unwrap();
    assert!(a.needs_approval);
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-destroy".into(), "plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
}

#[tokio::test(start_paused = true)]
async fn a_failing_baseline_holds_the_host_until_forced() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.baseline_fail = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "held", |_, hs| host(hs, "a").state == HostPhase::Held)
        .await;
    h.idle(3600).await;
    assert!(h.applies().is_empty(), "a held host is not touched");
    let (r, _) = h.snapshot(&id).await;
    assert!(r.is_running());
    h.command(Command::Force {
        rollout: id.clone(),
        host: "a".into(),
        workload: String::new(),
        note: "known flaky".into(),
    })
    .await
    .unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
    assert!(h.applies()[0].2.force);
}

#[tokio::test(start_paused = true)]
async fn max_failures_zero_pauses_on_the_first_failure() {
    let mut h = Harness::new().await;
    h.revision("r1", "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', max_failures: 0 }\n").await;
    for x in ["a", "b", "c"] {
        h.enrol(x, "web").await;
        h.connect(x, None);
    }
    h.script("a", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r1").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    assert_eq!(r.runtime.pause.as_ref().unwrap().kind, "failure_budget");
    assert_eq!(host(&hosts, "a").state, HostPhase::Failed);
    h.idle(600).await;
    assert_eq!(h.applies().len(), 1, "nothing else starts once paused");
}

#[tokio::test(start_paused = true)]
async fn a_failed_hosts_structured_error_is_stored_once_and_shown_on_its_row() {
    let mut h = Harness::new().await;
    h.revision("r1", "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', max_failures: 0 }\n").await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    h.script("a", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r1").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    let a = host(&hosts, "a");
    let stored = a
        .runtime
        .error_detail
        .clone()
        .expect("stored with the result");
    assert_eq!(stored.advice, vec!["Look at web's health checks."]);
    let row = super::view::host(&r, a, &[]);
    assert_eq!(row.error_detail, Some(stored));
    // The view reads the stored error, not the result document.
    let mut without = a.clone();
    without.runtime.error_detail = None;
    assert_eq!(super::view::host(&r, &without, &[]).error_detail, None);
}

#[tokio::test(start_paused = true)]
async fn a_percentage_budget_pauses_on_the_right_count() {
    let mut h = Harness::new().await;
    h.revision("r1", "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', max_failures: 10% }\n").await;
    let names: Vec<String> = (0..10).map(|i| format!("h{i:02}")).collect();
    for n in &names {
        h.enrol(n, "web").await;
        h.connect(n, None);
    }
    h.script("h00", |s| s.apply_fail = Some(vec![]));
    h.script("h01", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r1").await;
    let (r, _) = h.wait_state(&id, RolloutState::Paused).await;
    assert!(
        r.runtime
            .pause
            .as_ref()
            .unwrap()
            .reason
            .contains("2 failed"),
        "{:?}",
        r.runtime.pause
    );
    assert_eq!(
        h.applies().len(),
        2,
        "10% of 10 tolerates one failure; the second pauses"
    );
}

/// A host bakes for its phase's `bake`, raised by the `bake` of each module
/// its run changed; a host whose run changed nothing does not bake.
#[tokio::test(start_paused = true)]
async fn a_host_bakes_for_the_longest_bake_among_the_modules_it_changed() {
    let mut h = Harness::new().await;
    h.revision_with(
        "r1",
        "  health: { bake: 1m }\n  phases:\n    - { name: web, select: 'true', parallel: 3 }\n",
        "",
        &[
            ("modules/web/module.yaml", "name: web\n"),
            ("modules/nomad/module.yaml", "name: nomad\nbake: 10m\n"),
            ("modules/shell/module.yaml", "name: shell\nbake: 5s\n"),
        ],
    )
    .await;
    for (host, changed) in [
        ("a", vec!["nomad", "web"]),
        ("b", vec!["shell", "web"]),
        ("c", vec![]),
    ] {
        h.enrol(host, "web").await;
        h.script(host, |s| {
            s.changed = Some(changed.into_iter().map(String::from).collect())
        });
        h.connect(host, None);
    }
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(
            &id,
            "the unchanged host done while the others bake",
            |_, hs| {
                host(hs, "c").state == HostPhase::Healthy
                    && host(hs, "a").state == HostPhase::Baking
                    && host(hs, "b").state == HostPhase::Baking
            },
        )
        .await;
    let window = |id: &str| host(&hosts, id).runtime.bake.clone().unwrap();
    assert_eq!(window("a").seconds, 600);
    assert_eq!(window("a").module.as_deref(), Some("nomad"));
    assert_eq!(window("b").seconds, 60, "the phase's bake is the minimum");
    assert!(window("c").unchanged);
    let (_, hosts) = h
        .wait(&id, "b baked", |_, hs| {
            host(hs, "b").state == HostPhase::Healthy
        })
        .await;
    assert_eq!(
        host(&hosts, "a").state,
        HostPhase::Baking,
        "nomad's bake outlasts the phase's"
    );
    h.wait_state(&id, RolloutState::Completed).await;
}

#[tokio::test(start_paused = true)]
async fn a_bake_regression_past_settle_pauses() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  settle: 2m\n  health: { bake: 10m }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.script("a", |s| s.bake_fail = true);
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Failed);
    assert_eq!(a.runtime.failed_checks, vec!["web/http"]);
    // Measure from the first failing report the controller saw rather than from
    // the Apply: on a paused clock, store writes on blocking threads let virtual
    // time jump ahead between the two, which made an Apply-relative bound flaky.
    let failing_since = *a
        .runtime
        .failing_since
        .get("web/http")
        .expect("the regression is still recorded on the host");
    let paused_at = r.runtime.pause.as_ref().unwrap().at;
    assert!(
        paused_at - failing_since >= chrono::Duration::minutes(2),
        "settle is honoured"
    );
    assert!(
        paused_at - failing_since < chrono::Duration::minutes(3),
        "the pause follows settle rather than waiting for the bake window"
    );
    assert!(
        r.runtime
            .pause
            .as_ref()
            .unwrap()
            .reason
            .contains("regression"),
        "{:?}",
        r.runtime.pause
    );
}

#[tokio::test(start_paused = true)]
async fn a_flap_inside_settle_is_not_a_failure() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  settle: 2m\n  health: { bake: 5m }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.bake_fail = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "baking", |_, hs| {
        host(hs, "a").state == HostPhase::Baking
    })
    .await;
    h.idle(30).await;
    // The check recovers within settle.
    h.handle.agent_message(
        "a",
        AMsg::Health(HealthReport {
            target: String::new(),
            workload: String::new(),
            scope: HealthScope::Bake as i32,
            request_id: String::new(),
            revision: String::new(),
            results: vec![pb::HealthResult {
                module: "web".into(),
                check: "http".into(),
                required: true,
                passed: true,
                ..Default::default()
            }],
        }),
    );
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "a").runtime.flaps, vec!["web/http"]);
}

#[tokio::test(start_paused = true)]
async fn a_failing_pre_phase_gate_pauses_before_the_next_phase() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        TWO_PHASES.replace(", approval: required", "").as_str(),
    )
    .await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.script("c1", |s| s.gate_fail = true);
    h.connect("c1", None);
    h.connect("w1", None);
    let id = h.start("r1").await;
    let (r, _) = h.wait_state(&id, RolloutState::Paused).await;
    let pause = r.runtime.pause.unwrap();
    assert_eq!(pause.kind, "gate");
    assert!(pause.reason.contains("c1") && pause.reason.contains("web/http"));
    assert!(
        h.applies().iter().all(|a| a.1 == "c1"),
        "the web phase never starts"
    );
}

#[tokio::test(start_paused = true)]
async fn continue_waives_the_failed_checks_and_finishes() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.apply_fail = Some(vec!["web/http".into()]));
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::Paused).await;
    h.script("a", |s| s.apply_fail = None);
    h.command(Command::Continue {
        rollout: id.clone(),
        host: "a".into(),
        workload: String::new(),
        waive: vec!["web/http".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let applies = h.applies();
    assert_eq!(applies.len(), 2);
    assert_eq!(applies[1].2.waived_checks, vec!["web/http"]);
    assert_eq!(host(&hosts, "a").runtime.waived_checks, vec!["web/http"]);
}

#[tokio::test(start_paused = true)]
async fn rollback_applies_the_stable_revision_newest_phase_first() {
    let mut h = Harness::new().await;
    let three = "  health: { bake: 30s }\n  phases:\n    - { name: canary, select: 'host.groups contains \"canary\"' }\n    - { name: web, select: 'host.groups contains \"web\"' }\n    - { name: db, select: 'host.groups contains \"db\"' }\n";
    h.revision("r1", three).await;
    h.revision("r2", three).await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.enrol("d1", "db").await;
    for x in ["c1", "w1", "d1"] {
        h.connect(x, None);
    }
    let first = h.start("r1").await;
    h.wait_state(&first, RolloutState::Completed).await;
    h.script("d1", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r2").await;
    let (r, _) = h.wait_state(&id, RolloutState::Paused).await;
    assert_eq!(r.runtime.previous_stable.as_deref(), Some("r1"));
    h.command(Command::Rollback {
        rollout: id.clone(),
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::RolledBack).await;
    let order: Vec<String> = h
        .sent(|_, m| matches!(m, SMsg::Rollback(_)))
        .into_iter()
        .map(|(_, host, m)| {
            if let SMsg::Rollback(rb) = m {
                assert_eq!(rb.revision, "r1");
            }
            host
        })
        .collect();
    assert_eq!(order, vec!["d1", "w1", "c1"]);
    assert_eq!(host(&hosts, "c1").state, HostPhase::RolledBack);
    let stable = h
        .store
        .call(|db| db.kv().get("stable_revision"))
        .await
        .unwrap();
    assert_eq!(
        stable.as_deref(),
        Some("r1"),
        "a rolled-back revision is never promoted"
    );
}

#[tokio::test(start_paused = true)]
async fn stragglers_stage_applies_late_hosts_and_carries_over() {
    let mut h = Harness::new().await;
    let policy = "  health: { bake: 30s }\n  stragglers: { parallel: 2 }\n  phases:\n    - { name: web, select: 'true' }\n";
    h.revision("r1", policy).await;
    h.revision("r2", policy).await;
    h.enrol("a", "web").await;
    h.enrol("offline", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(r.runtime.stragglers_open);
    let off = host(&hosts, "offline");
    assert_eq!(off.phase, STRAGGLERS);
    assert_eq!(off.state, HostPhase::PlanPending);

    // The missed host comes online and converges in the stragglers stage.
    h.connect("offline", None);
    h.wait(&id, "straggler healthy", |_, hs| {
        host(hs, "offline").state == HostPhase::Healthy
    })
    .await;

    // A host enrolled after promotion joins the stage when it connects.
    h.enrol("late", "web").await;
    h.connect("late", None);
    h.wait(&id, "late host healthy", |_, hs| {
        hs.iter()
            .any(|x| x.host == "late" && x.state == HostPhase::Healthy)
    })
    .await;

    // One more straggler that never shows up is carried to the next rollout.
    h.enrol("ghost", "web").await;
    h.handle.host_changed("ghost");
    let second = h.start("r2").await;
    let (r2, hosts2) = h.wait_state(&second, RolloutState::Completed).await;
    assert!(r2.runtime.stragglers_open);
    assert_eq!(host(&hosts2, "ghost").phase, STRAGGLERS);
    let (r1, _) = h.snapshot(&id).await;
    assert!(
        !r1.runtime.stragglers_open,
        "promotion of r2 closes r1's stage"
    );
}

/// One `web` phase under `supersede: {policy}`, held for approval when `gated`.
fn superseding(policy: &str, gated: bool) -> String {
    let approval = if gated { ", approval: required" } else { "" };
    format!(
        "  supersede: {policy}\n  health: {{ bake: 30s }}\n  phases:\n    - {{ name: web, select: 'true'{approval} }}\n"
    )
}

async fn approve_web(h: &mut Harness, rollout: &str) {
    h.command(Command::Approve {
        rollout: rollout.to_owned(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn supersede_latest_skips_queued_revisions_for_the_newest() {
    // `queue` is the older name for `latest`.
    for policy in ["latest", "queue"] {
        let mut h = Harness::new().await;
        h.revision("r1", &superseding(policy, true)).await;
        h.revision("r2", &superseding(policy, false)).await;
        h.revision("r3", &superseding(policy, false)).await;
        h.enrol("a", "web").await;
        h.connect("a", None);
        let first = h.start("r1").await;
        h.wait_state(&first, RolloutState::AwaitingApproval).await;
        let second = h.start("r2").await;
        let third = h.start("r3").await;
        let (r2, _) = h.wait_state(&second, RolloutState::Skipped).await;
        assert_eq!(
            r2.runtime.superseded_by.as_ref(),
            Some(&third),
            "{policy}: names the rollout that superseded it"
        );
        assert_eq!(h.snapshot(&third).await.0.state, RolloutState::Detected);
        approve_web(&mut h, &first).await;
        h.wait_state(&first, RolloutState::Completed).await;
        h.wait_state(&third, RolloutState::Completed).await;
        assert_eq!(h.snapshot(&second).await.0.state, RolloutState::Skipped);
    }
}

#[tokio::test(start_paused = true)]
async fn supersede_sequential_rolls_out_every_queued_revision_in_order() {
    let mut h = Harness::new().await;
    for r in ["r1", "r2", "r3"] {
        h.revision(r, &superseding("sequential", true)).await;
    }
    h.enrol("a", "web").await;
    h.connect("a", None);
    let ids = [
        h.start("r1").await,
        h.start("r2").await,
        h.start("r3").await,
    ];
    for (i, id) in ids.iter().enumerate() {
        h.wait_state(id, RolloutState::AwaitingApproval).await;
        for later in &ids[i + 1..] {
            assert_eq!(
                h.snapshot(later).await.0.state,
                RolloutState::Detected,
                "a later revision waits its turn"
            );
        }
        approve_web(&mut h, id).await;
        h.wait_state(id, RolloutState::Completed).await;
    }
}

#[tokio::test(start_paused = true)]
async fn supersede_replace_aborts_the_current_rollout() {
    let mut h = Harness::new().await;
    let gated = "  supersede: replace\n  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n";
    h.revision("r1", gated).await;
    h.revision("r2", &gated.replace(", approval: required", ""))
        .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let first = h.start("r1").await;
    h.wait_state(&first, RolloutState::AwaitingApproval).await;
    let second = h.start("r2").await;
    let (r1, _) = h.wait_state(&first, RolloutState::Aborted).await;
    assert!(r1.runtime.end_reason.unwrap().contains(&second));
    h.wait_state(&second, RolloutState::Completed).await;
}

#[tokio::test(start_paused = true)]
async fn a_local_rollback_fails_the_host_and_pauses() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 5m }\n  phases:\n    - { name: web, select: 'true', max_failures: 5 }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.local_rollback_on_bake = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    assert_eq!(r.runtime.pause.unwrap().kind, "local_rollback");
    assert_eq!(host(&hosts, "a").state, HostPhase::Failed);
    assert!(!host(&hosts, "a").runtime.needs_attention);
}

#[tokio::test(start_paused = true)]
async fn a_module_rollback_during_bake_fails_the_host_and_pauses() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 5m }\n  phases:\n    - { name: web, select: 'true', max_failures: 5 }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| {
        s.local_rollback_on_bake = true;
        s.rollback_modules = vec!["web".into()];
    });
    h.connect("a", None);
    let id = h.start("r1").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    assert_eq!(r.runtime.pause.unwrap().kind, "local_rollback");
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Failed);
    assert_eq!(a.runtime.failed_module.as_deref(), Some("web"));
    assert!(
        a.runtime
            .error
            .as_deref()
            .unwrap()
            .contains("module(s) web")
    );
    assert_eq!(a.result.as_ref().unwrap()["kind"], "module_rollback");
}

#[tokio::test(start_paused = true)]
async fn a_failed_local_rollback_needs_attention_with_both_results() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 5m }\n  phases:\n    - { name: web, select: 'true', max_failures: 5 }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| {
        s.local_rollback_on_bake = true;
        s.local_rollback_fails = true;
    });
    h.connect("a", None);
    let id = h.start("r1").await;
    let (_, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Failed);
    assert!(a.runtime.needs_attention);
    let error = a.runtime.error.as_deref().unwrap();
    assert!(error.contains("needs attention"), "{error}");
    assert!(error.contains("health failing"), "the failure: {error}");
    assert!(
        error.contains("apt is locked"),
        "the rollback's own result: {error}"
    );
    let result = a.result.as_ref().expect("both results are stored");
    assert_eq!(result["succeeded"], false);
    assert_eq!(result["rollback_error"], "apt is locked");
    let (r, hs) = h.snapshot(&id).await;
    let approvals = vec![];
    assert!(super::view::host(&r, host(&hs, "a"), &approvals).needs_attention);
}

#[tokio::test(start_paused = true)]
async fn a_restart_mid_rollout_resumes_from_the_store() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 5m }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.script("b", |s| s.silent_apply = true);
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait(&id, "b applying", |_, hs| {
        host(hs, "b").state == HostPhase::Applying
    })
    .await;

    h.restart().await;
    h.script("b", |s| s.silent_apply = false);
    h.connect("a", Some("r1"));
    h.connect("b", Some("r1"));
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));
    let plans_a = h
        .sent(|host, m| host == "a" && matches!(m, SMsg::Plan(_)))
        .len();
    assert_eq!(
        plans_a, 1,
        "a finished host is not re-planned after a restart"
    );
    let applies_b = h.applies().iter().filter(|a| a.1 == "b").count();
    assert_eq!(
        applies_b, 1,
        "a host that reconnects on the target revision is not re-applied"
    );
}

#[tokio::test(start_paused = true)]
async fn a_host_that_disconnects_mid_wave_times_out_to_failed() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 5m }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.silent_apply = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "applying", |_, hs| {
        host(hs, "a").state == HostPhase::Applying
    })
    .await;
    let gone = h.clock.now();
    h.disconnect("a");
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Failed);
    assert!(a.runtime.error.as_deref().unwrap().contains("disconnected"));
    assert!(r.runtime.pause.unwrap().at - gone >= chrono::Duration::minutes(10));
}

#[tokio::test(start_paused = true)]
async fn a_plan_that_changes_after_approval_needs_reapproval() {
    let mut h = Harness::new().await;
    h.revision("r1", "  health: { bake: 2m }\n  phases:\n    - { name: web, select: 'true', approval: required }\n").await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::AwaitingApproval).await;
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "a applied", |_, hs| {
        host(hs, "a").state == HostPhase::Baking
    })
    .await;
    // b's host changed underneath us: a fresh plan with another digest.
    h.handle.agent_message(
        "b",
        AMsg::Plan(PlanResult {
            revision: "r1".into(),
            digest: "plan-b".into(),
            changes: 2,
            ..Default::default()
        }),
    );
    let (r, hosts) = h.wait_state(&id, RolloutState::AwaitingApproval).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Healthy);
    let detail = super::view::detail(&r, &hosts, &[], &[]);
    assert!(
        detail.phases[0]
            .hosts
            .iter()
            .any(|x| x.host == "b" && x.needs_approval)
    );
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-b".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
    let b_apply = h.applies().into_iter().find(|a| a.1 == "b").unwrap();
    assert_eq!(b_apply.2.plan_digest, "plan-b");
}

#[tokio::test(start_paused = true)]
async fn new_valid_revisions_start_rollouts_automatically() {
    let mut h = Harness::new().await;
    h.revision("r1", "  health: { bake: 30s }\n").await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    h.handle.revision_added("r1");
    for _ in 0..600 {
        let found = h
            .store
            .call(|db| db.rollouts().latest_for_revision("r1"))
            .await
            .unwrap();
        if let Some(r) = found
            && r.state == RolloutState::Completed
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("no automatic rollout completed");
}

#[tokio::test(start_paused = true)]
async fn every_transition_is_audited() {
    let mut h = Harness::new().await;
    h.revision("r1", "  health: { bake: 30s }\n").await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::Completed).await;
    let (entries, _) = h
        .store
        .call(|db| db.audit().list(&Default::default()))
        .await
        .unwrap();
    let actions: Vec<&str> = entries.iter().map(|e| e.action.as_str()).collect();
    for expected in [
        "rollout.planning",
        "rollout.host.healthy",
        "rollout.completed",
        "revision.promoted",
    ] {
        assert!(
            actions.contains(&expected),
            "missing {expected} in {actions:?}"
        );
    }
    // Progress transitions are live events only (security lane: audit noise).
    for progress in buckle_api::PROGRESS_ACTIONS {
        assert!(
            !actions.contains(progress),
            "{progress} in the audit log: {actions:?}"
        );
    }
    assert!(!actions.contains(&"rollout.baking"), "{actions:?}");
}

#[tokio::test(start_paused = true)]
async fn an_agent_reported_stale_plan_needs_reapproval_for_that_host() {
    let mut h = Harness::new().await;
    h.revision("r1", "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n").await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.stale_plan_result = Some("plan-fresh".into()));
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "planned", |r, _| {
        r.state == RolloutState::AwaitingApproval
    })
    .await;
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    let (r, hosts) = h
        .wait(&id, "re-approval", |r, hs| {
            r.state == RolloutState::AwaitingApproval
                && host(hs, "a").plan_digest.as_deref() == Some("plan-fresh")
        })
        .await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Planned, "a stale plan is not a failure");
    assert!(
        super::view::detail(&r, &hosts, &[], &[])
            .plans
            .iter()
            .any(|g| g.digest == "plan-fresh")
    );
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-fresh".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
    let applies = h.applies();
    assert_eq!(applies.len(), 2);
    assert_eq!(applies[1].2.plan_digest, "plan-fresh");
    assert!(
        applies.iter().all(|a| !a.2.plan_digest.is_empty()),
        "Apply always carries a digest"
    );
}

#[tokio::test(start_paused = true)]
async fn a_plan_that_is_stale_on_every_apply_holds_the_host() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.stale_plan_always = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    let (r, hosts) = h
        .wait(&id, "held", |_, hs| host(hs, "a").state == HostPhase::Held)
        .await;
    let a = host(&hosts, "a");
    assert_eq!(a.wave, Some(1), "a refused apply does not start a new wave");
    assert_eq!(
        r.runtime.phases[0].started, 0,
        "the wave slot is given back"
    );
    let error = a.runtime.error.clone().unwrap_or_default();
    assert!(error.contains("stale plan"), "{error}");
    assert!(
        error.contains(a.plan_digest.as_deref().unwrap_or_default()),
        "names the fresh digest: {error}"
    );
    assert_eq!(
        h.applies().len(),
        super::controller::MAX_STALE_REFUSALS as usize
    );
    assert_ne!(r.state, RolloutState::Completed);
}

/// A passing baseline answer from `host` to its last baseline request.
fn answer_baseline(h: &Harness, host: &str) {
    let request_id = h
        .sent(|x, m| {
            x == host && matches!(m, SMsg::Health(r) if r.scope == HealthScope::Baseline as i32)
        })
        .into_iter()
        .filter_map(|(_, _, m)| match m {
            SMsg::Health(r) => Some(r.request_id),
            _ => None,
        })
        .next_back()
        .expect("a baseline request");
    h.handle.agent_message(
        host,
        AMsg::Health(HealthReport {
            target: String::new(),
            workload: String::new(),
            scope: HealthScope::Baseline as i32,
            request_id,
            revision: String::new(),
            results: vec![pb::HealthResult {
                module: "web".into(),
                check: "http".into(),
                kind: "http".into(),
                required: true,
                passed: true,
                ..Default::default()
            }],
        }),
    );
}

const ONE_PHASE_TWO_WIDE: &str = "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', parallel: 2, max_failures: 0 }\n";

#[tokio::test(start_paused = true)]
async fn a_baseline_answer_while_paused_does_not_apply() {
    let mut h = Harness::new().await;
    h.revision("r1", ONE_PHASE_TWO_WIDE).await;
    for x in ["a", "b"] {
        h.enrol(x, "web").await;
        h.connect(x, None);
    }
    h.script("a", |s| s.silent_baseline = true);
    h.script("b", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r1").await;
    h.wait(&id, "a in baseline and paused", |r, hs| {
        r.state == RolloutState::Paused && host(hs, "a").state == HostPhase::Baseline
    })
    .await;
    answer_baseline(&h, "a");
    h.idle(60).await;
    assert!(
        h.applies().iter().all(|a| a.1 != "a"),
        "a paused rollout applies nothing"
    );
    let (_, hosts) = h.snapshot(&id).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Planned);

    // Resuming starts it again from its baseline.
    h.script("a", |s| s.silent_baseline = false);
    h.command(Command::Resume {
        rollout: id.clone(),
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "a applied", |_, hs| {
        matches!(host(hs, "a").state, HostPhase::Baking | HostPhase::Healthy)
    })
    .await;
    assert_eq!(h.applies().iter().filter(|a| a.1 == "a").count(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_late_baseline_answer_during_a_rollback_does_not_apply() {
    let mut h = Harness::new().await;
    h.revision("r1", ONE_PHASE_TWO_WIDE).await;
    h.revision("r2", ONE_PHASE_TWO_WIDE).await;
    for x in ["a", "b"] {
        h.enrol(x, "web").await;
        h.connect(x, None);
    }
    let first = h.start("r1").await;
    h.wait_state(&first, RolloutState::Completed).await;
    h.script("a", |s| s.silent_baseline = true);
    h.script("b", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r2").await;
    h.wait(&id, "a in baseline and paused", |r, hs| {
        r.state == RolloutState::Paused && host(hs, "a").state == HostPhase::Baseline
    })
    .await;
    h.command(Command::Rollback {
        rollout: id.clone(),
        note: String::new(),
    })
    .await
    .unwrap();
    answer_baseline(&h, "a");
    h.idle(120).await;
    assert!(
        h.applies()
            .iter()
            .all(|a| !(a.1 == "a" && a.2.revision == "r2")),
        "the new revision is never applied to a host the rollback skips"
    );
}

#[tokio::test(start_paused = true)]
async fn an_applying_host_that_reconnects_off_target_gets_the_same_apply_again() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.silent_apply = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait_apply(&id, "a").await;

    // The agent skipped the apply (its session died first) and reconnects
    // on the old revision with nothing in flight: the same Apply again.
    h.disconnect("a");
    h.idle(5).await;
    h.connect("a", None);
    // Wait on the outcome rather than a fixed idle: on the paused clock,
    // virtual time can jump ahead while the controller's store calls run on
    // blocking threads.
    for _ in 0..600 {
        if h.applies().len() >= 2 {
            break;
        }
        h.idle(1).await;
    }
    let applies = h.applies();
    assert_eq!(applies.len(), 2);
    assert_eq!(applies[0].2.plan_digest, applies[1].2.plan_digest);
    assert_eq!(applies[0].2.approval_token, applies[1].2.approval_token);
    assert!(!applies[1].2.approval_token.is_empty());

    // A reconnect that announces the apply as running waits for its result.
    h.disconnect("a");
    h.idle(5).await;
    h.attach("a");
    h.connect_with("a", None, vec!["r1".into()]);
    h.idle(5).await;
    assert_eq!(h.applies().len(), 2);

    // The result arrives: the host bakes.
    h.script("a", |s| s.silent_apply = false);
    h.disconnect("a");
    h.idle(5).await;
    h.connect("a", None);
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Healthy);
}

#[tokio::test(start_paused = true)]
async fn an_applying_host_that_never_reports_fails_after_the_apply_timeout() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.silent_apply = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(&id, "applying", |_, hs| {
            host(hs, "a").state == HostPhase::Applying
        })
        .await;
    let sent = host(&hosts, "a").runtime.apply_started_at.unwrap();
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Failed);
    assert!(
        a.runtime
            .error
            .as_deref()
            .unwrap()
            .contains("no apply result"),
        "{:?}",
        a.runtime.error
    );
    assert!(r.runtime.pause.unwrap().at - sent >= chrono::Duration::minutes(30));
}

#[tokio::test(start_paused = true)]
async fn approving_pausing_and_relabelling_reach_a_connected_agent() {
    let mut h = Harness::new().await;
    h.revision("r1", "  phases:\n    - { name: web, select: 'true' }\n")
        .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    h.store
        .call(|db| {
            db.hosts().set_status("a", HostStatus::Paused)?;
            let mut labels = BTreeMap::new();
            labels.insert("tier".to_string(), "db".to_string());
            db.hosts().set_labels("a", &labels)?;
            Ok(())
        })
        .await
        .unwrap();
    h.handle.host_changed("a");
    h.idle(1).await;
    let updates: Vec<pb::HostUpdate> = h
        .sent(|host, m| host == "a" && matches!(m, SMsg::HostUpdate(_)))
        .into_iter()
        .filter_map(|(_, _, m)| match m {
            SMsg::HostUpdate(u) => Some(u),
            _ => None,
        })
        .collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].status, "paused");
    assert_eq!(
        updates[0].labels.get("tier").map(String::as_str),
        Some("db")
    );

    h.store
        .call(|db| db.hosts().set_status("a", HostStatus::Active).map(|_| ()))
        .await
        .unwrap();
    h.handle.host_changed("a");
    h.idle(1).await;
    let last = h
        .sent(|host, m| host == "a" && matches!(m, SMsg::HostUpdate(_)))
        .pop()
        .unwrap();
    assert!(matches!(last.2, SMsg::HostUpdate(u) if u.status == "active"));
}

#[tokio::test(start_paused = true)]
async fn blocked_steps_fail_the_host_as_needing_attention_without_a_rollback() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', max_failures: 0 }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.apply_blocked = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Failed);
    assert!(a.runtime.needs_attention);
    assert!(
        a.runtime
            .error
            .as_deref()
            .unwrap()
            .starts_with("needs attention"),
        "{:?}",
        a.runtime.error
    );
    assert_eq!(r.runtime.pause.unwrap().kind, "failure_budget");
    assert!(
        h.sent(|_, m| matches!(m, SMsg::Rollback(_))).is_empty(),
        "nothing rolled back"
    );
}

// ------------------------------------------------------------ review fixes

/// After a restart nobody is connected yet: hosts still to plan or apply
/// must not be taken for offline (missed) while their agents reconnect, or
/// the phase completes and the revision is promoted unapproved.
#[tokio::test(start_paused = true)]
async fn a_restart_while_awaiting_approval_keeps_awaiting_it() {
    let mut h = Harness::new().await;
    h.revision("r1", TWO_PHASES).await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.connect("c1", None);
    h.connect("w1", None);
    let id = h.start("r1").await;
    h.wait(&id, "awaiting approval with plans", |r, hs| {
        r.state == RolloutState::AwaitingApproval && hs.iter().all(|x| x.plan_digest.is_some())
    })
    .await;

    h.restart().await;
    // Inside the disconnect timeout nothing is missed or promoted.
    h.idle(120).await;
    let (r, hosts) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::AwaitingApproval, "{:?}", r.runtime);
    assert!(
        hosts.iter().all(|x| x.state == HostPhase::Planned),
        "{:?}",
        hosts.iter().map(|x| (&x.host, x.state)).collect::<Vec<_>>()
    );
    assert!(h.applies().is_empty());
    let stable = h
        .store
        .call(|db| db.kv().get("stable_revision"))
        .await
        .unwrap();
    assert_eq!(stable, None, "nothing was promoted");

    h.connect("c1", None);
    h.connect("w1", None);
    h.idle(5).await;
    let (r, _) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::AwaitingApproval);
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "canary".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));
}

/// A planned host whose session drops while the rollout waits for approval
/// (or for its plan) is inside the same reconnect grace as after a restart:
/// it is not missed, the phase does not finish under it and its plan stands
/// when the agent comes back.
#[tokio::test(start_paused = true)]
async fn a_disconnect_while_awaiting_approval_keeps_the_host_in_its_phase() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait(&id, "plans in", |r, hs| {
        r.state == RolloutState::AwaitingApproval && hs.iter().all(|x| x.plan_digest.is_some())
    })
    .await;

    h.disconnect("a");
    h.disconnect("b");
    h.idle(120).await;
    let (r, hosts) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::AwaitingApproval, "{:?}", r.runtime);
    assert!(
        hosts
            .iter()
            .all(|x| x.state == HostPhase::Planned && x.plan_digest.is_some()),
        "{:?}",
        hosts
            .iter()
            .map(|x| (&x.host, x.state, &x.plan_digest))
            .collect::<Vec<_>>()
    );
    assert!(h.applies().is_empty());

    h.connect("a", None);
    h.connect("b", None);
    h.idle(5).await;
    let (r, _) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::AwaitingApproval);
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));
}

/// A planned host that stays away past the disconnect timeout misses its
/// phase, as it would after a restart, and the phase finishes without it.
#[tokio::test(start_paused = true)]
async fn a_host_that_stays_disconnected_misses_its_phase() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait(&id, "plans in", |r, hs| {
        r.state == RolloutState::AwaitingApproval && hs.iter().all(|x| x.plan_digest.is_some())
    })
    .await;
    h.disconnect("b");
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "a healthy", |_, hs| {
        host(hs, "a").state == HostPhase::Healthy
    })
    .await;
    let (r, hosts) = h.snapshot(&id).await;
    assert!(r.is_running(), "b's grace holds the phase open");
    assert_eq!(host(&hosts, "b").state, HostPhase::Planned);
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "b").phase, STRAGGLERS, "b missed its phase");
    assert_eq!(host(&hosts, "b").state, HostPhase::PlanPending);
}

/// A host that does not come back within the disconnect timeout after a
/// restart misses its phase as an offline host would.
#[tokio::test(start_paused = true)]
async fn a_host_that_stays_away_after_a_restart_misses_its_phase() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait(&id, "plans in", |r, hs| {
        r.state == RolloutState::AwaitingApproval && hs.iter().all(|x| x.plan_digest.is_some())
    })
    .await;
    h.restart().await;
    h.connect("a", None);
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "a healthy", |_, hs| {
        host(hs, "a").state == HostPhase::Healthy
    })
    .await;
    let (r, hosts) = h.snapshot(&id).await;
    assert!(r.is_running(), "b's grace holds the phase open");
    assert_eq!(host(&hosts, "b").state, HostPhase::Planned);
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "b").phase, STRAGGLERS, "b missed its phase");
}

/// A dropped session leaves the link before its disconnect reaches the
/// controller. A pass over the rollout in between must not take the host for
/// offline: it keeps its phase, and the disconnect starts its grace.
#[tokio::test(start_paused = true)]
async fn a_dropped_session_not_yet_reported_does_not_miss_its_phase() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait(&id, "plans in", |r, hs| {
        r.state == RolloutState::AwaitingApproval && hs.iter().all(|x| x.plan_digest.is_some())
    })
    .await;

    // b's session is gone from the link; its disconnect is still on its way.
    let session = h.session_ids.remove("b").expect("b has a session");
    h.sessions.detach("b", session);
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "a healthy", |_, hs| {
        host(hs, "a").state == HostPhase::Healthy
    })
    .await;
    let (r, hosts) = h.snapshot(&id).await;
    assert!(r.is_running(), "{:?}", r.state);
    assert_eq!(
        (host(&hosts, "b").phase.as_str(), host(&hosts, "b").state),
        ("web", HostPhase::Planned)
    );

    h.handle.disconnected("b");
    h.idle(30).await;
    assert!(
        h.snapshot(&id).await.0.is_running(),
        "b is inside its grace"
    );
    h.connect("b", None);
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(
        (host(&hosts, "b").phase.as_str(), host(&hosts, "b").state),
        ("web", HostPhase::Healthy),
        "b finished in its own phase"
    );
}

/// A phase that requires approval does not complete because every one of
/// its hosts missed it (offline when the rollout started): nothing would
/// have been approved and the next phase would start on its own. It waits
/// until its host comes back and is approved, or an operator skips it.
#[rstest::rstest]
#[case::reconnect_and_approve("approve")]
#[case::skip("skip")]
#[tokio::test(start_paused = true)]
async fn a_required_phase_whose_hosts_are_all_offline_waits(#[case] decision: &str) {
    let mut h = Harness::new().await;
    h.revision("r1", TWO_PHASES).await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.connect("w1", None);
    let id = h.start("r1").await;
    h.idle(120).await;
    let (r, hosts) = h.snapshot(&id).await;
    assert!(r.is_running(), "{:?}", r.state);
    assert_eq!(r.runtime.current_phase, 0, "the canary phase holds");
    assert_eq!(host(&hosts, "c1").phase, "canary");
    assert!(h.applies().is_empty(), "nothing applied unapproved");
    assert_eq!(
        h.store
            .call(|db| db.kv().get("stable_revision"))
            .await
            .unwrap(),
        None,
        "nothing was promoted"
    );

    match decision {
        "approve" => {
            h.connect("c1", None);
            h.wait_state(&id, RolloutState::AwaitingApproval).await;
            h.command(Command::Approve {
                rollout: id.clone(),
                phase: "canary".into(),
                digests: vec!["plan-a".into()],
                note: String::new(),
            })
            .await
            .unwrap();
        }
        _ => {
            h.command(Command::Skip {
                rollout: id.clone(),
                host: "c1".into(),
                workload: String::new(),
                note: String::new(),
            })
            .await
            .unwrap();
        }
    }
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "w1").state, HostPhase::Healthy);
}

/// Retrying one of two failed hosts lets that host proceed, but the phase
/// stays over its budget (max_failures 0) while the other failure stands: no
/// completion, no promotion, until that one is resolved too.
#[tokio::test(start_paused = true)]
async fn a_phase_over_budget_does_not_complete_while_a_failure_stands() {
    let mut h = Harness::new().await;
    h.revision("r1", ONE_PHASE_TWO_WIDE).await;
    for x in ["a", "b"] {
        h.enrol(x, "web").await;
        h.connect(x, None);
        h.script(x, |s| s.apply_fail = Some(vec![]));
    }
    let id = h.start("r1").await;
    h.wait(&id, "both failed and paused", |r, hs| {
        r.state == RolloutState::Paused && hs.iter().all(|x| x.state == HostPhase::Failed)
    })
    .await;
    h.script("a", |s| s.apply_fail = None);
    h.command(Command::Retry {
        rollout: id.clone(),
        host: "a".into(),
        workload: String::new(),
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "a healthy", |_, hs| {
        host(hs, "a").state == HostPhase::Healthy
    })
    .await;
    h.idle(120).await;
    let (r, hosts) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::Paused, "b's failure still stands");
    assert_eq!(r.runtime.pause.as_ref().unwrap().kind, "failure_budget");
    assert_eq!(host(&hosts, "b").state, HostPhase::Failed);
    let stable = h
        .store
        .call(|db| db.kv().get("stable_revision"))
        .await
        .unwrap();
    assert_eq!(stable, None);

    // Skipping b clears the breach; the rollout resumes and completes.
    h.command(Command::Skip {
        rollout: id.clone(),
        host: "b".into(),
        workload: String::new(),
        note: "decommissioning".into(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "b").state, HostPhase::Skipped);
    assert!(
        host(&hosts, "b")
            .runtime
            .error
            .as_deref()
            .unwrap()
            .contains("decommissioning")
    );
}

/// An operator's pause is never lifted by a host decision: skip and retry
/// act on the host but the rollout stays paused until resumed.
#[tokio::test(start_paused = true)]
async fn host_decisions_do_not_override_an_operator_pause() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', parallel: 1, max_failures: 1 }\n",
    )
    .await;
    for x in ["a", "b", "c"] {
        h.enrol(x, "web").await;
        h.connect(x, None);
    }
    h.script("a", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r1").await;
    h.wait(&id, "a failed within budget", |_, hs| {
        host(hs, "a").state == HostPhase::Failed
    })
    .await;
    h.command(Command::Pause {
        rollout: id.clone(),
        note: String::new(),
    })
    .await
    .unwrap();
    let applies = h.applies().len();
    h.script("a", |s| s.apply_fail = None);
    h.command(Command::Retry {
        rollout: id.clone(),
        host: "a".into(),
        workload: String::new(),
        note: String::new(),
    })
    .await
    .unwrap();
    h.idle(300).await;
    let (r, hosts) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::Paused);
    assert_eq!(r.runtime.pause.as_ref().unwrap().kind, "operator");
    assert_eq!(host(&hosts, "a").state, HostPhase::Planned);
    assert_eq!(
        h.applies().len(),
        applies,
        "nothing applies under an operator pause"
    );
    h.command(Command::Resume {
        rollout: id.clone(),
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));
}

/// A failed pre-phase gate fails the host with the gate's checks, so the
/// operator's host decisions apply to it; skipping it lets the gate pass on
/// the remaining hosts instead of re-running on the failing one forever.
#[rstest::rstest]
#[case::skip("skip")]
#[case::continue_waiving("continue")]
#[case::resume("resume")]
#[tokio::test(start_paused = true)]
async fn a_failed_gate_fails_the_host_and_host_decisions_clear_it(#[case] decision: &str) {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        TWO_PHASES.replace(", approval: required", "").as_str(),
    )
    .await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.script("c1", |s| s.gate_fail = true);
    h.connect("c1", None);
    h.connect("w1", None);
    let id = h.start("r1").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    assert_eq!(r.runtime.pause.as_ref().unwrap().kind, "gate");
    let c1 = host(&hosts, "c1");
    assert_eq!(c1.state, HostPhase::Failed);
    assert_eq!(c1.runtime.failed_checks, vec!["web/http"]);
    assert!(
        c1.runtime
            .error
            .as_deref()
            .unwrap()
            .contains("pre-phase gate")
    );
    let rollout = id.clone();
    let note = String::new();
    let command = match decision {
        "skip" => Command::Skip {
            rollout,
            host: "c1".into(),
            workload: String::new(),
            note,
        },
        "continue" => Command::Continue {
            rollout,
            host: "c1".into(),
            workload: String::new(),
            waive: vec!["web/http".into()],
            note,
        },
        _ => Command::Resume { rollout, note },
    };
    h.command(command).await.unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "w1").state, HostPhase::Healthy);
    let expected = match decision {
        "skip" => HostPhase::Skipped,
        "continue" => HostPhase::Healthy,
        _ => HostPhase::Failed,
    };
    assert_eq!(host(&hosts, "c1").state, expected);
}

/// A pre-phase gate pending across a restart is re-armed: the check is
/// re-sent when the host reconnects, and a host that never comes back fails
/// the gate after the disconnect timeout instead of stalling it for ever.
#[rstest::rstest]
#[case::reconnects(true)]
#[case::stays_away(false)]
#[tokio::test(start_paused = true)]
async fn a_restart_with_a_gate_pending_re_arms_it(#[case] reconnects: bool) {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        TWO_PHASES.replace(", approval: required", "").as_str(),
    )
    .await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.script("c1", |s| s.silent_gate = true);
    h.connect("c1", None);
    h.connect("w1", None);
    let id = h.start("r1").await;
    h.wait(&id, "gate pending", |r, _| {
        r.runtime.phases[0]
            .gate
            .as_ref()
            .is_some_and(|g| g.pending.contains_key("c1"))
    })
    .await;
    h.restart().await;
    h.script("c1", |s| s.silent_gate = false);
    h.connect("w1", None);
    if reconnects {
        h.connect("c1", Some("r1"));
        let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
        assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));
    } else {
        let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
        assert_eq!(r.runtime.pause.as_ref().unwrap().kind, "gate");
        let c1 = host(&hosts, "c1");
        assert_eq!(c1.state, HostPhase::Failed);
        assert!(
            c1.runtime
                .error
                .as_deref()
                .unwrap()
                .contains("disconnected")
        );
        assert!(h.applies().iter().all(|a| a.1 == "c1"), "web never started");
    }
}

impl Harness {
    /// Makes every checkpoint that writes an audit row fail (or stops it).
    async fn fail_checkpoints(&self, fail: bool) {
        self.store
            .call(move |db| {
                let sql = if fail {
                    "CREATE TRIGGER fail_audit BEFORE INSERT ON audit_log BEGIN SELECT RAISE(ABORT, 'injected failure'); END"
                } else {
                    "DROP TRIGGER fail_audit"
                };
                db.conn()
                    .execute_batch(sql)
                    .map_err(|e| buckle_core::error::system(e.to_string(), &[]))
            })
            .await
            .unwrap();
    }

    async fn audit_actions(&self, id: &str) -> Vec<String> {
        let id = id.to_owned();
        self.store
            .call(move |db| db.audit().for_subject(&id, 1000))
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.action)
            .collect()
    }
}

/// A checkpoint that cannot be written loses nothing and sends nothing ahead
/// of its state: the audit rows and the queued messages are kept, a finished
/// rollout stays live until its final state is written, and the controller
/// retries on its own.
#[tokio::test(start_paused = true)]
async fn a_failed_checkpoint_keeps_its_audit_outbox_and_finished_rollout() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 1m }\n  phases:\n    - { name: web, select: 'true', approval: required }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::AwaitingApproval).await;

    h.fail_checkpoints(true).await;
    let refused = h
        .command(Command::Approve {
            rollout: id.clone(),
            phase: "web".into(),
            digests: vec!["plan-a".into()],
            note: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.kind, CommandErrorKind::Internal, "{refused}");
    h.idle(30).await;
    assert!(
        h.sent(|_, m| matches!(m, SMsg::Health(_) | SMsg::Apply(_)))
            .is_empty(),
        "nothing is sent before its state is saved"
    );
    h.fail_checkpoints(false).await;
    h.wait(&id, "baking", |_, hs| {
        host(hs, "a").state == HostPhase::Baking
    })
    .await;
    assert!(
        h.audit_actions(&id)
            .await
            .contains(&"rollout.approve".to_owned())
    );

    // The bake ends (and the rollout completes) while checkpoints fail.
    h.fail_checkpoints(true).await;
    h.idle(120).await;
    let (r, _) = h.snapshot(&id).await;
    assert_ne!(r.state, RolloutState::Completed, "not written yet");
    h.fail_checkpoints(false).await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(r.finished_at.is_some());
    assert_eq!(host(&hosts, "a").state, HostPhase::Healthy);
    let actions = h.audit_actions(&id).await;
    for a in [
        "rollout.host.healthy",
        "rollout.completed",
        "revision.promoted",
    ] {
        assert!(actions.contains(&a.to_owned()), "{a} missing: {actions:?}");
    }
    let stable = h
        .store
        .call(|db| db.kv().get("stable_revision"))
        .await
        .unwrap();
    assert_eq!(stable.as_deref(), Some("r1"));
}

/// A connected host that never sends a bake report does not bake healthy:
/// it fails once a grace after its window has passed.
#[tokio::test(start_paused = true)]
async fn a_bake_without_a_report_does_not_complete_healthy() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  settle: 2m\n  health: { bake: 1m }\n  phases:\n    - { name: web, select: 'true', max_failures: 0 }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.silent_bake = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "baking", |_, hs| {
        host(hs, "a").state == HostPhase::Baking
    })
    .await;
    h.idle(90).await;
    let (_, hosts) = h.snapshot(&id).await;
    assert_eq!(
        host(&hosts, "a").state,
        HostPhase::Baking,
        "no report, not baked"
    );
    let (_, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Failed);
    assert!(
        a.runtime
            .error
            .as_deref()
            .unwrap()
            .contains("no health report")
    );
}

/// A host away when its bake window closes is not taken as baked: when it
/// comes back it is asked again, and its fresh report completes it.
#[tokio::test(start_paused = true)]
async fn a_host_away_at_the_end_of_its_bake_reports_afresh_before_it_counts() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 1m }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "baking with a report", |_, hs| {
        let a = host(hs, "a");
        a.state == HostPhase::Baking && a.runtime.bake_reports > 0
    })
    .await;
    h.disconnect("a");
    h.idle(180).await;
    let (_, hosts) = h.snapshot(&id).await;
    assert_eq!(
        host(&hosts, "a").state,
        HostPhase::Baking,
        "away, not baked"
    );
    let bakes_before = h
        .sent(|x, m| {
            x == "a" && matches!(m, SMsg::Health(r) if r.scope == HealthScope::Bake as i32)
        })
        .len();
    h.connect("a", Some("r1"));
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Healthy);
    let bakes_after = h
        .sent(|x, m| {
            x == "a" && matches!(m, SMsg::Health(r) if r.scope == HealthScope::Bake as i32)
        })
        .len();
    assert_eq!(bakes_after, bakes_before + 1, "asked afresh on reconnect");
}

/// A required check still failing when the bake window closes is followed
/// through settle: the agent samples only until the `until` it was asked for,
/// so it is asked again to cover settle. A recovery after the window (a job
/// whose replacement allocation starts late) is seen and the row is healthy;
/// a check that never recovers fails past settle. The same for a host and a
/// cluster target (whatever its kind, its executor answers the request).
#[rstest::rstest]
#[case::host_recovers("a", true)]
#[case::host_stays_failing("a", false)]
#[case::cluster_recovers(CLUSTER, true)]
#[case::cluster_stays_failing(CLUSTER, false)]
#[tokio::test(start_paused = true)]
async fn a_check_failing_at_the_end_of_the_bake_is_sampled_through_settle(
    #[case] row: &str,
    #[case] recovers: bool,
) {
    let mut h = Harness::new().await;
    let policy = "  settle: 2m\n  health: { bake: 1m }\n  phases:\n    - { name: all, select: 'true', parallel: 4 }\n";
    let (agent, target) = if row == CLUSTER {
        h.revision_with("r1", policy, NOMAD_CLUSTER, FLAT).await;
        h.enrol("n1", "nomad").await;
        ("n1", CLUSTER)
    } else {
        h.revision("r1", policy).await;
        h.enrol(row, "web").await;
        (row, "")
    };
    let fail = |s: &mut Script, on: bool| {
        if target.is_empty() {
            s.bake_fail = on
        } else {
            s.target_bake_fail = on
        }
    };
    h.script(agent, |s| fail(s, true));
    h.connect(agent, None);
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(&id, "failing in its bake", |_, hs| {
            let r = host(hs, row);
            r.state == HostPhase::Baking && !r.runtime.failing_since.is_empty()
        })
        .await;
    let since = *host(&hosts, row)
        .runtime
        .failing_since
        .values()
        .next()
        .unwrap();
    let window_end = host(&hosts, row).runtime.bake_until.unwrap();
    h.script(agent, |s| fail(s, !recovers));

    let (_, hosts) = h
        .wait(&id, "past the window", |_, hs| {
            host(hs, row).runtime.bake_asked_until > Some(window_end)
        })
        .await;
    assert_eq!(
        host(&hosts, row).runtime.bake_asked_until,
        Some(since + chrono::Duration::minutes(2)),
        "the agent is asked to sample through the failing check's settle"
    );
    let asks = h.sent(|x, m| {
        x == agent
            && matches!(m, SMsg::Health(r) if r.scope == HealthScope::Bake as i32 && r.target == target)
    });
    assert_eq!(
        asks.len(),
        2,
        "the window's request, then settle's: {asks:?}"
    );

    if recovers {
        let (_, hosts) = h
            .wait(&id, "healthy", |_, hs| {
                host(hs, row).state == HostPhase::Healthy
            })
            .await;
        assert_eq!(host(&hosts, row).runtime.flaps, vec!["web/http"]);
    } else {
        let (_, hosts) = h
            .wait(&id, "failed", |_, hs| {
                host(hs, row).state == HostPhase::Failed
            })
            .await;
        let r = host(&hosts, row);
        assert!(
            r.runtime
                .error
                .as_deref()
                .unwrap()
                .contains("bake regression")
        );
    }
}

/// A host the previous rollout's stragglers stage is still applying is not
/// started by the next rollout's phase until that apply is done.
#[tokio::test(start_paused = true)]
async fn a_phase_waits_for_a_host_busy_in_another_rollout() {
    let mut h = Harness::new().await;
    let policy = "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n";
    h.revision("r1", policy).await;
    h.revision("r2", policy).await;
    h.enrol("a", "web").await;
    h.enrol("late", "web").await;
    h.connect("a", None);
    let first = h.start("r1").await;
    h.wait_state(&first, RolloutState::Completed).await;
    h.script("late", |s| s.silent_apply = true);
    h.connect("late", None);
    h.wait(&first, "late applying in the stragglers stage", |_, hs| {
        host(hs, "late").state == HostPhase::Applying
    })
    .await;
    let second = h.start("r2").await;
    h.idle(60).await;
    assert!(
        h.applies()
            .iter()
            .all(|x| !(x.1 == "late" && x.2.revision == "r2")),
        "r2 does not apply to a host r1 is applying"
    );
    let (_, hosts) = h.snapshot(&second).await;
    assert_eq!(host(&hosts, "late").state, HostPhase::Planned);
    // r1's apply finishes; then r2 takes the host.
    let apply = h.applies().into_iter().find(|x| x.1 == "late").unwrap().2;
    h.script("late", |s| s.silent_apply = false);
    h.handle.agent_message(
        "late",
        AMsg::Result(ApplyResult {
            revision: apply.revision.clone(),
            plan_digest: apply.plan_digest.clone(),
            succeeded: true,
            applied_revision: apply.revision,
            ..Default::default()
        }),
    );
    let (_, hosts) = h.wait_state(&second, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "late").state, HostPhase::Healthy);
    assert_eq!(host(&hosts, "late").phase, "web");
}

/// A bake report naming another revision or a request this bake never sent
/// (e.g. answering another rollout) does not count for the host's bake.
#[tokio::test(start_paused = true)]
async fn a_bake_report_for_another_revision_or_request_is_not_this_bake() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  settle: 30s\n  health: { bake: 2m }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "baking", |_, hs| {
        let a = host(hs, "a");
        a.state == HostPhase::Baking && a.runtime.bake_reports > 0
    })
    .await;
    let failing = |revision: &str, request: &str| {
        AMsg::Health(HealthReport {
            target: String::new(),
            workload: String::new(),
            scope: HealthScope::Bake as i32,
            request_id: request.into(),
            revision: revision.into(),
            results: vec![pb::HealthResult {
                module: "web".into(),
                check: "http".into(),
                required: true,
                passed: false,
                ..Default::default()
            }],
        })
    };
    h.handle.agent_message("a", failing("r0", ""));
    h.handle
        .agent_message("a", failing("r1", "someone-elses-request"));
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Healthy);
    assert!(a.runtime.failing_since.is_empty() && a.runtime.flaps.is_empty());
}

/// A plan that fails for a host of a later phase is shown on the host at
/// once but charged to its phase's budget only when that phase starts: the
/// canary is not paused for it.
#[tokio::test(start_paused = true)]
async fn a_plan_error_in_a_later_phase_is_charged_when_that_phase_starts() {
    let mut h = Harness::new().await;
    h.revision("r1", TWO_PHASES).await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.enrol("w2", "web").await;
    h.script("w1", |s| s.plan_error = Some("template error".into()));
    for x in ["c1", "w1", "w2"] {
        h.connect(x, None);
    }
    let id = h.start("r1").await;
    let (r, hosts) = h
        .wait(&id, "w1 failed planning", |_, hs| {
            host(hs, "w1").state == HostPhase::Failed
        })
        .await;
    assert_ne!(r.state, RolloutState::Paused, "the canary is not paused");
    assert!(
        host(&hosts, "w1")
            .runtime
            .error
            .as_deref()
            .unwrap()
            .contains("template error")
    );
    h.wait_state(&id, RolloutState::AwaitingApproval).await;
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "canary".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    assert_eq!(host(&hosts, "c1").state, HostPhase::Healthy);
    let pause = r.runtime.pause.unwrap();
    assert_eq!(pause.kind, "failure_budget");
    assert!(pause.reason.contains("'web'"), "{}", pause.reason);
    assert!(
        h.applies().iter().all(|a| a.1 == "c1"),
        "nothing starts in a phase over its budget"
    );
    h.script("w1", |s| s.plan_error = None);
    h.command(Command::Retry {
        rollout: id.clone(),
        host: "w1".into(),
        workload: String::new(),
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));
}

/// Forgetting a host mid-rollout skips it (it is neither waited for nor a
/// failure), and when it registers again it is no longer excluded.
#[tokio::test(start_paused = true)]
async fn a_host_forgotten_mid_rollout_is_skipped_and_can_register_again() {
    let mut h = Harness::new().await;
    let policy = "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', parallel: 2, max_failures: 0 }\n";
    h.revision("r1", policy).await;
    h.revision("r2", policy).await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.script("a", |s| s.silent_apply = true);
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait(&id, "a applying", |_, hs| {
        host(hs, "a").state == HostPhase::Applying
    })
    .await;
    h.store
        .call(|db| db.hosts().delete("a").map(|_| ()))
        .await
        .unwrap();
    h.disconnect("a");
    h.handle.host_changed("a");
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Skipped);
    assert!(a.runtime.error.as_deref().unwrap().contains("forgotten"));

    // It registers afresh (the gRPC layer tells the controller).
    h.enrol("a", "web").await;
    h.handle.host_changed("a");
    h.connect("a", Some("r1"));
    let second = h.start("r2").await;
    let (_, hosts) = h.wait_state(&second, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Healthy);
    assert_eq!(host(&hosts, "a").phase, "web");
}

const THREE_PHASES: &str = "  health: { bake: 30s }\n  phases:\n    - { name: canary, select: 'host.groups contains \"canary\"' }\n    - { name: web, select: 'host.groups contains \"web\"' }\n    - { name: db, select: 'host.groups contains \"db\"' }\n";

/// Three hosts converged on r1 (stable), ready for a rollout of r2 whose
/// policy appends `extra` to THREE_PHASES' rollout section.
async fn on_stable_r1(extra: &str) -> Harness {
    let mut h = Harness::new().await;
    h.revision("r1", THREE_PHASES).await;
    h.revision("r2", &format!("{extra}{THREE_PHASES}")).await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.enrol("d1", "db").await;
    for x in ["c1", "w1", "d1"] {
        h.connect(x, Some("r0"));
    }
    let first = h.start("r1").await;
    h.wait_state(&first, RolloutState::Completed).await;
    h
}

/// `on_failure: rollback` rolls the updated hosts back on its own when the
/// budget is breached.
#[tokio::test(start_paused = true)]
async fn on_failure_rollback_rolls_back_without_an_operator() {
    let h = on_stable_r1("  on_failure: rollback\n").await;
    h.script("w1", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r2").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::RolledBack).await;
    assert!(r.runtime.pause.is_none());
    assert_eq!(host(&hosts, "c1").state, HostPhase::RolledBack);
    assert_eq!(
        host(&hosts, "d1").state,
        HostPhase::Planned,
        "the db phase never started"
    );
    assert!(
        h.applies()
            .iter()
            .all(|x| !(x.1 == "d1" && x.2.revision == "r2"))
    );
    let rolled: Vec<String> = h
        .sent(|_, m| matches!(m, SMsg::Rollback(_)))
        .into_iter()
        .map(|x| x.1)
        .collect();
    assert_eq!(rolled, vec!["w1", "c1"]);
}

/// `on_failure: continue` records failures without pausing: the rollout
/// completes with the failed host shown failed.
#[tokio::test(start_paused = true)]
async fn on_failure_continue_completes_past_failures() {
    let h = on_stable_r1("  on_failure: continue\n").await;
    h.script("w1", |s| s.apply_fail = Some(vec![]));
    let id = h.start("r2").await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(r.runtime.pause.is_none());
    assert_eq!(host(&hosts, "w1").state, HostPhase::Failed);
    assert_eq!(host(&hosts, "d1").state, HostPhase::Healthy);
}

/// Abort stops the rollout where it is: nothing else applies and no
/// stragglers stage opens.
#[tokio::test(start_paused = true)]
async fn abort_stops_the_rollout_where_it_is() {
    let h = on_stable_r1("").await;
    h.script("w1", |s| s.silent_apply = true);
    let id = h.start("r2").await;
    h.wait(&id, "w1 applying", |_, hs| {
        host(hs, "w1").state == HostPhase::Applying
    })
    .await;
    h.command(Command::Abort {
        rollout: id.clone(),
        note: "wrong change".into(),
    })
    .await
    .unwrap();
    let (r, _) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::Aborted);
    assert!(r.finished_at.is_some());
    assert!(!r.runtime.stragglers_open);
    assert!(r.runtime.end_reason.unwrap().contains("aborted by alice"));
    h.idle(600).await;
    assert!(
        h.applies()
            .iter()
            .all(|a| a.2.revision != "r2" || a.1 != "d1"),
        "nothing starts after an abort"
    );
    let again = h
        .command(Command::Abort {
            rollout: id.clone(),
            note: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(again.kind, CommandErrorKind::NotFound);
    assert!(
        h.audit_actions(&id)
            .await
            .contains(&"rollout.abort".to_owned())
    );
}

/// A skipped host is excluded from the rollout, including from a rollback;
/// and a forward result that failed (leaving the host on the previous
/// revision) is not read as a rollback result.
#[tokio::test(start_paused = true)]
async fn rollback_leaves_skipped_hosts_and_ignores_forward_results() {
    let mut h = Harness::new().await;
    let policy = "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', parallel: 1, max_failures: 0 }\n";
    h.revision("r1", policy).await;
    h.revision("r2", policy).await;
    for x in ["a", "b", "c"] {
        h.enrol(x, "web").await;
        h.connect(x, None);
    }
    let first = h.start("r1").await;
    h.wait_state(&first, RolloutState::Completed).await;
    h.script("a", |s| s.apply_fail = Some(vec![]));
    h.script("b", |s| {
        s.apply_fail = Some(vec![]);
        s.silent_rollback = true;
    });
    let id = h.start("r2").await;
    h.wait(&id, "a failed and paused", |r, hs| {
        r.state == RolloutState::Paused && host(hs, "a").state == HostPhase::Failed
    })
    .await;
    h.command(Command::Skip {
        rollout: id.clone(),
        host: "a".into(),
        workload: String::new(),
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "b failed and paused", |r, hs| {
        r.state == RolloutState::Paused && host(hs, "b").state == HostPhase::Failed
    })
    .await;
    h.command(Command::Rollback {
        rollout: id.clone(),
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "b rolling back", |_, hs| {
        host(hs, "b").state == HostPhase::RollingBack
    })
    .await;
    // A late forward result for r2 that left b on r1.
    h.handle.agent_message(
        "b",
        AMsg::Result(ApplyResult {
            revision: "r2".into(),
            succeeded: false,
            applied_revision: "r1".into(),
            error: "late".into(),
            ..Default::default()
        }),
    );
    h.idle(5).await;
    let (_, hosts) = h.snapshot(&id).await;
    assert_eq!(host(&hosts, "b").state, HostPhase::RollingBack);
    h.handle.agent_message(
        "b",
        AMsg::Result(ApplyResult {
            revision: "r1".into(),
            succeeded: true,
            applied_revision: "r1".into(),
            ..Default::default()
        }),
    );
    let (_, hosts) = h.wait_state(&id, RolloutState::RolledBack).await;
    assert_eq!(host(&hosts, "b").state, HostPhase::RolledBack);
    assert_eq!(host(&hosts, "a").state, HostPhase::Skipped);
    assert!(
        h.sent(|x, m| x == "a" && matches!(m, SMsg::Rollback(_)))
            .is_empty(),
        "a skipped host is not rolled back"
    );
}

/// A rollback the agent cannot even plan fails the host (needs attention)
/// instead of leaving it rolling back for ever.
#[tokio::test(start_paused = true)]
async fn a_rollback_that_cannot_be_planned_fails_the_host() {
    let h = on_stable_r1("").await;
    h.script("w1", |s| {
        s.apply_fail = Some(vec![]);
        s.silent_rollback = true;
    });
    let id = h.start("r2").await;
    h.wait_state(&id, RolloutState::Paused).await;
    h.command(Command::Rollback {
        rollout: id.clone(),
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait(&id, "w1 rolling back", |_, hs| {
        host(hs, "w1").state == HostPhase::RollingBack
    })
    .await;
    h.handle.agent_message(
        "w1",
        AMsg::Plan(PlanResult {
            revision: "r1".into(),
            error: "cannot fetch r1".into(),
            ..Default::default()
        }),
    );
    let (_, hosts) = h.wait_state(&id, RolloutState::RolledBack).await;
    let w1 = host(&hosts, "w1");
    assert_eq!(w1.state, HostPhase::Failed);
    assert!(
        w1.runtime
            .error
            .as_deref()
            .unwrap()
            .starts_with("rollback failed")
    );
}

/// `baseline: skip` excludes an unhealthy host; `baseline: ignore` applies
/// to it anyway.
#[rstest::rstest]
#[case::skip("skip", HostPhase::Skipped)]
#[case::ignore("ignore", HostPhase::Healthy)]
#[tokio::test(start_paused = true)]
async fn the_baseline_policy_skips_or_ignores_an_unhealthy_host(
    #[case] policy: &str,
    #[case] expected: HostPhase,
) {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        &format!("  health: {{ bake: 30s, baseline: {policy} }}\n  phases:\n    - {{ name: web, select: 'true' }}\n"),
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.script("a", |s| s.baseline_fail = true);
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "a").state, expected);
    assert_eq!(host(&hosts, "b").state, HostPhase::Healthy);
    assert_eq!(host(&hosts, "a").phase, "web", "excluded, not a straggler");
    let applied_a = h.applies().iter().any(|x| x.1 == "a");
    assert_eq!(applied_a, expected == HostPhase::Healthy);
}

/// Continue waives only checks the host actually failed.
#[tokio::test(start_paused = true)]
async fn continue_refuses_to_waive_checks_the_host_did_not_fail() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.apply_fail = Some(vec!["web/http".into()]));
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::Paused).await;
    let refused = h
        .command(Command::Continue {
            rollout: id.clone(),
            host: "a".into(),
            workload: String::new(),
            waive: vec!["web/http".into(), "db/ping".into()],
            note: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.kind, CommandErrorKind::Invalid);
    assert!(refused.to_string().contains("db/ping"), "{refused}");
    let (r, _) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::Paused);
}

/// Under `if_destructive`, a plan with modules deferred to apply time needs
/// approval: their plan is unknown and may turn out destructive.
#[tokio::test(start_paused = true)]
async fn deferred_modules_need_approval_under_if_destructive() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', parallel: 2, approval: if_destructive }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.script("a", |s| {
        s.deferred = vec!["docker_app".into()];
        s.digest = "plan-deferred".into();
    });
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait(&id, "b healthy", |_, hs| {
        host(hs, "b").state == HostPhase::Healthy
    })
    .await;
    let (r, hosts) = h.wait_state(&id, RolloutState::AwaitingApproval).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Planned);
    assert!(h.applies().iter().all(|x| x.1 != "a"));
    let detail = super::view::detail(&r, &hosts, &[], &[]);
    let a = detail.phases[0]
        .hosts
        .iter()
        .find(|x| x.host == "a")
        .unwrap();
    assert!(a.needs_approval);
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-deferred".into(), "plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
}

impl Harness {
    /// Restarts the controller with an empty revision cache (it re-reads
    /// revisions from disk).
    async fn restart_with_fresh_cache(&mut self) {
        self.revisions = Arc::new(RevisionCache::new(self._data.path()));
        self.restart().await;
    }
}

/// A host update that changes neither status nor effective labels (a notes
/// edit) re-sends nothing; a label change pushes the update and re-sends
/// what is in flight.
#[tokio::test(start_paused = true)]
async fn a_host_update_resends_in_flight_work_only_when_labels_or_status_change() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 5m }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    // The bake request leaves the outbox only after Baking is saved, so wait
    // for its report: by then the request has reached the session's log.
    h.wait(&id, "baking with a report", |_, hs| {
        let a = host(hs, "a");
        a.state == HostPhase::Baking && a.runtime.bake_reports > 0
    })
    .await;
    let count = |h: &Harness| {
        h.sent(|x, m| {
            x == "a" && matches!(m, SMsg::HostUpdate(_) | SMsg::Health(_) | SMsg::Plan(_))
        })
        .len()
    };
    let before = count(&h);
    h.store
        .call(|db| db.hosts().set_notes("a", "rack 4").map(|_| ()))
        .await
        .unwrap();
    h.handle.host_changed("a");
    h.idle(2).await;
    assert_eq!(count(&h), before, "a notes edit re-sends nothing");

    h.store
        .call(|db| {
            let mut labels = BTreeMap::new();
            labels.insert("tier".to_string(), "web".to_string());
            labels.insert("rack".to_string(), "4".to_string());
            db.hosts().set_labels("a", &labels).map(|_| ())
        })
        .await
        .unwrap();
    h.handle.host_changed("a");
    for _ in 0..60 {
        if count(&h) > before {
            break;
        }
        h.idle(1).await;
    }
    let updates = h.sent(|x, m| x == "a" && matches!(m, SMsg::HostUpdate(_)));
    assert_eq!(updates.len(), 1, "the label change reaches the agent");
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Healthy);
}

/// A live rollout whose revision cannot be loaded at start-up is ended as
/// aborted, with the reason, instead of being left looking live.
#[tokio::test(start_paused = true)]
async fn a_rollout_whose_revision_cannot_load_at_start_up_is_aborted_with_a_reason() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  phases:\n    - { name: web, select: 'true', approval: required }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::AwaitingApproval).await;
    std::fs::remove_dir_all(h.revisions.dir("r1")).unwrap();
    h.restart_with_fresh_cache().await;
    let (r, _) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::Aborted);
    assert!(r.finished_at.is_some());
    let reason = r.runtime.end_reason.unwrap();
    assert!(reason.contains("could not be loaded"), "{reason}");
    assert!(
        h.audit_actions(&id)
            .await
            .contains(&"rollout.aborted".to_owned())
    );
}

/// A host released from a baseline hold by a skip was never updated: on
/// promotion it moves to the stragglers stage and converges there.
#[tokio::test(start_paused = true)]
async fn a_host_skipped_at_baseline_moves_to_the_stragglers_on_promotion() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.enrol("b", "web").await;
    h.script("a", |s| s.baseline_fail = true);
    h.connect("a", None);
    h.connect("b", None);
    let id = h.start("r1").await;
    h.wait(&id, "a held", |_, hs| {
        host(hs, "a").state == HostPhase::Held
    })
    .await;
    // It recovers soon after; the operator releases the phase from it.
    h.script("a", |s| s.baseline_fail = false);
    h.command(Command::Skip {
        rollout: id.clone(),
        host: "a".into(),
        workload: String::new(),
        note: "fix it later".into(),
    })
    .await
    .unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
    let (_, hosts) = h
        .wait(&id, "a healthy in stragglers", |_, hs| {
            host(hs, "a").state == HostPhase::Healthy
        })
        .await;
    assert_eq!(host(&hosts, "a").phase, STRAGGLERS);
}

/// Reject ends a rollout awaiting approval as aborted, with the reason,
/// audited; anything else is refused.
#[tokio::test(start_paused = true)]
async fn reject_aborts_a_rollout_awaiting_approval() {
    let mut h = Harness::new().await;
    h.revision("r1", TWO_PHASES).await;
    h.enrol("c1", "canary").await;
    h.connect("c1", None);
    let id = h.start("r1").await;
    let empty = h
        .command(Command::Reject {
            rollout: id.clone(),
            reason: "  ".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(empty.kind, CommandErrorKind::Invalid);
    h.wait_state(&id, RolloutState::AwaitingApproval).await;
    h.command(Command::Reject {
        rollout: id.clone(),
        reason: "wrong window".into(),
    })
    .await
    .unwrap();
    let (r, _) = h.snapshot(&id).await;
    assert_eq!(r.state, RolloutState::Aborted);
    assert!(r.runtime.end_reason.unwrap().contains("wrong window"));
    assert!(h.applies().is_empty());
    let actions = h.audit_actions(&id).await;
    assert!(
        actions.contains(&"rollout.reject".to_owned()),
        "{actions:?}"
    );
    assert!(
        actions.contains(&"rollout.aborted".to_owned()),
        "{actions:?}"
    );

    // Not awaiting approval: refused.
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 5m }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "baking", |_, hs| {
        host(hs, "a").state == HostPhase::Baking
    })
    .await;
    let refused = h
        .command(Command::Reject {
            rollout: id.clone(),
            reason: "no".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.kind, CommandErrorKind::Conflict);
}

/// A durable report the gRPC layer stored, whose reaction the controller had
/// not saved when the server stopped, is replayed on start-up (the agent was
/// never acked, and its re-send would be taken for a duplicate).
#[tokio::test(start_paused = true)]
async fn a_durable_report_stored_before_a_crash_is_replayed_on_start_up() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true' }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.silent_apply = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    let apply = h.wait_apply(&id, "a").await;
    let result = AMsg::Result(ApplyResult {
        revision: apply.revision.clone(),
        plan_digest: apply.plan_digest.clone(),
        succeeded: true,
        applied_revision: apply.revision.clone(),
        report_id: 42,
        changes: 1,
        outcome_json: outcome_json(&["web".into()]),
        ..Default::default()
    });
    // What the gRPC layer commits before handing the report over.
    let (key, value) = (
        super::pending_report_key("a", 42),
        super::encode_pending_report("a", 42, &result),
    );
    h.store
        .call(move |db| db.kv().set(&key, &value))
        .await
        .unwrap();
    h.restart().await;
    h.wait(&id, "the replayed result reacted to", |_, hs| {
        host(hs, "a").state == HostPhase::Baking
    })
    .await;
    let left = h
        .store
        .call(|db| db.kv().with_prefix("controller.pending_report."))
        .await
        .unwrap();
    assert!(left.is_empty(), "{left:?}");
    h.connect("a", Some("r1"));
    h.wait_state(&id, RolloutState::Completed).await;
}

/// The controller answers a durable report only once its reaction is
/// saved: while checkpoints fail the report is not acknowledged.
#[tokio::test(start_paused = true)]
async fn a_durable_report_is_confirmed_only_after_its_reaction_is_saved() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', max_failures: 0 }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.silent_apply = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    let apply = h.wait_apply(&id, "a").await;
    let result = AMsg::Result(ApplyResult {
        revision: apply.revision.clone(),
        plan_digest: apply.plan_digest.clone(),
        succeeded: false,
        error: "boom".into(),
        report_id: 7,
        ..Default::default()
    });
    let (key, value) = (
        super::pending_report_key("a", 7),
        super::encode_pending_report("a", 7, &result),
    );
    h.store
        .call(move |db| db.kv().set(&key, &value))
        .await
        .unwrap();
    h.fail_checkpoints(true).await;
    let mut saved = h.handle.durable_report("a", 7, result.clone());
    h.idle(10).await;
    assert!(saved.try_recv().is_err(), "not confirmed while unsaved");
    h.fail_checkpoints(false).await;
    let confirmed = tokio::time::timeout(Duration::from_secs(120), saved)
        .await
        .expect("confirmed once saved");
    assert_eq!(confirmed, Ok(true));
    let (r, hosts) = h.snapshot(&id).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Failed);
    assert_eq!(r.state, RolloutState::Paused);
    let left = h
        .store
        .call(|db| db.kv().with_prefix("controller.pending_report."))
        .await
        .unwrap();
    assert!(left.is_empty());
    // A re-send of the same report is confirmed without a second reaction.
    let again = h.handle.durable_report("a", 7, result);
    assert_eq!(again.await, Ok(true));
    let failed = h
        .audit_actions(&id)
        .await
        .into_iter()
        .filter(|a| a == "rollout.host.failed")
        .count();
    assert_eq!(failed, 1);
}

// ---------------------------------------------------------------------------
// Force apply and intent-bound approvals
// ---------------------------------------------------------------------------

const ONE_PHASE: &str =
    "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', max_failures: 0 }\n";

/// A one-host rollout of `phases`, with `script` applied to the host before
/// it connects; returns the rollout id once `ready` holds for the host.
async fn one_host(
    h: &mut Harness,
    phases: &str,
    script: impl FnOnce(&mut Script),
    ready: fn(&RolloutHostRecord) -> bool,
) -> String {
    h.revision("r1", phases).await;
    h.enrol("a", "web").await;
    h.script("a", script);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "host ready", |_, hs| ready(host(hs, "a")))
        .await;
    id
}

fn force(id: &str) -> Command {
    Command::Force {
        rollout: id.to_owned(),
        host: "a".into(),
        workload: String::new(),
        note: "operator says so".into(),
    }
}

#[rstest::rstest]
#[case::non_deterministic_plan(ONE_PHASE, (|s: &mut Script| s.stale_plan_always = true) as fn(&mut Script), HostPhase::Held)]
#[case::failed_apply(ONE_PHASE, (|s: &mut Script| s.apply_fail = Some(vec![])) as fn(&mut Script), HostPhase::Failed)]
#[case::awaiting_approval(
    "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n",
    (|_: &mut Script| {}) as fn(&mut Script),
    HostPhase::Planned
)]
#[tokio::test(start_paused = true)]
async fn force_apply_runs_whatever_the_host_plans_now(
    #[case] phases: &str,
    #[case] script: fn(&mut Script),
    #[case] from: HostPhase,
) {
    let mut h = Harness::new().await;
    let ready: fn(&RolloutHostRecord) -> bool = match from {
        HostPhase::Held => |a| a.state == HostPhase::Held,
        HostPhase::Failed => |a| a.state == HostPhase::Failed,
        _ => |a| a.state == HostPhase::Planned,
    };
    let id = one_host(&mut h, phases, script, ready).await;
    let before = h.applies().len();
    // The forced apply itself succeeds.
    h.script("a", |s| {
        s.apply_fail = None;
        s.stale_plan_always = false;
        s.digest = "plan-now".into();
    });
    h.command(force(&id)).await.unwrap();
    let (_, hosts) = h
        .wait(&id, "healthy", |_, hs| {
            host(hs, "a").state == HostPhase::Healthy
        })
        .await;
    let applies = h.applies();
    assert_eq!(applies.len(), before + 1);
    assert!(
        applies.last().unwrap().2.force_plan,
        "the Apply binds no digest"
    );
    assert_eq!(
        host(&hosts, "a").plan_digest.as_deref(),
        Some("plan-now"),
        "the applied plan is recorded"
    );
    assert!(
        h.audit_actions(&id)
            .await
            .contains(&"rollout.host.force".to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn force_apply_is_refused_for_an_agent_that_cannot_skip_its_digest_check() {
    let mut h = Harness::new().await;
    let id = one_host(
        &mut h,
        ONE_PHASE,
        |s| {
            s.apply_fail = Some(vec![]);
            s.legacy_agent = true;
        },
        |a| a.state == HostPhase::Failed,
    )
    .await;
    let err = h.command(force(&id)).await.expect_err("refused");
    assert_eq!(err.kind, CommandErrorKind::Conflict);
    assert!(!h.applies().last().unwrap().2.force_plan);
}

#[tokio::test(start_paused = true)]
async fn an_approval_covers_a_replan_that_only_changes_content() {
    let mut h = Harness::new().await;
    let id = one_host(
        &mut h,
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n",
        |s| s.intent = "intent-1".into(),
        |a| a.state == HostPhase::Planned,
    )
    .await;
    // The host's content drifts after the plan was reviewed.
    h.script("a", |s| s.digest = "plan-b".into());
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let applies = h.applies();
    assert_eq!(applies.len(), 1, "no re-approval round");
    assert_eq!(applies[0].2.approval_digest, "intent-1");
    assert_eq!(host(&hosts, "a").plan_digest.as_deref(), Some("plan-b"));
    assert!(
        h.audit_actions(&id)
            .await
            .contains(&"rollout.host.plan_content_changed".to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn a_deferred_module_that_diverged_from_its_preview_replans_the_host() {
    let mut h = Harness::new().await;
    let id = one_host(
        &mut h,
        ONE_PHASE,
        |s| s.stale_deferred = vec!["nomad".into()],
        |a| a.state != HostPhase::PlanPending,
    )
    .await;
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Healthy, "not failed");
    assert_eq!(h.applies().len(), 2, "re-planned and applied again");
    assert_eq!(r.runtime.phases[0].accepted_failures, 0);
    let plans = h
        .log
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, m)| matches!(m, SMsg::Plan(_)))
        .count();
    assert_eq!(plans, 2, "the host was asked to plan again");
}

// ---------------------------------------------------------------- reboots

fn reboot(timeout_secs: i64) -> pb::Reboot {
    pb::Reboot {
        delay: Some(prost_types::Duration {
            seconds: 5,
            nanos: 0,
        }),
        timeout: Some(prost_types::Duration {
            seconds: timeout_secs,
            nanos: 0,
        }),
        boot_id: "boot-1".into(),
        requested_by: vec!["pi:reboot".into()],
    }
}

/// A one-host rollout whose apply asks for a reboot; returns once the host
/// is rebooting.
async fn rebooting(h: &mut Harness, timeout_secs: i64) -> String {
    h.revision("r1", "  phases:\n    - { name: web, select: 'true' }\n")
        .await;
    h.enrol("a", "web").await;
    h.script("a", |s| {
        s.boot_id = Some("boot-1".into());
        s.reboot = Some(reboot(timeout_secs));
    });
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "rebooting", |_, hs| {
        host(hs, "a").state == HostPhase::Rebooting
    })
    .await;
    id
}

#[tokio::test(start_paused = true)]
async fn a_rebooting_host_is_waited_for_until_its_agent_is_back_from_a_new_boot() {
    let mut h = Harness::new().await;
    let id = rebooting(&mut h, 1800).await;
    // Away for longer than the disconnect timeout: the reboot timeout governs.
    h.disconnect("a");
    h.idle(11 * 60).await;
    let (_, hosts) = h.snapshot(&id).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Rebooting);
    // A session from the same boot (before the reboot happened) is not it.
    h.connect("a", Some("r1"));
    h.idle(5).await;
    assert_eq!(h.snapshot(&id).await.1[0].state, HostPhase::Rebooting);
    h.disconnect("a");
    h.script("a", |s| s.boot_id = Some("boot-2".into()));
    let back = h.clock.now();
    h.connect("a", Some("r1"));
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let a = host(&hosts, "a");
    assert_eq!(a.state, HostPhase::Healthy);
    assert!(
        a.runtime
            .reboot
            .as_ref()
            .is_some_and(|w| w.back_at.is_some())
    );
    // Its deferred checks ran after the reboot, for at least a minute.
    let bake: Vec<DateTime<Utc>> = h
        .sent(|host, m| {
            host == "a" && matches!(m, SMsg::Health(x) if x.scope == HealthScope::Bake as i32)
        })
        .into_iter()
        .map(|(at, _, _)| at)
        .collect();
    assert!(!bake.is_empty() && bake.iter().all(|at| *at >= back));
    assert!(r.finished_at.unwrap() - back >= chrono::Duration::seconds(60));
    let actions = h.audit_actions(&id).await;
    for action in ["rollout.host.rebooting", "rollout.host.rebooted"] {
        assert!(
            actions.iter().any(|a| a == action),
            "{action} in {actions:?}"
        );
    }
}

#[derive(Debug, Clone, Copy)]
enum NotBack {
    Offline,
    SameBoot,
    RebootFailed,
}

#[rstest::rstest]
#[case::offline(NotBack::Offline, "did not come back")]
#[case::never_rebooted(NotBack::SameBoot, "still up on the same boot")]
#[case::reboot_failed(NotBack::RebootFailed, "Access denied")]
#[tokio::test(start_paused = true)]
async fn a_host_that_does_not_come_back_from_its_reboot_fails(
    #[case] how: NotBack,
    #[case] reason: &str,
) {
    let mut h = Harness::new().await;
    let id = rebooting(&mut h, 300).await;
    match how {
        NotBack::Offline => h.disconnect("a"),
        NotBack::SameBoot => {}
        NotBack::RebootFailed => h.handle.agent_message(
            "a",
            AMsg::RebootFailed(pb::RebootFailed {
                boot_id: "boot-1".into(),
                error: "`systemctl reboot` failed: Access denied".into(),
            }),
        ),
    }
    let (_, hosts) = h
        .wait(&id, "failed", |_, hs| {
            host(hs, "a").state == HostPhase::Failed
        })
        .await;
    let error = host(&hosts, "a").runtime.error.clone().unwrap_or_default();
    assert!(error.contains(reason), "{error}");
}

#[tokio::test(start_paused = true)]
async fn a_result_that_arrives_after_the_reboot_goes_straight_to_baking() {
    let mut h = Harness::new().await;
    h.revision("r1", "  phases:\n    - { name: web, select: 'true' }\n")
        .await;
    h.enrol("a", "web").await;
    h.script("a", |s| {
        s.boot_id = Some("boot-1".into());
        s.silent_apply = true;
    });
    h.connect("a", None);
    let id = h.start("r1").await;
    let apply = h.wait_apply(&id, "a").await;
    // The agent rebooted before the server acknowledged the result: it comes
    // back from another boot and re-sends it.
    h.disconnect("a");
    h.script("a", |s| s.boot_id = Some("boot-2".into()));
    h.attach("a");
    h.connect_with("a", Some("r1"), vec!["r1".into()]);
    h.idle(5).await;
    assert_eq!(h.snapshot(&id).await.1[0].state, HostPhase::Applying);
    h.handle.agent_message(
        "a",
        AMsg::Result(ApplyResult {
            revision: "r1".into(),
            plan_digest: apply.plan_digest,
            succeeded: true,
            applied_revision: "r1".into(),
            reboot: Some(reboot(300)),
            ..Default::default()
        }),
    );
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Healthy);
    assert!(
        h.audit_actions(&id)
            .await
            .iter()
            .any(|a| a == "rollout.host.rebooted")
    );
}

/// A target that appears after the freeze joins a phase the rollout has not
/// reached yet (late admission); one whose phase is already running waits
/// for the stragglers stage, as before.
#[tokio::test(start_paused = true)]
async fn a_target_appearing_mid_rollout_joins_a_phase_not_yet_started() {
    let mut h = Harness::new().await;
    h.revision("r1", TWO_PHASES).await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.connect("c1", None);
    h.connect("w1", None);
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::AwaitingApproval).await;

    // Enrolled while the canary phase waits: `web` has not started, so the
    // host joins it on connecting; a second canary host has missed its phase.
    h.enrol("late-web", "web").await;
    h.handle.host_registered("late-web");
    h.connect("late-web", None);
    h.enrol("late-canary", "canary").await;
    h.handle.host_changed("late-canary");
    let (_, hosts) = h
        .wait(&id, "late web host admitted and planned", |_, hs| {
            hs.iter()
                .any(|x| x.host == "late-web" && x.state == HostPhase::Planned)
        })
        .await;
    assert_eq!(host(&hosts, "late-web").phase, "web");
    assert!(
        !hosts.iter().any(|x| x.host == "late-canary"),
        "a phase already reached is not joined"
    );

    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "canary".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, "late-web").state, HostPhase::Healthy);
    assert_eq!(host(&hosts, "late-web").phase, "web");
    assert!(r.runtime.stragglers_open);
    assert_eq!(host(&hosts, "late-canary").phase, STRAGGLERS);
    let actions = h.audit_actions(&id).await;
    assert!(actions.iter().any(|a| a == "rollout.target.admitted"));
}

// ---------------------------------------------------------------- clusters

/// A Nomad cluster whose nodes (and so executors) are the `nomad` tier.
const NOMAD_CLUSTER: &str = "  clusters:\n    home-nomad:\n      kind: nomad\n      nodes: 'host.groups contains \"nomad\"'\n";
const CLUSTER: &str = "home-nomad";
const ONE_PHASE_ALL: &str =
    "  health: { bake: 30s }\n  phases:\n    - { name: all, select: 'true', parallel: 4 }\n";

/// `nomad` on the nodes, a workload needing it, and `backup` needing the
/// workload: three stages.
const LAYERED: &[(&str, &str)] = &[
    (
        "modules/nomad/module.yaml",
        "name: nomad\nwhen: 'host.groups contains \"nomad\"'\n",
    ),
    (
        "modules/backup/module.yaml",
        "name: backup\nwhen: 'host.groups contains \"nomad\"'\nneeds: ['workload:postgres']\n",
    ),
    (
        "workloads/postgres/workload.yaml",
        "name: postgres\nneeds: ['module:nomad']\nresources: [postgres.nomad.hcl]\n",
    ),
    (
        "workloads/postgres/files/postgres.nomad.hcl",
        "job \"postgres\" {}\n",
    ),
];

/// One workload with no cross-layer needs: one stage.
const FLAT: &[(&str, &str)] = &[
    (
        "workloads/postgres/workload.yaml",
        "name: postgres\nresources: [postgres.nomad.hcl]\n",
    ),
    (
        "workloads/postgres/files/postgres.nomad.hcl",
        "job \"postgres\" {}\n",
    ),
];

/// The cluster a message is aimed at, if any.
fn targeted(m: &SMsg) -> Option<&str> {
    match m {
        SMsg::Assign(a) => Some(a.target.as_str()),
        SMsg::Plan(p) => Some(p.target.as_str()),
        SMsg::Apply(a) => Some(a.target.as_str()),
        SMsg::Health(h) => Some(h.target.as_str()),
        SMsg::Lease(l) => Some(l.target.as_str()),
        SMsg::Release(r) => Some(r.target.as_str()),
        _ => None,
    }
    .filter(|t| !t.is_empty())
}

/// Cross-layer `needs` lay a phase out in stages: the nodes' `nomad` module
/// first, the cluster's workload next (planned when its stage opens, through
/// an executor picked among the nodes), and the module needing the workload
/// last, as a second whole-revision step of the same hosts.
#[tokio::test(start_paused = true)]
async fn a_cluster_is_staged_between_its_nodes_modules_and_applied_through_an_executor() {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, LAYERED)
        .await;
    h.enrol("n1", "nomad").await;
    h.enrol("n2", "nomad").await;
    h.connect("n1", None);
    h.connect("n2", None);
    let id = h.start("r1").await;
    let (r, hosts) = h
        .wait(&id, "the freeze", |_, hs| {
            hs.iter().any(|x| x.host == CLUSTER)
        })
        .await;
    assert_eq!(r.runtime.phases[0].stages, 3);
    let cluster = host(&hosts, CLUSTER);
    assert_eq!(cluster.runtime.kind, TargetKind::Workload);
    assert_eq!(
        cluster.workload, "postgres",
        "one placement row per workload"
    );
    assert_eq!(cluster.runtime.stage, 1);
    for n in ["n1", "n2"] {
        let steps: Vec<(u32, bool)> = host(&hosts, n)
            .runtime
            .steps
            .iter()
            .map(|s| (s.stage, s.select().contains("nomad")))
            .collect();
        assert_eq!(steps, [(0, true), (2, false)], "{n}");
    }
    let (_, hosts) = h
        .wait(&id, "the cluster's preview", |_, hs| {
            host(hs, CLUSTER).state == HostPhase::Planned
        })
        .await;
    assert!(
        host(&hosts, CLUSTER).runtime.preview,
        "a cluster is previewed at the freeze, before its stage opens"
    );

    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let cluster = host(&hosts, CLUSTER);
    assert_eq!(cluster.state, HostPhase::Healthy);
    assert_eq!(cluster.runtime.executor.as_deref(), Some("n1"));
    for n in ["n1", "n2"] {
        assert_eq!(host(&hosts, n).state, HostPhase::Healthy, "{n}");
        assert_eq!(host(&hosts, n).runtime.step, 1, "{n} ran both steps");
    }
    // The nodes' module step, then the cluster over n1's session, then the
    // nodes' whole-revision step.
    let applies = h.applies();
    let index = |pred: &dyn Fn(&pb::Apply) -> bool| -> Vec<usize> {
        applies
            .iter()
            .enumerate()
            .filter(|(_, a)| pred(&a.2))
            .map(|(i, _)| i)
            .collect()
    };
    let module_step = index(&|a| a.target.is_empty() && a.select.contains("nomad"));
    let cluster_apply = index(&|a| a.target == CLUSTER && a.workload == "postgres");
    let whole = index(&|a| a.target.is_empty() && a.select.is_empty());
    assert_eq!(
        (module_step.len(), cluster_apply.len(), whole.len()),
        (2, 1, 2),
        "{applies:?}"
    );
    assert_eq!(applies[cluster_apply[0]].1, "n1");
    assert!(module_step.iter().all(|&i| i < cluster_apply[0]));
    assert!(whole.iter().all(|&i| i > cluster_apply[0]));
    // The executor keeps nothing: every request for the cluster follows an
    // Assign of its revision describing the cluster, with its secrets and
    // fingerprint key.
    let to_executor: Vec<SMsg> = h
        .sent(|host, m| host == "n1" && targeted(m).is_some())
        .into_iter()
        .map(|(_, _, m)| m)
        .collect();
    let mut requests = 0;
    for (i, m) in to_executor.iter().enumerate() {
        let revision = match m {
            SMsg::Plan(p) => &p.revision,
            SMsg::Apply(a) => &a.revision,
            // Health before the cluster applied anything names no revision.
            SMsg::Health(hr) if !hr.revision.is_empty() => &hr.revision,
            _ => continue,
        };
        requests += 1;
        match i.checked_sub(1).map(|j| &to_executor[j]) {
            Some(SMsg::Assign(a)) => {
                assert_eq!(&a.revision, revision, "{m:?}");
                let c = a
                    .cluster
                    .as_ref()
                    .expect("the Assign describes the cluster");
                assert_eq!(c.kind, "nomad");
                assert_eq!(c.fingerprint_key.len(), 32);
            }
            other => panic!("{m:?} followed {other:?}, not an Assign"),
        }
    }
    assert!(requests >= 2, "plan and apply: {to_executor:?}");
    let actions = h.audit_actions(&id).await;
    for a in [
        "rollout.phase.stage",
        "rollout.host.staged",
        "rollout.target.executor",
    ] {
        assert!(
            actions.iter().any(|x| x == a),
            "{a} missing from {actions:?}"
        );
    }
}

/// An executor running inside the cluster speaks for it, though no
/// selector names it, and runs no modules of its own.
#[tokio::test(start_paused = true)]
async fn an_in_cluster_executor_serves_only_its_cluster() {
    let mut h = Harness::new().await;
    let other = "  clusters:\n    home-nomad:\n      kind: nomad\n      nodes: 'host.groups contains \"nomad\"'\n    lab-nomad:\n      kind: nomad\n";
    h.revision_with("r1", ONE_PHASE_ALL, other, FLAT).await;
    h.enrol("n1", "nomad").await;
    h.script("n1", |s| s.no_targets = true);
    h.enrol_executor("home-nomad-executor", CLUSTER).await;
    h.connect("n1", None);
    h.connect("home-nomad-executor", None);
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(&id, "home-nomad healthy", |_, hs| {
            host(hs, CLUSTER).state == HostPhase::Healthy
        })
        .await;
    assert_eq!(
        host(&hosts, CLUSTER).runtime.executor.as_deref(),
        Some("home-nomad-executor")
    );
    assert!(
        hosts.iter().all(|x| x.host != "home-nomad-executor"),
        "no row of its own"
    );
    assert_eq!(host(&hosts, "lab-nomad").runtime.executor, None);
    let untargeted = h.sent(|to, m| {
        to == "home-nomad-executor"
            && targeted(m).is_none()
            && matches!(m, SMsg::Plan(_) | SMsg::Apply(_))
    });
    assert!(untargeted.is_empty(), "{untargeted:?}");
}

/// Between rollouts an applied cluster stays leased to its last executor,
/// which watches it; while that one is away past the disconnect timeout the
/// lease moves to another capable host (the record follows); with none
/// online, nobody holds it.
#[rstest::rstest]
#[case::last_executor(2, false)]
#[case::another_while_the_last_is_away(2, true)]
#[case::none_online(1, true)]
#[tokio::test(start_paused = true)]
async fn an_applied_cluster_stays_leased_between_rollouts(
    #[case] nodes: usize,
    #[case] last_away: bool,
) {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, FLAT)
        .await;
    let names: Vec<String> = (1..=nodes).map(|i| format!("n{i}")).collect();
    for n in &names {
        h.enrol(n, "nomad").await;
        h.connect(n, None);
    }
    let id = h.start("r1").await;
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let last = host(&hosts, CLUSTER).runtime.executor.clone().unwrap();
    let leased_to = |h: &Harness, to: &str| {
        h.sent(|x, m| x == to && matches!(m, SMsg::Lease(l) if l.target == CLUSTER))
    };
    assert!(
        !leased_to(&h, &last).is_empty(),
        "the executor holds the lease"
    );
    assert_eq!(h.steward(CLUSTER).await.as_deref(), Some(last.as_str()));
    // The gRPC route records the applied revision; here, by hand.
    h.store
        .call(|db| db.hosts().set_applied_revision(CLUSTER, "r1"))
        .await
        .unwrap();
    if last_away {
        h.disconnect(&last);
    }
    h.idle(11 * 60).await;
    let expected = if last_away {
        names.iter().find(|n| **n != last)
    } else {
        Some(&last)
    };
    assert_eq!(
        h.steward(CLUSTER).await.as_deref(),
        expected.map(String::as_str)
    );
    match expected {
        Some(other) if last_away => {
            let moved = leased_to(&h, other);
            assert_eq!(moved.len(), 1, "{moved:?}");
            // The lease names the applied revision, behind its Assign.
            assert!(
                matches!(&moved[0].2, SMsg::Lease(l) if l.applied_revision == "r1" && l.cluster.is_some()),
                "{moved:?}"
            );
            let released = h.sent(|x, m| x == last && matches!(m, SMsg::Release(_)));
            assert!(
                released.is_empty(),
                "nobody to release while away: {released:?}"
            );
        }
        Some(same) => {
            let others = h.sent(|x, m| x != same && matches!(m, SMsg::Lease(_)));
            assert!(others.is_empty(), "{others:?}");
        }
        None => {}
    }
}

/// A cluster none of whose candidate executors runs a pipeline per target is
/// handed nothing, waits through the grace, misses its phase and converges
/// in the stragglers stage once a capable host connects.
#[tokio::test(start_paused = true)]
async fn a_cluster_without_a_capable_executor_misses_its_phase_and_converges_in_stragglers() {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, FLAT)
        .await;
    h.enrol("n1", "nomad").await;
    h.script("n1", |s| s.no_targets = true);
    h.connect("n1", None);
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(&id, "n1 healthy", |_, hs| {
            host(hs, "n1").state == HostPhase::Healthy
        })
        .await;
    let cluster = host(&hosts, CLUSTER);
    assert_eq!(cluster.state, HostPhase::PlanPending);
    assert_eq!(cluster.runtime.executor, None);
    assert!(
        cluster.runtime.disconnected_at.is_some(),
        "awaiting an executor runs the disconnect grace"
    );
    assert!(
        h.sent(|_, m| targeted(m).is_some()).is_empty(),
        "an agent without the capability is never handed cluster work"
    );

    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(r.runtime.stragglers_open);
    assert_eq!(host(&hosts, CLUSTER).phase, STRAGGLERS);
    assert_eq!(host(&hosts, CLUSTER).state, HostPhase::PlanPending);

    h.enrol("n2", "nomad").await;
    h.connect("n2", None);
    let (_, hosts) = h
        .wait(&id, "cluster healthy in stragglers", |_, hs| {
            host(hs, CLUSTER).state == HostPhase::Healthy
        })
        .await;
    assert_eq!(
        host(&hosts, CLUSTER).runtime.executor.as_deref(),
        Some("n2")
    );
    assert_eq!(host(&hosts, "n2").state, HostPhase::Healthy);
    let to_n2 = h.sent(|host, m| host == "n2" && targeted(m) == Some(CLUSTER));
    assert!(
        to_n2.iter().any(|(_, _, m)| matches!(m, SMsg::Apply(_))),
        "the cluster's apply travels over n2's session: {to_n2:?}"
    );
}

/// An executor that stays away past the disconnect timeout mid-apply is
/// replaced: the cluster re-plans through the next candidate and finishes
/// there, with every pick in the audit log.
#[tokio::test(start_paused = true)]
async fn a_lost_executor_is_replaced_and_the_cluster_re_planned_through_the_next() {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, FLAT)
        .await;
    h.enrol("n1", "nomad").await;
    h.enrol("n2", "nomad").await;
    h.script("n1", |s| s.silent_target_apply = true);
    h.connect("n1", None);
    h.connect("n2", None);
    let id = h.start("r1").await;
    h.wait(
        &id,
        "nodes healthy, cluster applying through n1",
        |_, hs| {
            let c = host(hs, CLUSTER);
            c.state == HostPhase::Applying
                && c.runtime.executor.as_deref() == Some("n1")
                && host(hs, "n1").state == HostPhase::Healthy
                && host(hs, "n2").state == HostPhase::Healthy
        },
    )
    .await;
    h.disconnect("n1");
    h.wait(&id, "cluster re-planned through n2", |_, hs| {
        host(hs, CLUSTER).runtime.executor.as_deref() == Some("n2")
    })
    .await;
    h.connect("n1", None);
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let cluster = host(&hosts, CLUSTER);
    assert_eq!(cluster.state, HostPhase::Healthy);
    assert_eq!(cluster.runtime.executor.as_deref(), Some("n2"));
    assert_eq!(
        h.applies()
            .iter()
            .filter(|a| a.1 == "n2" && a.2.target == CLUSTER)
            .count(),
        1
    );
    let actions = h.audit_actions(&id).await;
    assert_eq!(
        actions
            .iter()
            .filter(|a| *a == "rollout.target.executor")
            .count(),
        2,
        "picked, then moved with the lease: {actions:?}"
    );
    // n1 was released (once back) and n2 leased the cluster.
    let leases = h.sent(|to, m| to == "n2" && matches!(m, SMsg::Lease(l) if l.target == CLUSTER));
    assert!(
        !leases.is_empty(),
        "{:?}",
        h.sent(|_, m| targeted(m).is_some())
    );
}

/// Retrying a rollout paused on failed plans re-plans every failed host and
/// carries on from the same phase (it does not accept the failures and move
/// on, as resuming does).
#[tokio::test(start_paused = true)]
async fn retrying_a_rollout_replans_every_failed_host() {
    let mut h = Harness::new().await;
    h.revision("r1", ONE_PHASE_TWO_WIDE).await;
    for x in ["a", "b"] {
        h.enrol(x, "web").await;
        h.script(x, |s| s.plan_error = Some("identity refused".into()));
        h.connect(x, None);
    }
    let id = h.start("r1").await;
    let (r, _) = h
        .wait(&id, "paused with both failed", |r, hs| {
            r.state == RolloutState::Paused && hs.iter().all(|x| x.state == HostPhase::Failed)
        })
        .await;
    assert_eq!(r.runtime.pause.as_ref().unwrap().kind, "failure_budget");
    for x in ["a", "b"] {
        h.script(x, |s| s.plan_error = None);
    }
    h.command(Command::RetryFailed {
        rollout: id.clone(),
        note: String::new(),
    })
    .await
    .unwrap();
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));
    assert_eq!(r.runtime.phases[0].accepted_failures, 0);
}

#[tokio::test(start_paused = true)]
async fn retrying_a_rollout_without_failures_is_refused() {
    let mut h = Harness::new().await;
    h.revision("r1", ONE_PHASE_TWO_WIDE).await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.silent_apply = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "applying", |_, hs| {
        host(hs, "a").state == HostPhase::Applying
    })
    .await;
    let err = h
        .command(Command::RetryFailed {
            rollout: id.clone(),
            note: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.kind, CommandErrorKind::Conflict);
}

const LONG_BAKE: &str = "  settle: 10m\n  health: { bake: 1h }\n  phases:\n    - { name: web, select: 'true', parallel: 2 }\n";

/// Skipping a bake (one host's, or every baking host's) counts it as baked
/// at once, instead of waiting out the window.
#[rstest::rstest]
#[case::one_host(Some("a"))]
#[case::rollout(None)]
#[tokio::test(start_paused = true)]
async fn skipping_a_bake_finishes_the_host_now(#[case] target: Option<&str>) {
    let mut h = Harness::new().await;
    h.revision("r1", LONG_BAKE).await;
    for x in ["a", "b"] {
        h.enrol(x, "web").await;
        h.connect(x, None);
    }
    let id = h.start("r1").await;
    h.wait(&id, "baking", |_, hs| {
        hs.iter().all(|x| x.state == HostPhase::Baking)
    })
    .await;
    h.command(Command::SkipBake {
        rollout: id.clone(),
        host: target.map(String::from),
        workload: String::new(),
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h.snapshot(&id).await;
    for x in ["a", "b"] {
        let skipped = target.is_none_or(|t| t == x);
        let want = if skipped {
            HostPhase::Healthy
        } else {
            HostPhase::Baking
        };
        assert_eq!(host(&hosts, x).state, want, "{x}");
    }
}

/// A bake with failing checks is not skipped: skipping never passes a
/// regression.
#[tokio::test(start_paused = true)]
async fn a_bake_with_failing_checks_cannot_be_skipped() {
    let mut h = Harness::new().await;
    h.revision("r1", LONG_BAKE).await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.bake_fail = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait(&id, "failing in its bake", |_, hs| {
        let a = host(hs, "a");
        a.state == HostPhase::Baking && !a.runtime.failing_since.is_empty()
    })
    .await;
    for target in [Some("a".to_string()), None] {
        let err = h
            .command(Command::SkipBake {
                rollout: id.clone(),
                host: target,
                workload: String::new(),
                note: String::new(),
            })
            .await
            .unwrap_err();
        assert_eq!(err.kind, CommandErrorKind::Conflict);
    }
    let (_, hosts) = h.snapshot(&id).await;
    assert_eq!(host(&hosts, "a").state, HostPhase::Baking);
}

/// A host whose plan is not applied yet can be planned again: it gets a fresh
/// plan and keeps waiting for its approval, nothing applies.
#[tokio::test(start_paused = true)]
async fn a_planned_host_can_be_planned_again_before_it_applies() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n",
    )
    .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::AwaitingApproval).await;
    let plans = || h.sent(|x, m| x == "a" && matches!(m, SMsg::Plan(_))).len();
    let before = plans();
    h.command(Command::Retry {
        rollout: id.clone(),
        host: "a".into(),
        workload: String::new(),
        note: String::new(),
    })
    .await
    .unwrap();
    let (_, hosts) = h
        .wait(&id, "planned again", |_, hs| {
            host(hs, "a").state == HostPhase::Planned
        })
        .await;
    assert!(plans() > before, "a fresh plan is requested");
    assert!(!host(&hosts, "a").runtime.released, "it keeps its place");
    assert!(h.applies().is_empty(), "nothing applies without approval");
}

// ---------------------------------------------------------------- previews

const TWO_AUTO_PHASES: &str = "  health: { bake: 30s }\n  phases:\n    - { name: canary, select: 'host.groups contains \"canary\"' }\n    - { name: web, select: 'host.groups contains \"web\"' }\n";

fn plans_to(h: &Harness, to: &str, target: &str) -> usize {
    h.sent(|x, m| x == to && matches!(m, SMsg::Plan(p) if p.target == target))
        .len()
}

/// A host of a later phase is planned at the freeze as a preview and again
/// when its phase starts; what applies is the fresh plan, and a plan that
/// changed in between is recorded as drift (an auto phase applies it).
#[tokio::test(start_paused = true)]
async fn a_later_phase_is_planned_again_when_it_starts() {
    for drift in [false, true] {
        let mut h = Harness::new().await;
        h.revision("r1", TWO_AUTO_PHASES).await;
        h.enrol("c", "canary").await;
        h.enrol("w", "web").await;
        h.connect("c", None);
        h.connect("w", None);
        let id = h.start("r1").await;
        let (_, hosts) = h
            .wait(&id, "w previewed", |_, hs| {
                host(hs, "w").state == HostPhase::Planned
            })
            .await;
        assert!(host(&hosts, "w").runtime.preview, "drift={drift}");
        assert!(
            !host(&hosts, "c").runtime.preview,
            "the open phase is no preview"
        );
        if drift {
            h.script("w", |s| s.digest = "plan-b".into());
        }

        let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
        let w = host(&hosts, "w");
        assert_eq!(w.state, HostPhase::Healthy, "drift={drift}");
        assert_eq!(
            plans_to(&h, "w", ""),
            2,
            "previewed, then planned at its phase"
        );
        assert_eq!(plans_to(&h, "c", ""), 1, "the first phase is planned once");
        let applied = h.applies().into_iter().find(|a| a.1 == "w").unwrap().2;
        let expected = if drift { "plan-b" } else { "plan-a" };
        assert_eq!(applied.plan_digest, expected, "the fresh plan applies");
        assert_eq!(w.runtime.drifted_from.is_some(), drift);
        let drifted = h
            .audit_actions(&id)
            .await
            .iter()
            .any(|a| a == "rollout.target.plan_drifted");
        assert_eq!(drifted, drift);
    }
}

/// A cluster previewed at the freeze is covered by its phase's approval:
/// planned again when its stage opens, it applies under that approval when
/// its plan is unchanged and waits for a new one when it drifted.
#[tokio::test(start_paused = true)]
async fn a_previewed_cluster_needs_approval_again_only_when_its_plan_drifted() {
    let gated = "  health: { bake: 30s }\n  phases:\n    - { name: all, select: 'true', parallel: 4, approval: required }\n";
    for drift in [false, true] {
        let mut h = Harness::new().await;
        h.revision_with("r1", gated, NOMAD_CLUSTER, LAYERED).await;
        for n in ["n1", "n2"] {
            h.enrol(n, "nomad").await;
            h.connect(n, None);
        }
        let id = h.start("r1").await;
        let (_, hosts) = h
            .wait(&id, "every plan in", |_, hs| {
                hs.iter().all(|x| x.state == HostPhase::Planned)
            })
            .await;
        assert!(host(&hosts, CLUSTER).runtime.preview);
        h.command(Command::Approve {
            rollout: id.clone(),
            phase: "all".into(),
            digests: vec!["plan-a".into()],
            note: String::new(),
        })
        .await
        .unwrap();
        if drift {
            for n in ["n1", "n2"] {
                h.script(n, |s| s.target_digest = Some("plan-b".into()));
            }
        }

        if !drift {
            let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
            assert_eq!(host(&hosts, CLUSTER).state, HostPhase::Healthy);
            assert!(host(&hosts, CLUSTER).runtime.drifted_from.is_none());
            continue;
        }
        let (_, hosts) = h
            .wait(&id, "the cluster re-planned", |_, hs| {
                host(hs, CLUSTER).plan_digest.as_deref() == Some("plan-b")
            })
            .await;
        let cluster = host(&hosts, CLUSTER);
        assert_eq!(cluster.runtime.drifted_from.as_deref(), Some("plan-a"));
        assert!(!cluster.runtime.preview);
        h.idle(120).await;
        let (_, hosts) = h.snapshot(&id).await;
        assert_eq!(
            host(&hosts, CLUSTER).state,
            HostPhase::Planned,
            "it waits for approval"
        );
        assert!(h.applies().iter().all(|a| a.2.target != CLUSTER));
        assert!(
            h.audit_actions(&id)
                .await
                .iter()
                .any(|a| a == "rollout.target.plan_drifted")
        );
        h.command(Command::Approve {
            rollout: id.clone(),
            phase: "all".into(),
            digests: vec!["plan-b".into()],
            note: String::new(),
        })
        .await
        .unwrap();
        let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
        assert_eq!(host(&hosts, CLUSTER).state, HostPhase::Healthy);
    }
}

/// A cluster whose preview cannot be planned (an earlier stage provides what
/// it needs) is not failed: it stays unplanned and plans when its stage
/// opens.
#[tokio::test(start_paused = true)]
async fn a_cluster_whose_preview_fails_plans_when_its_stage_opens() {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, LAYERED)
        .await;
    for n in ["n1", "n2"] {
        h.enrol(n, "nomad").await;
        h.script(n, |s| {
            s.target_plan_error = Some("namespace missing".into())
        });
        h.connect(n, None);
    }
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(&id, "the preview failed", |_, hs| {
            host(hs, CLUSTER).runtime.error.is_some()
        })
        .await;
    assert_eq!(host(&hosts, CLUSTER).state, HostPhase::PlanPending);
    for n in ["n1", "n2"] {
        h.script(n, |s| s.target_plan_error = None);
    }
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(host(&hosts, CLUSTER).state, HostPhase::Healthy);
}

#[tokio::test(start_paused = true)]
async fn an_action_is_refused_while_its_target_applies_in_a_rollout() {
    let mut h = Harness::new().await;
    h.revision("r1", "  phases:\n    - { name: web, select: 'true' }\n")
        .await;
    h.enrol("a", "web").await;
    h.script("a", |s| s.silent_apply = true);
    h.connect("a", None);
    let id = h.start("r1").await;
    h.wait_apply(&id, "a").await;
    let refused = h
        .command(Command::RunAction {
            target: "a".into(),
            module: "m".into(),
            action: "restart".into(),
            revision: "r1".into(),
            note: String::new(),
        })
        .await
        .expect_err("refused mid-apply");
    assert_eq!(refused.kind, CommandErrorKind::Conflict);
    assert!(h.sent(|_, m| matches!(m, SMsg::Action(_))).is_empty());
}

// ---------------------------------------------------------------- action rollouts

const ACTION_PHASES: &str = "  health: { bake: 1m }\n  phases:\n    - { name: canary, select: 'host.groups contains \"canary\"' }\n    - { name: web, select: 'host.groups contains \"web\"', parallel: 1 }\n";

/// Hosts c1 (canary), w1, w2 (web) and x (web, not targeted), connected on
/// a stable r1 with `rollout`.
async fn action_fleet(rollout: &str) -> Harness {
    let mut h = Harness::new().await;
    h.revision("r1", rollout).await;
    h.stable("r1").await;
    for (id, tier) in [("c1", "canary"), ("w1", "web"), ("w2", "web"), ("x", "web")] {
        h.enrol(id, tier).await;
        h.connect(id, Some("r1"));
    }
    h
}

fn action_runs(h: &Harness) -> Vec<(DateTime<Utc>, String, pb::RunAction)> {
    h.sent(|_, m| matches!(m, SMsg::Action(_)))
        .into_iter()
        .filter_map(|(t, host, m)| match m {
            SMsg::Action(a) => Some((t, host, a)),
            _ => None,
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn an_action_rollout_runs_the_action_phase_by_phase_and_promotes_nothing() {
    let h = action_fleet(ACTION_PHASES).await;
    let id = h.start_action(&["c1", "w1", "w2"], false).await.unwrap();
    let (r, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(r.runtime.action.is_some());
    assert_eq!(hosts.len(), 3, "only the targets it was started for");
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));

    let runs = action_runs(&h);
    let order: Vec<&str> = runs.iter().map(|(_, host, _)| host.as_str()).collect();
    assert_eq!(order.len(), 3);
    assert_eq!(order[0], "c1", "the canary phase runs first");
    assert!(
        runs.iter()
            .all(|(_, _, a)| a.module == "common" && a.action == "upgrade")
    );
    // Nothing is planned or applied, and the stable revision is unchanged.
    assert!(
        h.sent(|_, m| matches!(m, SMsg::Plan(_) | SMsg::Apply(_)))
            .is_empty()
    );
    let stable = h
        .store
        .call(|db| db.kv().get("stable_revision"))
        .await
        .unwrap();
    assert_eq!(stable.as_deref(), Some("r1"));
    // Each target's run is the rollout's.
    let rid = id.clone();
    let stored = h
        .store
        .call(move |db| db.runs().ids_for_rollout(&rid))
        .await
        .unwrap();
    assert_eq!(stored.len(), 3);
    // Baseline and bake checks ran around each action.
    for scope in [HealthScope::Baseline, HealthScope::Bake] {
        assert!(
            !h.sent(
                |host, m| host == "w2" && matches!(m, SMsg::Health(x) if x.scope == scope as i32)
            )
            .is_empty()
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_failed_action_pauses_the_rollout_on_its_budget_and_cannot_be_rolled_back() {
    let h = action_fleet(ACTION_PHASES).await;
    h.script("c1", |s| s.action_fail = true);
    let id = h.start_action(&["c1", "w1", "w2"], false).await.unwrap();
    let (r, hosts) = h.wait_state(&id, RolloutState::Paused).await;
    assert_eq!(r.runtime.pause.as_ref().unwrap().kind, "failure_budget");
    let c1 = host(&hosts, "c1");
    assert_eq!(c1.state, HostPhase::Failed);
    assert!(
        c1.runtime
            .error
            .as_deref()
            .unwrap()
            .contains("apt is locked")
    );
    assert_eq!(action_runs(&h).len(), 1, "the web phase never started");
    let refused = h
        .command(Command::Rollback {
            rollout: id.clone(),
            note: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(refused.kind, CommandErrorKind::Conflict);
    // A retry runs the action again.
    h.script("c1", |s| s.action_fail = false);
    h.command(Command::Retry {
        rollout: id.clone(),
        host: "c1".into(),
        workload: String::new(),
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(action_runs(&h).len(), 4);
}

#[rstest::rstest]
#[case::required("approval: required", false)]
#[case::confirmed_action_if_destructive("approval: if_destructive", true)]
#[tokio::test(start_paused = true)]
async fn an_action_rollout_waits_for_one_approval_per_phase(
    #[case] approval: &str,
    #[case] confirm: bool,
) {
    let h = action_fleet(&format!(
        "  phases:\n    - {{ name: web, select: 'true', {approval} }}\n"
    ))
    .await;
    let id = h.start_action(&["w1", "w2"], confirm).await.unwrap();
    let (_, hosts) = h.wait_state(&id, RolloutState::AwaitingApproval).await;
    let digests: std::collections::BTreeSet<_> =
        hosts.iter().filter_map(|x| x.plan_digest.clone()).collect();
    assert_eq!(digests.len(), 1, "one action, one plan to approve");
    assert!(action_runs(&h).is_empty());
    h.command(Command::Approve {
        rollout: id.clone(),
        phase: "web".into(),
        digests: digests.into_iter().collect(),
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(action_runs(&h).len(), 2);
}

#[tokio::test(start_paused = true)]
async fn an_action_that_reboots_bakes_once_the_host_is_back() {
    let mut h =
        action_fleet("  health: { bake: 1m }\n  phases:\n    - { name: web, select: 'true' }\n")
            .await;
    h.script("w1", |s| {
        s.boot_id = Some("boot-1".into());
        s.reboot = Some(reboot(1800));
    });
    let id = h.start_action(&["w1"], false).await.unwrap();
    h.wait(&id, "rebooting", |_, hs| {
        host(hs, "w1").state == HostPhase::Rebooting
    })
    .await;
    h.disconnect("w1");
    h.script("w1", |s| s.boot_id = Some("boot-2".into()));
    h.connect("w1", Some("r1"));
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let w1 = host(&hosts, "w1");
    assert_eq!(w1.state, HostPhase::Healthy);
    assert!(
        w1.runtime
            .reboot
            .as_ref()
            .is_some_and(|w| w.back_at.is_some())
    );
    assert_eq!(action_runs(&h).len(), 1, "the action is not sent again");
}

/// A cluster's action rollout moves the placement of the workload declaring
/// the action, phased by its `workload.*`, through the cluster's executor,
/// one predating placements included (it echoes no workload).
#[rstest::rstest]
#[case::current_executor(false)]
#[case::executor_predating_placements(true)]
#[tokio::test(start_paused = true)]
async fn a_cluster_action_rollout_runs_on_its_workloads_placement(#[case] no_workloads: bool) {
    let mut h = Harness::new().await;
    let phases = "  health: { bake: 30s }\n  phases:\n    - { name: pg, select: 'workload.name == \"postgres\"' }\n";
    h.revision_with("r1", phases, NOMAD_CLUSTER, FLAT).await;
    h.stable("r1").await;
    h.enrol("n1", "nomad").await;
    h.script("n1", |s| s.no_workloads = no_workloads);
    h.connect("n1", Some("r1"));
    let id = h
        .command(Command::StartAction {
            action: RolloutAction {
                module: "postgres".into(),
                action: "restart".into(),
                label: "Restart".into(),
                confirm: false,
                selector: None,
            },
            targets: BTreeMap::from([(CLUSTER.to_string(), "r1".to_string())]),
            note: String::new(),
        })
        .await
        .unwrap();
    let (_, rows) = h.wait_state(&id, RolloutState::Completed).await;
    let placement = rows.iter().find(|x| x.host == CLUSTER).expect("a row");
    assert_eq!(
        (placement.workload.as_str(), placement.runtime.kind),
        ("postgres", TargetKind::Workload)
    );
    assert_eq!(placement.phase, "pg");
    assert_eq!(placement.state, HostPhase::Healthy);
    let runs = action_runs(&h);
    assert_eq!(runs.len(), 1);
    let (_, executor, sent) = &runs[0];
    assert_eq!(executor, "n1");
    assert_eq!(
        (sent.target.as_str(), sent.workload.as_str()),
        (CLUSTER, "postgres")
    );
}

#[rstest::rstest]
#[case::another_rollout_under_way(true)]
#[case::no_targets(false)]
#[tokio::test(start_paused = true)]
async fn an_action_rollout_is_refused(#[case] busy: bool) {
    let h = action_fleet(ACTION_PHASES).await;
    if busy {
        h.script("x", |s| s.silent_apply = true);
        h.revision("r2", ACTION_PHASES).await;
        h.start("r2").await;
    }
    let targets: &[&str] = if busy { &["w1"] } else { &[] };
    let refused = h.start_action(targets, false).await.unwrap_err();
    assert_eq!(
        refused.kind,
        if busy {
            CommandErrorKind::Conflict
        } else {
            CommandErrorKind::Invalid
        }
    );
    assert!(action_runs(&h).is_empty());
}

/// A target whose plan changes nothing is neither checked before its run
/// nor gated after it: its health is not the rollout's doing.
#[tokio::test(start_paused = true)]
async fn an_unchanged_host_is_not_held_by_its_baseline_or_gate() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        TWO_PHASES.replace(", approval: required", "").as_str(),
    )
    .await;
    h.enrol("c1", "canary").await;
    h.enrol("w1", "web").await;
    h.script("c1", |s| {
        s.unchanged = true;
        s.baseline_fail = true;
        s.gate_fail = true;
    });
    h.connect("c1", None);
    h.connect("w1", None);
    let id = h.start("r1").await;
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert!(hosts.iter().all(|x| x.state == HostPhase::Healthy));
    let checked = h.sent(|x, m| {
        x == "c1"
            && matches!(m, SMsg::Health(r) if r.scope == HealthScope::Baseline as i32
                || r.scope == HealthScope::Rollout as i32)
    });
    assert!(checked.is_empty(), "{checked:?}");
}

/// Targets with nothing to change run alongside a phase's waves without
/// taking a slot of its `parallel`.
#[tokio::test(start_paused = true)]
async fn unchanged_targets_start_past_the_parallel_cap() {
    let mut h = Harness::new().await;
    h.revision(
        "r1",
        "  health: { bake: 1m }\n  phases:\n    - { name: web, select: 'host.groups contains \"web\"', parallel: 1 }\n",
    )
    .await;
    for (w, unchanged) in [("w1", true), ("w2", false), ("w3", true), ("w4", false)] {
        h.enrol(w, "web").await;
        h.script(w, |s| {
            s.unchanged = unchanged;
            s.silent_apply = true;
        });
        h.connect(w, None);
    }
    let id = h.start("r1").await;
    let applying = |hs: &[RolloutHostRecord]| {
        hs.iter()
            .filter(|x| x.state == HostPhase::Applying)
            .map(|x| x.host.clone())
            .collect::<std::collections::BTreeSet<_>>()
    };
    h.wait(&id, "three applying", |_, hs| applying(hs).len() == 3)
        .await;
    h.idle(60).await;
    let (_, hosts) = h.snapshot(&id).await;
    let running = applying(&hosts);
    assert!(
        running.contains("w1") && running.contains("w3") && running.len() == 3,
        "both unchanged hosts and one changed one: {running:?}"
    );
}

/// A cluster is gated only on the workloads this rollout changes: one
/// already failing that it leaves alone (or a cluster it changes nothing
/// on) does not hold it.
#[rstest::rstest]
#[case::unrelated_workload(vec!["postgres"], "other", false)]
#[case::changed_workload(vec!["postgres"], "postgres", true)]
#[case::nothing_changed(vec![], "postgres", false)]
#[tokio::test(start_paused = true)]
async fn a_cluster_is_gated_on_the_workloads_it_changes(
    #[case] changes: Vec<&str>,
    #[case] failing: &str,
    #[case] held: bool,
) {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, FLAT)
        .await;
    h.enrol("n1", "nomad").await;
    h.script("n1", |s| {
        s.target_changes = Some(changes.iter().map(|c| c.to_string()).collect());
        s.failing_workload = Some(failing.into());
    });
    h.connect("n1", None);
    let id = h.start("r1").await;
    let (_, hosts) = h
        .wait(&id, "the cluster settles", |r, hs| {
            r.state == RolloutState::Completed || host(hs, CLUSTER).state == HostPhase::Held
        })
        .await;
    let state = host(&hosts, CLUSTER).state;
    if held {
        assert_eq!(state, HostPhase::Held);
        assert!(
            host(&hosts, CLUSTER)
                .runtime
                .error
                .as_deref()
                .is_some_and(|e| e.contains(failing)),
            "{:?}",
            host(&hosts, CLUSTER).runtime.error
        );
    } else {
        assert_eq!(state, HostPhase::Healthy);
    }
}

// ---------------------------------------------------------------- placements

/// `postgres` and `app`, which needs it: two placements on the cluster.
const TWO_WORKLOADS: &[(&str, &str)] = &[
    (
        "workloads/postgres/workload.yaml",
        "name: postgres\nresources: [postgres.nomad.hcl]\n",
    ),
    (
        "workloads/postgres/files/postgres.nomad.hcl",
        "job \"postgres\" {}\n",
    ),
    (
        "workloads/app/workload.yaml",
        "name: app\nneeds: [postgres]\nresources: [app.nomad.hcl]\n",
    ),
    ("workloads/app/files/app.nomad.hcl", "job \"app\" {}\n"),
];

/// `app` labelled `tier: canary`, with or without a need on `postgres`.
fn labelled(needs_postgres: bool) -> Vec<(&'static str, String)> {
    let needs = if needs_postgres {
        "needs: [postgres]\n"
    } else {
        ""
    };
    vec![
        (
            "workloads/postgres/workload.yaml",
            "name: postgres\nresources: [postgres.nomad.hcl]\n".to_owned(),
        ),
        (
            "workloads/postgres/files/postgres.nomad.hcl",
            "job \"postgres\" {}\n".to_owned(),
        ),
        (
            "workloads/app/workload.yaml",
            format!("name: app\nlabels: {{ tier: canary }}\n{needs}resources: [app.nomad.hcl]\n"),
        ),
        (
            "workloads/app/files/app.nomad.hcl",
            "job \"app\" {}\n".to_owned(),
        ),
    ]
}

/// A canary phase of the placements labelled `tier: canary`, then everything.
const CANARY_WORKLOADS: &str = "  health: { bake: 30s }\n  phases:\n    - { name: canary, select: 'workload.labels.tier == \"canary\"', parallel: 4 }\n    - { name: all, select: 'true', parallel: 4 }\n";

fn placement<'a>(
    hosts: &'a [RolloutHostRecord],
    cluster: &str,
    workload: &str,
) -> &'a RolloutHostRecord {
    hosts
        .iter()
        .find(|h| h.host == cluster && h.workload == workload)
        .unwrap_or_else(|| panic!("placement {cluster}/{workload} in rollout: {hosts:#?}"))
}

/// The applies aimed at `cluster`, in order, as (executor, workload).
fn cluster_applies(h: &Harness, cluster: &str) -> Vec<(String, String)> {
    h.applies()
        .into_iter()
        .filter(|a| a.2.target == cluster)
        .map(|a| (a.1, a.2.workload))
        .collect()
}

async fn applied_revision(h: &Harness, target: &str) -> Option<String> {
    let t = target.to_owned();
    h.store
        .call(move |db| Ok(db.hosts().target(&t)?.and_then(|r| r.applied_revision)))
        .await
        .unwrap()
}

/// A cluster is rolled one workload placement at a time: a row per workload
/// (`cluster/workload`), staged by the `needs` between them, each planned,
/// applied and baked on its own through the cluster's executor. The cluster
/// is on the revision once every placement is.
#[tokio::test(start_paused = true)]
async fn a_cluster_is_rolled_by_placements_staged_by_their_needs() {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, TWO_WORKLOADS)
        .await;
    h.enrol("n1", "nomad").await;
    h.connect("n1", None);
    let id = h.start("r1").await;
    let (r, hosts) = h
        .wait(&id, "the freeze", |_, hs| {
            hs.iter().filter(|x| x.host == CLUSTER).count() == 2
        })
        .await;
    assert_eq!(r.runtime.phases[0].stages, 2);
    let postgres = placement(&hosts, CLUSTER, "postgres");
    let app = placement(&hosts, CLUSTER, "app");
    assert_eq!(postgres.runtime.kind, TargetKind::Workload);
    assert_eq!((postgres.runtime.stage, app.runtime.stage), (0, 1));
    assert!(!app.runtime.removal);

    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    for w in ["postgres", "app"] {
        let p = placement(&hosts, CLUSTER, w);
        assert_eq!(p.state, HostPhase::Healthy, "{w}");
        assert_eq!(p.runtime.executor.as_deref(), Some("n1"), "{w}");
        assert!(p.plan_digest.is_some(), "{w} has a plan of its own");
    }
    assert_eq!(
        cluster_applies(&h, CLUSTER),
        vec![
            ("n1".to_owned(), "postgres".to_owned()),
            ("n1".to_owned(), "app".to_owned()),
        ],
        "one apply per placement, the needed one first"
    );
    // Every request for a placement names its workload (health included).
    let unscoped = h.sent(|_, m| {
        targeted(m) == Some(CLUSTER)
            && match m {
                SMsg::Plan(p) => p.workload.is_empty(),
                SMsg::Apply(a) => a.workload.is_empty(),
                SMsg::Health(x) => x.workload.is_empty(),
                _ => false,
            }
    });
    assert!(unscoped.is_empty(), "{unscoped:?}");
    assert_eq!(applied_revision(&h, CLUSTER).await.as_deref(), Some("r1"));
    let actions = h.audit_actions(&id).await;
    assert!(
        actions.iter().any(|a| a == "rollout.target.converged"),
        "{actions:?}"
    );
}

/// A placement belongs to the first phase whose selector matches its
/// workload's labels (`workload.*`) and its cluster's; the hosts and the
/// cluster's other workloads take their own phases.
#[tokio::test(start_paused = true)]
async fn placements_join_the_phase_their_labels_select() {
    let mut h = Harness::new().await;
    let files = labelled(false);
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    h.revision_with("r1", CANARY_WORKLOADS, NOMAD_CLUSTER, &files)
        .await;
    h.enrol("n1", "nomad").await;
    h.connect("n1", None);
    let id = h.start("r1").await;
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(placement(&hosts, CLUSTER, "app").phase, "canary");
    assert_eq!(placement(&hosts, CLUSTER, "postgres").phase, "all");
    assert_eq!(host(&hosts, "n1").phase, "all");
    let applies = cluster_applies(&h, CLUSTER);
    assert_eq!(applies.len(), 2, "{applies:?}");
    assert_eq!(applies[0].1, "app", "the canary phase goes first");
    assert_eq!(applied_revision(&h, CLUSTER).await.as_deref(), Some("r1"));
}

/// A placement whose workload needs one that is neither applied by this
/// rollout yet nor running on the cluster waits: under `warn` it misses its
/// phase and converges in the stragglers stage once the need is healthy;
/// under `refuse` it fails.
#[rstest::rstest]
#[case::warn(false)]
#[case::refuse(true)]
#[tokio::test(start_paused = true)]
async fn a_placement_waits_for_a_need_the_cluster_is_not_running_yet(#[case] refuse: bool) {
    let mut h = Harness::new().await;
    let policy = if refuse {
        format!("{CANARY_WORKLOADS}  dependencies: {{ across_phases: refuse }}\n")
    } else {
        CANARY_WORKLOADS.to_owned()
    };
    let files = labelled(true);
    let files: Vec<(&str, &str)> = files.iter().map(|(p, c)| (*p, c.as_str())).collect();
    h.revision_with("r1", &policy, NOMAD_CLUSTER, &files).await;
    h.enrol("n1", "nomad").await;
    h.connect("n1", None);
    let id = h.start("r1").await;
    let waited = if refuse {
        HostPhase::Failed
    } else {
        HostPhase::Missed
    };
    let (_, hosts) = h
        .wait(&id, "app waits for postgres", |_, hs| {
            placement(hs, CLUSTER, "app").state == waited
        })
        .await;
    let app = placement(&hosts, CLUSTER, "app");
    assert!(
        app.runtime
            .error
            .as_deref()
            .is_some_and(|e| e.contains("postgres")),
        "{:?}",
        app.runtime.error
    );
    assert!(
        cluster_applies(&h, CLUSTER).iter().all(|a| a.1 != "app"),
        "app is not applied before the need"
    );
    if refuse {
        let (r, _) = h.wait_state(&id, RolloutState::Paused).await;
        assert_eq!(
            r.runtime.pause.as_ref().map(|p| p.kind.as_str()),
            Some("failure_budget")
        );
        return;
    }
    let (r, hosts) = h
        .wait(&id, "app healthy in the stragglers stage", |_, hs| {
            placement(hs, CLUSTER, "app").state == HostPhase::Healthy
        })
        .await;
    assert_eq!(r.state, RolloutState::Completed);
    assert_eq!(placement(&hosts, CLUSTER, "app").phase, STRAGGLERS);
    assert_eq!(
        placement(&hosts, CLUSTER, "postgres").state,
        HostPhase::Healthy
    );
    let applies = cluster_applies(&h, CLUSTER);
    assert_eq!(
        applies.iter().map(|a| a.1.as_str()).collect::<Vec<_>>(),
        ["postgres", "app"],
        "{applies:?}"
    );
    assert_eq!(applied_revision(&h, CLUSTER).await.as_deref(), Some("r1"));
}

/// A workload the cluster's applied revision ran that the new one no longer
/// selects is a removal placement: it is planned (its objects' removal) after
/// the placements that needed it, by the applied revision's `needs`.
#[tokio::test(start_paused = true)]
async fn a_removed_workload_is_a_placement_after_the_ones_that_needed_it() {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, TWO_WORKLOADS)
        .await;
    h.enrol("n1", "nomad").await;
    h.connect("n1", None);
    let first = h.start("r1").await;
    h.wait_state(&first, RolloutState::Completed).await;
    assert_eq!(applied_revision(&h, CLUSTER).await.as_deref(), Some("r1"));

    // r2 drops both workloads.
    h.revision_with("r2", ONE_PHASE_ALL, NOMAD_CLUSTER, &[])
        .await;
    let id = h.start("r2").await;
    let (r, hosts) = h
        .wait(&id, "the freeze", |_, hs| {
            hs.iter().filter(|x| x.host == CLUSTER).count() == 2
        })
        .await;
    assert_eq!(r.runtime.phases[0].stages, 2);
    let postgres = placement(&hosts, CLUSTER, "postgres");
    let app = placement(&hosts, CLUSTER, "app");
    assert!(postgres.runtime.removal && app.runtime.removal);
    assert_eq!(postgres.runtime.after, vec!["app".to_owned()]);
    assert!(app.runtime.after.is_empty());
    assert_eq!((app.runtime.stage, postgres.runtime.stage), (0, 1));

    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    assert_eq!(placement(&hosts, CLUSTER, "app").state, HostPhase::Healthy);
    assert_eq!(
        placement(&hosts, CLUSTER, "postgres").state,
        HostPhase::Healthy
    );
    let applies: Vec<(String, String)> = h
        .applies()
        .into_iter()
        .filter(|a| a.2.target == CLUSTER && a.2.revision == "r2")
        .map(|a| (a.1, a.2.workload))
        .collect();
    assert_eq!(
        applies.iter().map(|a| a.1.as_str()).collect::<Vec<_>>(),
        ["app", "postgres"],
        "the dependent is removed first: {applies:?}"
    );
    assert_eq!(applied_revision(&h, CLUSTER).await.as_deref(), Some("r2"));
}

/// While an executor that predates placements (no `workloads` capability)
/// is connected for a cluster, the cluster is rolled as one row, as before.
#[tokio::test(start_paused = true)]
async fn an_older_executor_rolls_the_cluster_as_one_row() {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, TWO_WORKLOADS)
        .await;
    h.enrol("n1", "nomad").await;
    h.script("n1", |s| s.no_workloads = true);
    h.connect("n1", None);
    let id = h.start("r1").await;
    let (_, hosts) = h.wait_state(&id, RolloutState::Completed).await;
    let rows: Vec<&RolloutHostRecord> = hosts.iter().filter(|x| x.host == CLUSTER).collect();
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0].runtime.kind, TargetKind::Cluster);
    assert_eq!(rows[0].workload, "");
    assert_eq!(rows[0].state, HostPhase::Healthy);
    assert_eq!(
        cluster_applies(&h, CLUSTER),
        vec![("n1".to_owned(), String::new())]
    );
    // The whole cluster's result names the applied revision itself (the
    // server records it on arrival, outside this harness).
}

// ---------------------------------------------------------------- drift checks

fn drift_checks(h: &Harness) -> Vec<(String, pb::CheckDrift)> {
    h.sent(|_, m| matches!(m, SMsg::CheckDrift(_)))
        .into_iter()
        .filter_map(|(_, to, m)| match m {
            SMsg::CheckDrift(c) => Some((to, c)),
            _ => None,
        })
        .collect()
}

async fn set_applied(h: &Harness, target: &str) {
    let t = target.to_owned();
    h.store
        .call(move |db| db.hosts().set_applied_revision(&t, "r1"))
        .await
        .unwrap();
}

fn check(target: &str, workload: Option<&str>) -> Command {
    Command::CheckDrift {
        target: target.into(),
        workload: workload.map(str::to_owned),
        note: String::new(),
    }
}

#[tokio::test(start_paused = true)]
async fn a_drift_check_reaches_the_hosts_own_agent() {
    let mut h = Harness::new().await;
    h.revision("r1", ONE_PHASE_ALL).await;
    h.enrol("a", "web").await;
    h.connect("a", Some("r1"));
    set_applied(&h, "a").await;
    h.command(check("a", None)).await.expect("asked");
    h.idle(1).await;
    let sent = drift_checks(&h);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, "a");
    assert!(sent[0].1.target.is_empty(), "about the host itself");
}

#[rstest::rstest]
#[case::nothing_applied("applied", CommandErrorKind::Conflict)]
#[case::offline("offline", CommandErrorKind::Conflict)]
#[case::old_agent("legacy", CommandErrorKind::Conflict)]
#[case::workload_of_a_host("workload", CommandErrorKind::Invalid)]
#[case::unknown("unknown", CommandErrorKind::NotFound)]
#[tokio::test(start_paused = true)]
async fn a_drift_check_is_refused_when_it_cannot_run(
    #[case] why: &str,
    #[case] kind: CommandErrorKind,
) {
    let mut h = Harness::new().await;
    h.revision("r1", ONE_PHASE_ALL).await;
    h.enrol("a", "web").await;
    if why == "legacy" {
        h.script("a", |s| s.legacy_agent = true);
    }
    if why != "offline" {
        h.connect("a", Some("r1"));
    }
    if why != "applied" {
        set_applied(&h, "a").await;
    }
    let (target, workload) = match why {
        "unknown" => ("nope", None),
        "workload" => ("a", Some("postgres")),
        _ => ("a", None),
    };
    let refused = h
        .command(check(target, workload))
        .await
        .expect_err("refused");
    assert_eq!(refused.kind, kind, "{refused:?}");
    h.idle(1).await;
    assert!(drift_checks(&h).is_empty());
}

#[rstest::rstest]
#[case::whole_cluster(None)]
#[case::one_placement(Some("postgres"))]
#[tokio::test(start_paused = true)]
async fn a_clusters_drift_check_goes_to_its_steward(#[case] workload: Option<&str>) {
    let mut h = Harness::new().await;
    h.revision_with("r1", ONE_PHASE_ALL, NOMAD_CLUSTER, FLAT)
        .await;
    for n in ["n1", "n2"] {
        h.enrol(n, "nomad").await;
        h.connect(n, None);
    }
    let id = h.start("r1").await;
    h.wait_state(&id, RolloutState::Completed).await;
    set_applied(&h, CLUSTER).await;
    let steward = h.steward(CLUSTER).await.expect("leased");
    h.command(check(CLUSTER, workload)).await.expect("asked");
    h.idle(1).await;
    let sent = drift_checks(&h);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, steward);
    assert_eq!(sent[0].1.target, CLUSTER);
    assert_eq!(sent[0].1.workload, workload.unwrap_or_default());
}
