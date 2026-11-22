//! Node operations a cluster's steward performs for the cluster's nodes:
//! cordon, uncordon, eviction and bootstrap tokens. A host never holds a
//! credential for them: it asks the Buckle server, which hands the request
//! to the steward.

use crate::client::{Kube, seg, with_query};
use buckle_core::Result;
use buckle_core::error::system;
use secrecy::SecretString;
use serde_json::{Value, json};
use std::time::Duration;

/// A node as the API server sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    /// Whether new pods are scheduled onto it (it is not cordoned).
    pub schedulable: bool,
}

fn path(name: &str) -> String {
    format!("/api/v1/nodes/{}", seg(name))
}

/// The node named `name`, when the cluster has one.
pub async fn find(kube: &Kube, name: &str) -> Result<Option<Node>> {
    Ok(kube.get(&path(name)).await?.map(|node| Node {
        name: name.to_owned(),
        schedulable: node["spec"]["unschedulable"] != json!(true),
    }))
}

/// Uncordons `node`. Returns whether anything changed.
pub async fn uncordon(kube: &Kube, node: &Node) -> Result<bool> {
    if node.schedulable {
        return Ok(false);
    }
    kube.merge_patch(&path(&node.name), &json!({"spec": {"unschedulable": null}}))
        .await?;
    Ok(true)
}

/// How a drain went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drained {
    /// Pods asked to leave.
    pub evicted: usize,
    /// Pods a disruption budget holds for now; the cluster's controllers
    /// move them as the budget allows.
    pub held: usize,
}

/// Cordons `node` and evicts its pods, leaving those a DaemonSet runs and
/// the kubelet's own static pods. `None` when the node was already
/// cordoned: nothing is done then.
pub async fn drain(kube: &Kube, node: &Node) -> Result<Option<Drained>> {
    if !node.schedulable {
        return Ok(None);
    }
    kube.merge_patch(&path(&node.name), &json!({"spec": {"unschedulable": true}}))
        .await?;
    let pods = with_query(
        "/api/v1/pods",
        "fieldSelector",
        &format!("spec.nodeName={}", node.name),
    );
    let pods = kube.get(&pods).await?.unwrap_or(Value::Null);
    let mut drained = Drained::default();
    for pod in pods["items"].as_array().into_iter().flatten() {
        let meta = &pod["metadata"];
        let stays = pod["spec"]["nodeName"] != json!(node.name)
            || meta["annotations"]["kubernetes.io/config.mirror"].is_string()
            || meta["ownerReferences"]
                .as_array()
                .is_some_and(|o| o.iter().any(|r| r["kind"] == "DaemonSet"));
        let (Some(namespace), Some(name), false) =
            (meta["namespace"].as_str(), meta["name"].as_str(), stays)
        else {
            continue;
        };
        let eviction = format!(
            "/api/v1/namespaces/{}/pods/{}/eviction",
            seg(namespace),
            seg(name)
        );
        let body = json!({
            "apiVersion": "policy/v1",
            "kind": "Eviction",
            "metadata": {"name": name, "namespace": namespace},
        });
        let resp = kube
            .send(
                "POST",
                &eviction,
                Some("application/json"),
                Some(serde_json::to_vec(&body).unwrap_or_default()),
            )
            .await?;
        match resp.status {
            // Already gone.
            404 => {}
            // A PodDisruptionBudget does not allow it yet.
            429 => drained.held += 1,
            _ => {
                kube.ok("POST", &eviction, resp)?;
                drained.evicted += 1;
            }
        }
    }
    Ok(Some(drained))
}

/// The namespace bootstrap tokens live in.
pub const BOOTSTRAP_NAMESPACE: &str = "kube-system";

/// Creates a bootstrap token (`<id>.<secret>`, kubeadm's format) for a node
/// named `name` to join with, valid for `ttl`. `id` is six and `secret`
/// sixteen characters of `[a-z0-9]`, chosen by the caller at random. With
/// `pool`, the token authenticates into the extra group
/// `system:bootstrappers:buckle:<pool>` beside the default one.
pub async fn bootstrap_token(
    kube: &Kube,
    name: &str,
    pool: Option<&str>,
    ttl: Duration,
    id: &str,
    secret: &str,
) -> Result<SecretString> {
    let expires = chrono::Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default();
    let mut groups = vec!["system:bootstrappers:kubeadm:default-node-token".to_owned()];
    if let Some(pool) = pool.filter(|p| !p.is_empty()) {
        groups.push(format!("system:bootstrappers:buckle:{pool}"));
    }
    let object = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "type": "bootstrap.kubernetes.io/token",
        "metadata": {
            "name": format!("bootstrap-token-{id}"),
            "namespace": BOOTSTRAP_NAMESPACE,
            "labels": {"app.kubernetes.io/managed-by": "buckle"},
        },
        "stringData": {
            "description": format!("Buckle: {name} joins the cluster"),
            "token-id": id,
            "token-secret": secret,
            "expiration": expires.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "usage-bootstrap-authentication": "true",
            "usage-bootstrap-signing": "true",
            "auth-extra-groups": groups.join(","),
        },
    });
    let secrets = format!("/api/v1/namespaces/{BOOTSTRAP_NAMESPACE}/secrets");
    if !kube.create(&secrets, &object).await? {
        return Err(system(
            format!("A bootstrap token with the id {id} already exists."),
            &["Ask again: every token gets a new random id."],
        ));
    }
    Ok(format!("{id}.{secret}").into())
}
