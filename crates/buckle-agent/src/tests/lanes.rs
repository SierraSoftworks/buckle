//! Cluster lanes: a leased cluster is watched (health on change, drift every
//! interval, reachability) beside the host's own work, and released cleanly.

use super::fake_server::ServerSession;
use super::*;
use crate::tests::{apply, apply_result, health_report, plan_result, welcome};
use buckle_proto::v1 as pb;
use pb::agent_message::Msg as AMsg;
use pb::server_message::Msg as SMsg;
use std::time::Duration;

const NOMAD: &str = "home-nomad";
const KUBE: &str = "home-k8s";
const NOMAD_FLEET: &str = "version: 2\nfleet:\n  clusters:\n    home-nomad: { kind: nomad }\n";
const KUBE_FLEET: &str = "version: 2\nfleet:\n  clusters:\n    home-k8s: { kind: kubernetes }\n";
const PASTE_JOB: &str = r#"{"ID": "paste", "Type": "service", "TaskGroups": [{"Name": "api", "Count": 1, "Tasks": [{"Name": "paste"}]}]}"#;
const GRAFANA: &str = r#"apiVersion: apps/v1
kind: Deployment
metadata: { name: grafana }
spec:
  selector: { matchLabels: { app: grafana } }
  template:
    metadata: { labels: { app: grafana } }
    spec:
      containers:
        - { name: grafana, image: "grafana:{{ vars.version }}" }
"#;

fn cluster(kind: &str) -> pb::ClusterTarget {
    pb::ClusterTarget {
        kind: kind.into(),
        identity: "buckle_jwt".into(),
        fingerprint_key: vec![7; 32],
        ..Default::default()
    }
}

fn targeted_assign(target: &str, kind: &str, revision: &str, digest: &str) -> SMsg {
    SMsg::Assign(pb::Assign {
        revision: revision.into(),
        tree_digest: digest.into(),
        target: target.into(),
        cluster: Some(cluster(kind)),
        ..Default::default()
    })
}

fn lease(target: &str, kind: &str, applied: &str, health: u64, drift: u64) -> SMsg {
    SMsg::Lease(pb::Lease {
        target: target.into(),
        cluster: Some(cluster(kind)),
        applied_revision: applied.into(),
        watch: Some(pb::Watch {
            health: Some(prost_types::Duration {
                seconds: health as i64,
                nanos: 0,
            }),
            drift: Some(prost_types::Duration {
                seconds: drift as i64,
                nanos: 0,
            }),
        }),
        until: None,
    })
}

fn release(target: &str) -> SMsg {
    SMsg::Release(pb::Release {
        target: target.into(),
        reason: "another host stewards the cluster".into(),
    })
}

fn target_status(msg: &AMsg) -> Option<pb::TargetStatus> {
    match msg {
        AMsg::TargetStatus(s) => Some(s.clone()),
        _ => None,
    }
}

fn drift(msg: &AMsg) -> Option<pb::Drift> {
    match msg {
        AMsg::Drift(d) => Some(d.clone()),
        _ => None,
    }
}

fn is_heartbeat(msg: &AMsg) -> bool {
    matches!(msg, AMsg::Heartbeat(_))
}

/// Plans and applies `revision` to `target` over `session`, as a rollout
/// would, and returns the bundle digest.
async fn apply_cluster(
    h: &Harness,
    session: &mut ServerSession,
    target: &str,
    kind: &str,
    revision: &str,
    files: &[(&str, &str)],
) -> String {
    let digest = h.fleet.add_revision(revision, bundle_files(files));
    session
        .send(targeted_assign(target, kind, revision, &digest))
        .await;
    session
        .send(SMsg::Plan(pb::RequestPlan {
            target: target.into(),
            workload: String::new(),
            revision: revision.into(),
            select: String::new(),
        }))
        .await;
    let planned = session.expect(plan_result).await;
    assert_eq!(
        (planned.target.as_str(), planned.error.as_str()),
        (target, "")
    );
    session
        .send(targeted_assign(target, kind, revision, &digest))
        .await;
    session
        .send(SMsg::Apply(pb::Apply {
            target: target.into(),
            approval_digest: planned.approval_digest.clone(),
            ..apply(revision, &planned.digest)
        }))
        .await;
    let result = session.expect(apply_result).await;
    assert!(result.succeeded, "{result:?}");
    digest
}

fn paste_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("buckle.yaml", NOMAD_FLEET),
        (
            "workloads/paste/workload.yaml",
            "name: paste\nresources: [paste.nomad.hcl]\n",
        ),
        ("workloads/paste/files/paste.nomad.hcl", PASTE_JOB),
    ]
}

fn grafana_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("buckle.yaml", KUBE_FLEET),
        (
            "workloads/grafana/workload.yaml",
            "name: grafana\nnamespace: monitoring\nvars: { version: \"11.3.1\" }\nresources:\n  - grafana.yaml.j2\n",
        ),
        ("workloads/grafana/files/grafana.yaml.j2", GRAFANA),
    ]
}

/// The first health report after `since` came from the watch waking on a
/// change, not from the health timer.
fn woke_early(since: tokio::time::Instant, timer: Duration) {
    let elapsed = since.elapsed();
    assert!(
        elapsed < timer,
        "the watch did not wake on the change: read after {elapsed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_leased_nomad_cluster_reports_health_when_its_jobs_change_and_stops_when_released() {
    let nomad = buckle_nomad::testing::FakeNomad::new();
    let mut h = Harness::start(Setup {
        cluster: Some(nomad.clone()),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    apply_cluster(&h, &mut session, NOMAD, "nomad", "rev1", &paste_files()).await;

    // The lease starts the watch: the cluster answers, and its applied
    // revision's health is read once, as a drift-scope report.
    session.send(lease(NOMAD, "nomad", "rev1", 60, 600)).await;
    let seen = session.collect_until(|m| health_report(m).is_some()).await;
    let status = seen
        .iter()
        .find_map(|(_, m)| target_status(m))
        .expect("reachability is reported with the lease");
    assert_eq!((status.target.as_str(), status.reachable), (NOMAD, true));
    let report = seen.iter().find_map(|(_, m)| health_report(m)).unwrap();
    assert_eq!(report.target, NOMAD);
    assert_eq!(report.scope, pb::HealthScope::Drift as i32);
    assert_eq!(report.revision, "rev1");
    assert!(report.request_id.is_empty(), "unsolicited: {report:?}");
    assert!(
        !report.results.is_empty() && report.results.iter().all(|r| r.passed),
        "{report:?}"
    );

    // A write to the cluster ends the long poll: health is read again
    // once it settles, well before the health timer.
    let since = tokio::time::Instant::now();
    nomad.deployment_outcome("failed");
    let report = session.expect(health_report).await;
    assert_eq!(report.target, NOMAD);
    woke_early(since, Duration::from_secs(60));

    // Released: the lane closes and the cluster is no longer read.
    session.send(release(NOMAD)).await;
    nomad.deployment_outcome("successful");
    let quiet = session.collect_until(is_heartbeat).await;
    let quiet = session
        .collect_until(is_heartbeat)
        .await
        .into_iter()
        .chain(quiet);
    assert!(
        quiet
            .clone()
            .all(|(_, m)| health_report(&m).is_none() && target_status(&m).is_none()),
        "{:?}",
        quiet.map(|(_, m)| m).collect::<Vec<_>>()
    );
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_lease_without_an_applied_revision_probes_the_cluster() {
    let nomad = buckle_nomad::testing::FakeNomad::new();
    let mut h = Harness::start(Setup {
        cluster: Some(nomad.clone()),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    session.send(lease(NOMAD, "nomad", "", 60, 600)).await;
    let status = session.expect(target_status).await;
    assert_eq!((status.target.as_str(), status.reachable), (NOMAD, true));
    assert!(
        nomad
            .requests()
            .iter()
            .any(|(m, p)| m == "GET" && p == "/v1/status/leader"),
        "{:?}",
        nomad.requests()
    );
    // Nothing applied: no health to report before the heartbeat.
    let until_beat = session.collect_until(is_heartbeat).await;
    assert!(until_beat.iter().all(|(_, m)| health_report(m).is_none()));
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn a_leased_kubernetes_cluster_wakes_on_its_watch_stream() {
    let kube = buckle_kube::testing::FakeKube::new();
    let mut h = Harness::start(Setup {
        kube: Some(kube.clone()),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    apply_cluster(
        &h,
        &mut session,
        KUBE,
        "kubernetes",
        "rev1",
        &grafana_files(),
    )
    .await;

    session
        .send(lease(KUBE, "kubernetes", "rev1", 60, 600))
        .await;
    let seen = session.collect_until(|m| health_report(m).is_some()).await;
    assert!(
        seen.iter()
            .find_map(|(_, m)| target_status(m))
            .is_some_and(|s| s.reachable),
        "{seen:?}"
    );
    let report = seen.iter().find_map(|(_, m)| health_report(m)).unwrap();
    assert_eq!(
        (report.target.as_str(), report.revision.as_str()),
        (KUBE, "rev1")
    );

    // An out-of-band edit arrives on the watch stream: health is read
    // again once it settles, well before the health timer.
    let since = tokio::time::Instant::now();
    kube.edit(
        "apps/v1",
        "Deployment",
        "monitoring",
        "grafana",
        "kubectl",
        serde_json::json!({"spec": {"replicas": 2}}),
    );
    let report = session.expect(health_report).await;
    assert_eq!(report.target, KUBE);
    woke_early(since, Duration::from_secs(60));
    h.stop().await.expect("clean exit");
}

#[tokio::test(start_paused = true)]
async fn drift_is_planned_every_interval_and_reported_only_as_a_report() {
    let kube = buckle_kube::testing::FakeKube::new();
    let mut h = Harness::start(Setup {
        kube: Some(kube.clone()),
        ..Default::default()
    })
    .await;
    let mut session = h.sessions.accept().await;
    session.send(welcome(None, None)).await;
    apply_cluster(
        &h,
        &mut session,
        KUBE,
        "kubernetes",
        "rev1",
        &grafana_files(),
    )
    .await;

    session
        .send(lease(KUBE, "kubernetes", "rev1", 600, 5))
        .await;
    let first = session.expect(drift).await;
    assert_eq!(
        (first.target.as_str(), first.revision.as_str()),
        (KUBE, "rev1")
    );
    assert_eq!((first.changes, first.plan_digest.as_str()), (0, ""));

    // Someone changes the image under us: the next pass reports it, and
    // corrects nothing.
    kube.edit(
        "apps/v1",
        "Deployment",
        "monitoring",
        "grafana",
        "kubectl",
        serde_json::json!({"spec": {"template": {"spec": {"containers": [{"name": "grafana", "image": "grafana:0.0.1"}]}}}}),
    );
    let seen = session
        .collect_until(|m| drift(m).is_some_and(|d| d.changes > 0))
        .await;
    let drifted = seen
        .iter()
        .filter_map(|(_, m)| drift(m))
        .find(|d| d.changes > 0)
        .unwrap();
    assert!(!drifted.corrected);
    assert!(!drifted.plan_digest.is_empty() && !drifted.plan_json.is_empty());
    let image = kube
        .object("apps/v1", "Deployment", "monitoring", "grafana")
        .unwrap()["spec"]["template"]["spec"]["containers"][0]["image"]
        .clone();
    assert_eq!(
        image, "grafana:0.0.1",
        "report-only: the cluster was not touched"
    );
    h.stop().await.expect("clean exit");
}
