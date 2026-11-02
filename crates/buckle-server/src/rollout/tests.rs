//! Controller scenarios driven by scripted fake hosts on tokio's paused clock.

use super::model::{HostPhase, RolloutHostRecord, RolloutRecord, STRAGGLERS};
use super::*;
use crate::clock::{Clock, TokioClock};
use crate::events::EventBus;
use crate::secrets::ServerSecrets;
use crate::source::cache::RevisionCache;
use crate::store::{NewHost, RevisionRecord, Store};
use buckle_api::{HostStatus, RevisionStatus, RolloutState};
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
    gate_fail: bool,
    silent_apply: bool,
    /// On the next Apply, answer as the agent does: a PlanResult whose error
    /// starts with "stale plan" carrying this fresh digest.
    stale_plan_result: Option<String>,
    local_rollback_on_bake: bool,
}

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

fn fleet_yaml(rollout: &str) -> String {
    format!(
        "version: 2\nfleet:\n  groups:\n    canary: 'host.labels.tier == \"canary\"'\n    web: 'host.labels.tier == \"web\"'\n    db: 'host.labels.tier == \"db\"'\nrollout:\n{rollout}"
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
        let dir = self.revisions.dir(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("buckle.yaml"), fleet_yaml(rollout)).unwrap();
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

    fn script(&self, host: &str, f: impl FnOnce(&mut Script)) {
        f(&mut self.scripts[host].lock().unwrap());
    }

    fn connect(&mut self, host: &str, applied: Option<&str>) {
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
                    SMsg::Plan(p) => reply(AMsg::Plan(PlanResult {
                        revision: p.revision,
                        digest: s.digest.clone(),
                        changes: 1,
                        destructive: s.destructive,
                        removals: u32::from(s.destructive),
                        plan_json: b"{}".to_vec(),
                        ..Default::default()
                    })),
                    SMsg::Health(h) => {
                        let scope = HealthScope::try_from(h.scope).unwrap();
                        let fail = match scope {
                            HealthScope::Baseline => s.baseline_fail,
                            HealthScope::Bake => s.bake_fail,
                            HealthScope::Rollout => s.gate_fail,
                            _ => false,
                        };
                        if scope == HealthScope::Bake && s.local_rollback_on_bake {
                            reply(AMsg::Rollback(LocalRollback {
                                from_revision: String::new(),
                                to_revision: "stable".into(),
                                reason: "server unreachable and health failing".into(),
                                ..Default::default()
                            }));
                            continue;
                        }
                        reply(AMsg::Health(HealthReport {
                            scope: h.scope,
                            request_id: h.request_id,
                            revision: String::new(),
                            results: vec![pb::HealthResult {
                                module: "web".into(),
                                check: "http".into(),
                                kind: "http".into(),
                                required: true,
                                passed: !fail,
                                reason: if fail { "503".into() } else { String::new() },
                                ..Default::default()
                            }],
                        }));
                    }
                    SMsg::Apply(a) => {
                        if s.silent_apply {
                            continue;
                        }
                        if let Some(fresh) = s.stale_plan_result.clone() {
                            {
                                let mut sc = script.lock().unwrap();
                                sc.stale_plan_result = None;
                                sc.digest = fresh.clone();
                            }
                            reply(AMsg::Plan(PlanResult {
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
                        let failed = s.apply_fail.clone();
                        reply(AMsg::Result(ApplyResult {
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
                            ..Default::default()
                        }));
                    }
                    SMsg::Rollback(r) => reply(AMsg::Result(ApplyResult {
                        revision: r.revision.clone(),
                        succeeded: true,
                        applied_revision: r.revision,
                        ..Default::default()
                    })),
                    _ => {}
                }
            }
        });
        self.handle.connected(Connected {
            host: host.to_owned(),
            applied_revision: applied.map(str::to_owned),
            last_plan_digest: None,
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
}

fn host<'a>(hosts: &'a [RolloutHostRecord], id: &str) -> &'a RolloutHostRecord {
    hosts
        .iter()
        .find(|h| h.host == id)
        .expect("host in rollout")
}

const TWO_PHASES: &str = "  settle: 2m\n  health: { bake: 1m }\n  phases:\n    - { name: canary, select: 'host.groups contains \"canary\"', approval: required }\n    - { name: web, select: 'host.groups contains \"web\"', parallel: 2 }\n";

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

#[tokio::test(start_paused = true)]
async fn supersede_queue_waits_for_the_current_rollout() {
    let mut h = Harness::new().await;
    let gated = "  supersede: queue\n  health: { bake: 30s }\n  phases:\n    - { name: web, select: 'true', approval: required }\n";
    h.revision("r1", gated).await;
    h.revision("r2", &gated.replace(", approval: required", ""))
        .await;
    h.enrol("a", "web").await;
    h.connect("a", None);
    let first = h.start("r1").await;
    h.wait_state(&first, RolloutState::AwaitingApproval).await;
    let second = h.start("r2").await;
    let (r2, _) = h.snapshot(&second).await;
    assert_eq!(r2.state, RolloutState::Detected, "queued");
    h.command(Command::Approve {
        rollout: first.clone(),
        phase: "web".into(),
        digests: vec!["plan-a".into()],
        note: String::new(),
    })
    .await
    .unwrap();
    h.wait_state(&first, RolloutState::Completed).await;
    h.wait_state(&second, RolloutState::Completed).await;
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
        "rollout.host.planned",
        "rollout.host.applying",
        "rollout.host.healthy",
        "rollout.completed",
        "revision.promoted",
    ] {
        assert!(
            actions.contains(&expected),
            "missing {expected} in {actions:?}"
        );
    }
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
