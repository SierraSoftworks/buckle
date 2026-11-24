//! Node operations a cluster's steward performs for the cluster's nodes:
//! cordon, uncordon, eviction and bootstrap tokens. A host never holds a
//! credential for them: it asks the Buckle server, which hands the request
//! to the steward.

use crate::client::{Kube, seg, with_query};
use buckle_core::Result;
use buckle_core::error::system;
use chrono::{DateTime, Utc};
use secrecy::SecretString;
use serde_json::{Value, json};
use std::time::Duration;

/// The annotation a node carries while Buckle drains it: when its pods'
/// time to leave runs out (RFC 3339). Kubernetes has no drain of its own
/// (`kubectl drain` is a client loop), so this is what Nomad's drain
/// strategy is on a Nomad node: the steward carries the drain on whenever
/// it reads the node, and removes the annotation once the node is empty.
pub const DRAIN_DEADLINE_ANNOTATION: &str = "buckle.sierrasoftworks.com/drain-deadline";

/// A node as the API server sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub name: String,
    /// Whether new pods are scheduled onto it (it is not cordoned).
    pub schedulable: bool,
    /// While Buckle drains it: when what is left on it is deleted.
    pub drain_deadline: Option<DateTime<Utc>>,
}

fn path(name: &str) -> String {
    format!("/api/v1/nodes/{}", seg(name))
}

/// The node named `name`, when the cluster has one.
pub async fn find(kube: &Kube, name: &str) -> Result<Option<Node>> {
    Ok(kube.get(&path(name)).await?.map(|node| Node {
        name: name.to_owned(),
        schedulable: node["spec"]["unschedulable"] != json!(true),
        drain_deadline: node["metadata"]["annotations"][DRAIN_DEADLINE_ANNOTATION]
            .as_str()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc)),
    }))
}

/// Uncordons `node`, ending a drain Buckle has under way. Returns whether
/// it was cordoned.
pub async fn uncordon(kube: &Kube, node: &Node) -> Result<bool> {
    if node.schedulable && node.drain_deadline.is_none() {
        return Ok(false);
    }
    kube.merge_patch(
        &path(&node.name),
        &json!({
            "metadata": {"annotations": {DRAIN_DEADLINE_ANNOTATION: null}},
            "spec": {"unschedulable": null},
        }),
    )
    .await?;
    Ok(!node.schedulable)
}

/// How a drain stands after one pass over the node's pods.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Drained {
    /// Pods asked to leave in this pass.
    pub evicted: usize,
    /// Pods a disruption budget holds for now: asked again on the next pass.
    pub held: usize,
    /// Pods deleted in this pass because the deadline had passed,
    /// disruption budget or not.
    pub deleted: usize,
    /// Pods still on the node, those shutting down included.
    pub remaining: usize,
}

impl Drained {
    /// Whether the node's work has moved: nothing but DaemonSet and static
    /// pods is left on it.
    pub fn done(&self) -> bool {
        self.remaining == 0
    }
}

/// Cordons `node`, gives its pods until `deadline` from now to leave, and
/// asks each to leave once (not those a DaemonSet runs, nor the kubelet's
/// own static pods). [`proceed`] carries the drain on. `None` when the node
/// was already cordoned: nothing is done then.
pub async fn drain(kube: &Kube, node: &Node, deadline: Duration) -> Result<Option<Drained>> {
    if !node.schedulable {
        return Ok(None);
    }
    // A year at most: far enough to mean "no hurry", near enough to write.
    let deadline = deadline.min(Duration::from_secs(365 * 24 * 60 * 60));
    let until = Utc::now() + chrono::Duration::from_std(deadline).unwrap_or_default();
    kube.merge_patch(
        &path(&node.name),
        &json!({
            "metadata": {"annotations": {
                DRAIN_DEADLINE_ANNOTATION: until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            }},
            "spec": {"unschedulable": true},
        }),
    )
    .await?;
    let drained = pass(kube, &node.name, None).await?;
    if drained.done() {
        finish(kube, &node.name).await?;
    }
    Ok(Some(drained))
}

/// Carries on a drain Buckle has under way on `node`, as Nomad's drainer
/// does for its own: asks again what a disruption budget held, and once
/// the deadline has passed deletes what is still there (each pod keeps its
/// termination grace period). The drain ends, and the node keeps no mark
/// of it, once nothing is left to move. `None` when Buckle is not draining
/// the node, and when someone uncordoned it meanwhile (that ends the drain).
pub async fn proceed(kube: &Kube, node: &Node) -> Result<Option<Drained>> {
    let Some(deadline) = node.drain_deadline else {
        return Ok(None);
    };
    if node.schedulable {
        finish(kube, &node.name).await?;
        return Ok(None);
    }
    let drained = pass(kube, &node.name, Some(deadline)).await?;
    if drained.done() {
        finish(kube, &node.name).await?;
    }
    Ok(Some(drained))
}

/// Drops the drain's mark: the node stays cordoned.
async fn finish(kube: &Kube, name: &str) -> Result<()> {
    kube.merge_patch(
        &path(name),
        &json!({"metadata": {"annotations": {DRAIN_DEADLINE_ANNOTATION: null}}}),
    )
    .await?;
    Ok(())
}

/// One pass over the pods on node `name`: asks each one that should move
/// and is not already leaving to go, or deletes it once `deadline` has
/// passed.
async fn pass(kube: &Kube, name: &str, deadline: Option<DateTime<Utc>>) -> Result<Drained> {
    let force = deadline.is_some_and(|d| Utc::now() >= d);
    let pods = with_query(
        "/api/v1/pods",
        "fieldSelector",
        &format!("spec.nodeName={name}"),
    );
    let pods = kube.get(&pods).await?.unwrap_or(Value::Null);
    let mut drained = Drained::default();
    for pod in pods["items"].as_array().into_iter().flatten() {
        let meta = &pod["metadata"];
        let stays = pod["spec"]["nodeName"] != json!(name)
            || meta["annotations"]["kubernetes.io/config.mirror"].is_string()
            || meta["ownerReferences"]
                .as_array()
                .is_some_and(|o| o.iter().any(|r| r["kind"] == "DaemonSet"))
            // Finished: it holds nothing that has to move.
            || matches!(pod["status"]["phase"].as_str(), Some("Succeeded" | "Failed"));
        let (Some(namespace), Some(pod_name), false) =
            (meta["namespace"].as_str(), meta["name"].as_str(), stays)
        else {
            continue;
        };
        drained.remaining += 1;
        // Already shutting down: it only has to finish.
        if meta["deletionTimestamp"].is_string() {
            continue;
        }
        let pod_path = format!(
            "/api/v1/namespaces/{}/pods/{}",
            seg(namespace),
            seg(pod_name)
        );
        if force {
            match kube.delete(&pod_path).await? {
                true => drained.deleted += 1,
                false => drained.remaining -= 1,
            }
            continue;
        }
        let eviction = format!("{pod_path}/eviction");
        let body = json!({
            "apiVersion": "policy/v1",
            "kind": "Eviction",
            "metadata": {"name": pod_name, "namespace": namespace},
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
            404 => drained.remaining -= 1,
            // A PodDisruptionBudget does not allow it yet.
            429 => drained.held += 1,
            _ => {
                kube.ok("POST", &eviction, resp)?;
                drained.evicted += 1;
            }
        }
    }
    Ok(drained)
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
