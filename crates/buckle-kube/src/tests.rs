//! Workload scenarios against the in-memory Kubernetes API: plan, apply,
//! re-plan, drift between plan and apply, ownership, secrets, orphans,
//! removal, rollouts and health.

use crate::primitives::*;
use crate::testing::FakeKube;
use crate::{Workload, actions, health, index, object};
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::{InstallKey, Rendered, Secret, SecretStore};
use buckle_core::selectors::{HostContext, HostIdentity};
use buckle_core::template::Templater;
use pretty_assertions::assert_eq;
use rstest::rstest;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

const SECRET: &str = "hunter2-SENTINEL";

fn cx(fake: &FakeKube, module: &str) -> Context {
    let mut cx = Context::new(
        fake.client(),
        "lab-k3s",
        module,
        "rev-1",
        Arc::new(InstallKey::from_bytes([9; 32])),
    )
    .with_namespace(Some("monitoring"));
    cx.poll = Duration::from_millis(1);
    cx.rollout_timeout = Duration::from_millis(50);
    cx.placement_timeout = Duration::from_millis(10);
    cx
}

fn render(template: &str) -> Rendered {
    let mut store = SecretStore::new();
    store.insert(Secret::new(
        "db",
        SECRET.into(),
        &InstallKey::from_bytes([1; 32]),
    ));
    let host = HostContext::new(HostIdentity {
        id: "lab-k3s".into(),
        ..Default::default()
    })
    .with_secrets(Arc::new(store));
    Templater::new().render_sensitive(template, &host).unwrap()
}

fn deployment(image: &str) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "grafana"},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app": "grafana"}},
            "template": {
                "metadata": {"labels": {"app": "grafana"}},
                "spec": {"containers": [{"name": "grafana", "image": image}]}
            }
        }
    })
}

fn manifest(obj: Value) -> Primitive {
    Primitive::Manifest(ManifestSpec::new("grafana.yaml", obj))
}

fn grafana(image: &str) -> Workload {
    Workload::new(
        "grafana",
        vec![
            Primitive::Manifest(crate::primitives::ManifestSpec::new(
                "workload.yaml namespace",
                json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "monitoring"}}),
            )),
            manifest(deployment(image)),
            manifest(
                json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "grafana"},
                "spec": {"ports": [{"name": "http", "port": 3000}], "selector": {"app": "grafana"}}}),
            ),
        ],
    )
}

fn summaries(plan: &crate::WorkloadPlan) -> Vec<String> {
    plan.steps
        .iter()
        .map(|s| match &s.planned {
            Planned::NoOp => format!("noop {}", s.key),
            Planned::Change { diff } => diff.summary().to_owned(),
            Planned::Blocked { reason } => format!("blocked: {reason}"),
        })
        .collect()
}

fn diff_text(step: &Step) -> String {
    match &step.planned {
        Planned::Change {
            diff: Diff::Update { unified, .. },
        } => unified
            .as_ref()
            .map(|u| u.get().clone())
            .unwrap_or_default(),
        Planned::Change {
            diff: Diff::Create { preview, .. },
        } => preview
            .as_ref()
            .map(|u| u.get().clone())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn a_workload_is_created_then_plans_as_no_change() {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    let plan = grafana("grafana:11.3.1").plan(&cx).await.unwrap();
    assert_eq!(
        summaries(&plan),
        vec![
            "create Namespace monitoring",
            "create Deployment monitoring/grafana",
            "create Service monitoring/grafana"
        ]
    );
    plan.apply(&cx).await.unwrap();

    let d = fake
        .object("apps/v1", "Deployment", "monitoring", "grafana")
        .unwrap();
    assert_eq!(d["metadata"]["labels"][object::TARGET_LABEL], "lab-k3s");
    assert_eq!(d["metadata"]["labels"][object::WORKLOAD_LABEL], "grafana");
    let indexed = index::read(&cx.kube, "lab-k3s").await.unwrap().entries;
    assert_eq!(indexed.len(), 3);

    let again = grafana("grafana:11.3.1").plan(&cx).await.unwrap();
    assert_eq!(again.changes(), 0, "{:?}", summaries(&again));
}

#[tokio::test]
async fn an_update_shows_only_the_fields_buckle_owns() {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    grafana("grafana:11.3.1")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    // Another tool annotates the Deployment: not Buckle's field.
    fake.edit(
        "apps/v1",
        "Deployment",
        "monitoring",
        "grafana",
        "argo",
        json!({"metadata": {"annotations": {"note": "x"}}}),
    );

    let plan = grafana("grafana:11.4.0").plan(&cx).await.unwrap();
    let step = &plan.steps[1];
    assert_eq!(
        summaries(&plan)[1],
        "update Deployment monitoring/grafana: spec.template.spec.containers[grafana].image",
        "another tool's fields and server defaults are not Buckle's changes"
    );
    let text = diff_text(step);
    assert!(
        text.contains("-") && text.contains("grafana:11.3.1") && text.contains("grafana:11.4.0"),
        "{text}"
    );
    assert!(!text.contains("progressDeadlineSeconds"), "{text}");
}

#[tokio::test]
async fn an_out_of_band_edit_of_an_owned_field_is_planned_back() {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    grafana("grafana:11.3.1")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    fake.edit(
        "apps/v1", "Deployment", "monitoring", "grafana", "kubectl-edit",
        json!({"spec": {"template": {"spec": {"containers": [{"name": "grafana", "image": "hacked"}]}}}}),
    );
    let plan = grafana("grafana:11.3.1").plan(&cx).await.unwrap();
    assert!(plan.steps[1].is_change());
    assert!(diff_text(&plan.steps[1]).contains("hacked"));
}

/// A deployed grafana and a plan to move it to 11.4.0.
async fn planned_upgrade(cx: &Context) -> crate::WorkloadPlan {
    grafana("grafana:11.3.1")
        .plan(cx)
        .await
        .unwrap()
        .apply(cx)
        .await
        .unwrap();
    let plan = grafana("grafana:11.4.0").plan(cx).await.unwrap();
    assert_eq!(plan.changes(), 1, "{:?}", summaries(&plan));
    plan
}

fn image(fake: &FakeKube) -> Value {
    fake.object("apps/v1", "Deployment", "monitoring", "grafana")
        .unwrap()["spec"]["template"]["spec"]["containers"][0]["image"]
        .clone()
}

/// What moves the Deployment between plan and apply, and whether the apply
/// must refuse.
#[rstest]
#[case::controller_status(
    json!({"status": {"observedGeneration": 1, "readyReplicas": 1}}), "kube-controller-manager", false
)]
#[case::another_tools_field(json!({"metadata": {"annotations": {"x": "y"}}}), "kubectl", false)]
#[case::an_owned_field(
    json!({"spec": {"template": {"spec": {"containers": [{"name": "grafana", "image": "hacked"}]}}}}),
    "kubectl-edit",
    true
)]
#[tokio::test]
async fn an_apply_refuses_only_an_owned_change_made_after_the_plan(
    #[case] patch: Value,
    #[case] manager: &str,
    #[case] refused: bool,
) {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    let plan = planned_upgrade(&cx).await;
    if let Some(status) = patch.get("status") {
        fake.write_status(
            "apps/v1",
            "Deployment",
            "monitoring",
            "grafana",
            status.clone(),
        );
    } else {
        fake.edit(
            "apps/v1",
            "Deployment",
            "monitoring",
            "grafana",
            manager,
            patch,
        );
    }

    let result = plan.apply(&cx).await;
    if refused {
        let err = result.unwrap_err();
        assert!(
            err.description()
                .contains("changed in the cluster after it was planned"),
            "{}",
            err.description()
        );
        assert_eq!(
            image(&fake),
            "hacked",
            "an unshown change is never overwritten"
        );
    } else {
        result.unwrap();
        assert_eq!(image(&fake), "grafana:11.4.0");
    }
}

/// A controller that keeps writing status while the apply runs: a few
/// writes are retried through, an endless stream is refused rather than
/// retried forever.
#[rstest]
#[case::reconciling(2, true)]
#[case::never_settles(100, false)]
#[tokio::test]
async fn an_apply_retries_a_controller_that_keeps_writing_status(
    #[case] writes: usize,
    #[case] applies: bool,
) {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    let plan = planned_upgrade(&cx).await;
    fake.status_writes_before_apply(writes);

    let result = plan.apply(&cx).await;
    assert_eq!(
        result.is_ok(),
        applies,
        "{:?}",
        result.err().map(|e| e.description())
    );
    let expected = if applies {
        "grafana:11.4.0"
    } else {
        "grafana:11.3.1"
    };
    assert_eq!(image(&fake), expected);
}

/// A controller writing status while the plan reads the object: a few
/// writes are read through, an endless stream fails the plan.
#[rstest]
#[case::reconciling(2, true)]
#[case::never_settles(100, false)]
#[tokio::test]
async fn a_plan_reads_through_a_controller_writing_status(
    #[case] writes: usize,
    #[case] plans: bool,
) {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    grafana("grafana:11.3.1")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    fake.status_writes_before_apply(writes);

    match grafana("grafana:11.4.0").plan(&cx).await {
        Ok(plan) if plans => assert_eq!(plan.changes(), 1, "{:?}", summaries(&plan)),
        Err(e) if !plans => assert!(
            e.description().contains("could not be planned"),
            "{}",
            e.description()
        ),
        other => panic!(
            "expected the plan to {}: {:?}",
            if plans { "succeed" } else { "fail" },
            other.err().map(|e| e.description())
        ),
    }
}

/// An ignored field moved by its owner after the plan keeps its new value.
#[tokio::test]
async fn an_apply_keeps_an_ignored_field_moved_after_the_plan() {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    let workload = |image: &str| {
        let mut spec = ManifestSpec::new("grafana.yaml", deployment(image));
        spec.ignore =
            buckle_core::fieldpath::FieldPath::parse_all(&["spec.replicas".into()]).unwrap();
        Workload::new("grafana", vec![Primitive::Manifest(spec)])
    };
    let cx = cx.with_namespace(Some("default"));
    workload("grafana:11.3.1")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    let plan = workload("grafana:11.4.0").plan(&cx).await.unwrap();
    fake.edit(
        "apps/v1",
        "Deployment",
        "default",
        "grafana",
        "hpa",
        json!({"spec": {"replicas": 5}}),
    );

    plan.apply(&cx).await.unwrap();
    let d = fake
        .object("apps/v1", "Deployment", "default", "grafana")
        .unwrap();
    assert_eq!(d["spec"]["replicas"], 5);
    assert_eq!(
        d["spec"]["template"]["spec"]["containers"][0]["image"],
        "grafana:11.4.0"
    );
}

#[rstest]
#[case::other_target(json!({object::TARGET_LABEL: "home", object::WORKLOAD_LABEL: "x"}), "blocked")]
#[case::unmanaged(json!({}), "adopts")]
#[tokio::test]
async fn ownership_of_existing_objects(#[case] labels: Value, #[case] expected: &str) {
    let fake = FakeKube::new();
    fake.insert(
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "monitoring"}}),
    );
    let mut existing = deployment("grafana:11.3.1");
    existing["metadata"]["namespace"] = json!("monitoring");
    existing["metadata"]["labels"] = labels;
    fake.insert(existing);
    let plan = Workload::new("grafana", vec![manifest(deployment("grafana:11.3.1"))])
        .plan(&cx(&fake, "grafana"))
        .await
        .unwrap();
    let step = &plan.steps[0];
    let shown = format!("{} {:?}", summaries(&plan)[0], step.warnings);
    assert!(shown.contains(expected), "{shown}");
}

#[tokio::test]
async fn ignored_fields_keep_their_live_value() {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    grafana("grafana:11.3.1")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    fake.edit(
        "apps/v1",
        "Deployment",
        "monitoring",
        "grafana",
        "kubectl",
        json!({"spec": {"replicas": 5}}),
    );
    let mut spec = ManifestSpec::new("grafana.yaml", deployment("grafana:11.3.1"));
    spec.ignore = buckle_core::fieldpath::FieldPath::parse_all(&["spec.replicas".into()]).unwrap();
    let plan = Workload::new("grafana", vec![Primitive::Manifest(spec)])
        .plan(&cx)
        .await
        .unwrap();
    assert_eq!(plan.changes(), 0, "{:?}", summaries(&plan));
}

fn secret(items: &[(&str, Rendered)]) -> Primitive {
    Primitive::Secret(SecretSpec {
        namespace: String::new(),
        name: "grafana".into(),
        secret_type: None,
        items: items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect(),
    })
}

#[tokio::test]
async fn secret_values_never_appear_in_plans_and_owned_keys_are_tracked() {
    let fake = FakeKube::new();
    fake.insert(
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "monitoring"}}),
    );
    let cx = cx(&fake, "grafana");
    let first = Workload::new(
        "grafana",
        vec![secret(&[
            ("DB_PASSWORD", render("{{ secrets.db }}")),
            ("TZ", Rendered::Plain("UTC".into())),
        ])],
    );
    let plan = first.plan(&cx).await.unwrap();
    let text = format!("{:?} {}", plan.steps[0].planned, diff_text(&plan.steps[0]));
    assert!(!text.contains(SECRET), "{text}");
    assert!(
        text.contains("value@") && text.contains("\"UTC\""),
        "{text}"
    );
    plan.apply(&cx).await.unwrap();

    let stored = fake
        .object("v1", "Secret", "monitoring", "grafana")
        .unwrap();
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stored["data"]["DB_PASSWORD"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, SECRET.as_bytes());
    assert_eq!(first.plan(&cx).await.unwrap().changes(), 0);

    // An operator's key stays; a key the workload drops is removed.
    fake.edit(
        "v1",
        "Secret",
        "monitoring",
        "grafana",
        "kubectl",
        json!({"data": {"EXTRA": "eA=="}}),
    );
    let second = Workload::new(
        "grafana",
        vec![secret(&[("DB_PASSWORD", render("{{ secrets.db }}"))])],
    );
    let plan = second.plan(&cx).await.unwrap();
    let text = diff_text(&plan.steps[0]);
    assert!(text.contains("- TZ") && !text.contains("EXTRA"), "{text}");
    plan.apply(&cx).await.unwrap();
    let stored = fake
        .object("v1", "Secret", "monitoring", "grafana")
        .unwrap();
    let keys: Vec<&String> = stored["data"].as_object().unwrap().keys().collect();
    assert_eq!(keys, vec!["DB_PASSWORD", "EXTRA"]);
}

#[tokio::test]
async fn a_config_map_shows_its_values() {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    let w = |v: &str| {
        let mut prims = grafana("grafana:11.3.1").primitives;
        prims.insert(
            1,
            Primitive::Manifest(ManifestSpec::config_map(
                "grafana",
                None,
                [("grafana.ini".to_owned(), format!("[server]\nport = {v}\n"))],
            )),
        );
        Workload::new("grafana", prims)
    };
    w("3000").plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    let plan = w("3001").plan(&cx).await.unwrap();
    assert_eq!(plan.changes(), 1);
    let text = diff_text(&plan.steps[1]);
    assert!(text.contains("3000") && text.contains("3001"), "{text}");
}

#[rstest]
#[case::stop(Removal::Stop, &["delete Deployment monitoring/grafana", "delete Service monitoring/grafana"], true)]
#[case::purge(Removal::Purge, &["delete Deployment monitoring/grafana", "delete ConfigMap monitoring/grafana", "delete Service monitoring/grafana", "delete Namespace monitoring"], false)]
#[case::keep(Removal::Keep, &[], true)]
#[tokio::test]
async fn removed_objects_are_orphans_removed_by_policy(
    #[case] removal: Removal,
    #[case] expected: &[&str],
    #[case] config_kept: bool,
) {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    let mut w = grafana("grafana:11.3.1");
    w.primitives
        .push(Primitive::Manifest(ManifestSpec::config_map(
            "grafana",
            None,
            [("a".to_owned(), "b".to_owned())],
        )));
    w.plan(&cx).await.unwrap().apply(&cx).await.unwrap();

    let orphans = orphans(&cx, &[]).await.unwrap();
    let described: Vec<String> = orphans.iter().filter_map(|o| o.describe(removal)).collect();
    assert_eq!(described, expected);
    for o in &orphans {
        o.remove(&cx, removal).await.unwrap();
    }
    assert!(
        fake.object("apps/v1", "Deployment", "monitoring", "grafana")
            .is_some()
            == (removal == Removal::Keep)
    );
    assert_eq!(
        fake.object("v1", "ConfigMap", "monitoring", "grafana")
            .is_some(),
        config_kept
    );
}

#[rstest]
#[case::stuck("stuck", Some("did not roll out"))]
#[case::pending("pending", Some("still rolling out"))]
#[case::unschedulable("unschedulable", Some("could not be placed"))]
#[case::complete("complete", None)]
#[tokio::test]
async fn an_apply_waits_for_the_rollout(#[case] outcome: &str, #[case] error: Option<&str>) {
    let fake = FakeKube::new();
    fake.rollout_outcome(outcome);
    let cx = cx(&fake, "grafana");
    let result = grafana("grafana:11.3.1")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await;
    match error {
        Some(e) => assert!(result.unwrap_err().description().contains(e)),
        None => assert!(result.is_ok()),
    }
}

#[tokio::test]
async fn health_follows_rollouts_and_pods() {
    let fake = FakeKube::new();
    fake.rollout_outcome("crashloop");
    let cx = cx(&fake, "grafana");
    grafana("grafana:11.3.1")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    let key = object::ObjectKey::new("apps/v1", "Deployment", "monitoring", "grafana");
    let checks = health::object(&cx.kube, &key).await.unwrap();
    let failed: Vec<&str> = checks
        .iter()
        .filter(|c| !c.status.is_pass())
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(failed, vec!["monitoring/Deployment/grafana pods"]);
}

#[tokio::test]
async fn a_custom_resource_applies_after_its_crd() {
    let fake = FakeKube::new();
    let cx = cx(&fake, "certs");
    let w = Workload::new(
        "certs",
        vec![
            manifest(
                json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
                "metadata": {"name": "issuers.example.com"},
                "spec": {"group": "example.com", "scope": "Namespaced",
                    "names": {"kind": "Issuer", "plural": "issuers"}, "versions": [{"name": "v1"}]}}),
            ),
            manifest(
                json!({"apiVersion": "example.com/v1", "kind": "Issuer", "metadata": {"name": "lab", "namespace": "default"}}),
            ),
        ],
    );
    let plan = w.plan(&cx).await.unwrap();
    assert_eq!(plan.steps[1].warnings.len(), 1, "planned without a dry run");
    plan.apply(&cx).await.unwrap();
    assert!(
        fake.object("example.com/v1", "Issuer", "default", "lab")
            .is_some()
    );
    assert_eq!(w.plan(&cx).await.unwrap().changes(), 0);
}

#[tokio::test]
async fn an_object_listed_twice_is_refused() {
    let fake = FakeKube::new();
    let w = Workload::new(
        "grafana",
        vec![manifest(deployment("a")), manifest(deployment("b"))],
    );
    fake.insert(
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "monitoring"}}),
    );
    let err = w.plan(&cx(&fake, "grafana")).await.unwrap_err();
    assert!(err.description().contains("twice"));
}

#[tokio::test]
async fn restart_and_undo() {
    let fake = FakeKube::new();
    let cx = cx(&fake, "grafana");
    grafana("grafana:11.3.1")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    let key = object::ObjectKey::new("apps/v1", "Deployment", "monitoring", "grafana");
    let live = fake
        .object("apps/v1", "Deployment", "monitoring", "grafana")
        .unwrap();
    assert_eq!(
        actions::discover(&key, &live),
        vec![actions::Action::Restart]
    );
    actions::run(&cx.kube, &key, &actions::Action::Restart)
        .await
        .unwrap();
    let live = fake
        .object("apps/v1", "Deployment", "monitoring", "grafana")
        .unwrap();
    assert!(live["spec"]["template"]["metadata"]["annotations"][actions::RESTARTED_AT].is_string());

    // Two revisions, as the Deployment controller records them.
    fake.edit(
        "apps/v1",
        "Deployment",
        "monitoring",
        "grafana",
        "kube-controller-manager",
        json!({"metadata": {"annotations": {"deployment.kubernetes.io/revision": "2"}}}),
    );
    let uid = live["metadata"]["uid"].clone();
    for (rev, image) in [("1", "grafana:old"), ("2", "grafana:11.3.1")] {
        fake.insert(json!({"apiVersion": "apps/v1", "kind": "ReplicaSet",
            "metadata": {"name": format!("grafana-{rev}"), "namespace": "monitoring", "labels": {"app": "grafana"},
                "annotations": {"deployment.kubernetes.io/revision": rev}, "ownerReferences": [{"uid": uid}]},
            "spec": {"template": {"metadata": {"labels": {"app": "grafana", "pod-template-hash": rev}},
                "spec": {"containers": [{"name": "grafana", "image": image}]}}}}));
    }
    let live = fake
        .object("apps/v1", "Deployment", "monitoring", "grafana")
        .unwrap();
    assert!(actions::discover(&key, &live).contains(&actions::Action::Undo));
    actions::run(&cx.kube, &key, &actions::Action::Undo)
        .await
        .unwrap();
    let live = fake
        .object("apps/v1", "Deployment", "monitoring", "grafana")
        .unwrap();
    assert_eq!(
        live["spec"]["template"]["spec"]["containers"][0]["image"],
        "grafana:old"
    );
    assert!(live["spec"]["template"]["metadata"]["labels"]["pod-template-hash"].is_null());
}
