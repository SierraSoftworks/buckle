//! The primitives against a real Kubernetes API: a single-node k3s server
//! started by `scripts/k3s-dev-server.sh` (CI runs it on the x64 Linux job).
//!
//! Skipped unless `BUCKLE_KUBECONFIG` names its kubeconfig. Each test uses
//! its own target and namespace, so they run in parallel.

use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::{InstallKey, Rendered};
use buckle_kube::primitives::*;
use buckle_kube::{Endpoint, Kube, Workload, health, object::ObjectKey};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

fn kube() -> Option<Kube> {
    let path = std::env::var("BUCKLE_KUBECONFIG").ok()?;
    let endpoint =
        Endpoint::from_kubeconfig(std::path::Path::new(&path)).expect("a usable kubeconfig");
    Some(Kube::connect(endpoint).expect("a client"))
}

macro_rules! kube_or_skip {
    () => {
        match kube() {
            Some(k) => k,
            None => {
                eprintln!("BUCKLE_KUBECONFIG is not set; skipping (run scripts/k3s-dev-server.sh)");
                return;
            }
        }
    };
}

fn cx(kube: &Kube, target: &str, ns: &str) -> Context {
    let mut cx = Context::new(
        kube.clone(),
        target,
        "it",
        "rev-1",
        Arc::new(InstallKey::from_bytes([5; 32])),
    )
    .with_namespace(Some(ns));
    cx.poll = Duration::from_secs(1);
    cx.rollout_timeout = Duration::from_secs(240);
    cx
}

fn deployment(env: &str) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": "app", "labels": {"app": "it"}},
        "spec": {
            "replicas": 1,
            "selector": {"matchLabels": {"app": "it"}},
            "template": {
                "metadata": {"labels": {"app": "it"}},
                "spec": {
                    "terminationGracePeriodSeconds": 1,
                    "containers": [{
                        "name": "app",
                        "image": "registry.k8s.io/pause:3.10",
                        "env": [{"name": "MODE", "value": env}]
                    }]
                }
            }
        }
    })
}

fn workload(ns: &str, env: &str, config: &str) -> Workload {
    Workload::new(
        "it",
        vec![
            Primitive::Manifest(namespace(ns)),
            Primitive::Manifest(ManifestSpec::config_map(
                "app",
                None,
                [("app.conf".to_owned(), config.to_owned())],
            )),
            Primitive::Manifest(ManifestSpec::new("app.yaml", deployment(env))),
            Primitive::Manifest(ManifestSpec::new(
                "app.yaml",
                json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "app"},
                    "spec": {"selector": {"app": "it"}, "ports": [{"name": "http", "port": 80, "targetPort": 8080}]}}),
            )),
        ],
    )
}

fn changed(plan: &buckle_kube::WorkloadPlan) -> Vec<String> {
    plan.steps
        .iter()
        .filter(|s| s.is_change())
        .map(|s| match &s.planned {
            Planned::Change { diff } => diff.summary().to_owned(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn a_workload_rolls_out_replans_as_no_change_and_is_purged() {
    let kube = kube_or_skip!();
    let ns = "it-lifecycle";
    let cx = cx(&kube, "it-lifecycle", ns);

    let plan = workload(ns, "a", "x = 1\n").plan(&cx).await.unwrap();
    assert_eq!(plan.changes(), 4, "{:?}", changed(&plan));
    plan.apply(&cx).await.unwrap();

    // The real server's defaults and managedFields must not show as changes.
    let again = workload(ns, "a", "x = 1\n").plan(&cx).await.unwrap();
    assert_eq!(changed(&again), Vec::<String>::new());

    // Another manager's field is not Buckle's change.
    let path = format!("/apis/apps/v1/namespaces/{ns}/deployments/app");
    kube.send(
        "PATCH",
        &format!("{path}?fieldManager=someone-else"),
        Some("application/merge-patch+json"),
        Some(br#"{"metadata":{"annotations":{"note":"x"}}}"#.to_vec()),
    )
    .await
    .unwrap();
    let again = workload(ns, "a", "x = 1\n").plan(&cx).await.unwrap();
    assert_eq!(changed(&again), Vec::<String>::new());

    // A real change: only the env var and the config.
    let next = workload(ns, "b", "x = 2\n").plan(&cx).await.unwrap();
    let summaries = changed(&next);
    assert_eq!(summaries.len(), 2, "{summaries:?}");
    assert!(summaries.iter().any(|s| s.contains("env")), "{summaries:?}");
    let diff = next
        .steps
        .iter()
        .find_map(|s| match &s.planned {
            Planned::Change {
                diff: Diff::Update {
                    unified: Some(u), ..
                },
            } if s.key.kind == "ConfigMap" => Some(u.get().clone()),
            _ => None,
        })
        .expect("the ConfigMap's diff");
    assert!(diff.contains("x = 1") && diff.contains("x = 2"), "{diff}");

    // The object moves between plan and apply: the apply refuses.
    kube.send(
        "PATCH",
        &format!("{path}?fieldManager=someone-else"),
        Some("application/merge-patch+json"),
        Some(br#"{"metadata":{"annotations":{"note":"y"}}}"#.to_vec()),
    )
    .await
    .unwrap();
    let err = next.apply(&cx).await.unwrap_err();
    assert!(
        err.description().contains("after it was planned"),
        "{}",
        err.description()
    );
    workload(ns, "b", "x = 2\n")
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();

    let key = ObjectKey::new("apps/v1", "Deployment", ns, "app");
    let checks = health::object(&kube, &key).await.unwrap();
    assert!(checks.iter().all(|c| c.status.is_pass()), "{checks:?}");

    let orphans = orphans(&cx, &[]).await.unwrap();
    assert_eq!(
        orphans.last().map(|o| o.key.kind.as_str()),
        Some("Namespace")
    );
    for o in &orphans {
        o.remove(&cx, Removal::Purge).await.unwrap();
    }
    assert!(orphans_after(&cx).await.is_empty());
}

async fn orphans_after(cx: &Context) -> Vec<Orphan> {
    orphans(cx, &[]).await.unwrap()
}

#[tokio::test]
async fn a_secret_keeps_operator_keys_and_never_shows_values() {
    let kube = kube_or_skip!();
    let ns = "it-secret";
    let cx = cx(&kube, "it-secret", ns);
    let secret = |items: &[(&str, &str)]| {
        Primitive::Secret(SecretSpec {
            namespace: String::new(),
            name: "creds".into(),
            secret_type: None,
            items: items
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Rendered::Plain((*v).to_owned())))
                .collect(),
        })
    };
    let w = |items: &[(&str, &str)]| {
        Workload::new(
            "it",
            vec![Primitive::Manifest(namespace(ns)), secret(items)],
        )
    };
    w(&[("A", "1"), ("B", "2")])
        .plan(&cx)
        .await
        .unwrap()
        .apply(&cx)
        .await
        .unwrap();
    assert_eq!(
        w(&[("A", "1"), ("B", "2")])
            .plan(&cx)
            .await
            .unwrap()
            .changes(),
        0
    );

    kube.send(
        "PATCH",
        &format!("/api/v1/namespaces/{ns}/secrets/creds?fieldManager=operator"),
        Some("application/merge-patch+json"),
        Some(br#"{"data":{"EXTRA":"eA=="}}"#.to_vec()),
    )
    .await
    .unwrap();
    let plan = w(&[("A", "1")]).plan(&cx).await.unwrap();
    assert_eq!(plan.changes(), 1);
    plan.apply(&cx).await.unwrap();
    let live = kube
        .get(&format!("/api/v1/namespaces/{ns}/secrets/creds"))
        .await
        .unwrap()
        .unwrap();
    let mut keys: Vec<&String> = live["data"].as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, vec!["A", "EXTRA"]);

    for o in orphans(&cx, &[]).await.unwrap() {
        o.remove(&cx, Removal::Purge).await.unwrap();
    }
}

#[tokio::test]
async fn a_custom_resource_applies_with_its_crd() {
    let kube = kube_or_skip!();
    let cx = cx(&kube, "it-crd", "default");
    let crd = json!({
        "apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition",
        "metadata": {"name": "widgets.it.buckle.test"},
        "spec": {
            "group": "it.buckle.test", "scope": "Namespaced",
            "names": {"kind": "Widget", "plural": "widgets", "singular": "widget"},
            "versions": [{"name": "v1", "served": true, "storage": true,
                "schema": {"openAPIV3Schema": {"type": "object", "properties": {
                    "spec": {"type": "object", "properties": {"size": {"type": "integer"}}}}}}}]
        }
    });
    let w = |size: u64| {
        Workload::new(
            "it",
            vec![
                Primitive::Manifest(ManifestSpec::new("crd.yaml", crd.clone())),
                Primitive::Manifest(ManifestSpec::new(
                    "widget.yaml",
                    json!({"apiVersion": "it.buckle.test/v1", "kind": "Widget",
                        "metadata": {"name": "w"}, "spec": {"size": size}}),
                )),
            ],
        )
    };
    w(1).plan(&cx).await.unwrap().apply(&cx).await.unwrap();
    assert_eq!(w(1).plan(&cx).await.unwrap().changes(), 0);
    assert_eq!(w(2).plan(&cx).await.unwrap().changes(), 1);
    for o in orphans(&cx, &[]).await.unwrap() {
        o.remove(&cx, Removal::Purge).await.unwrap();
    }
}
