//! Implicit actions on a workload's controllers: Rollout restart on every
//! Deployment, StatefulSet and DaemonSet, and Roll back on a Deployment with
//! an earlier revision. None are written in the workload.

use crate::client::{Kube, with_query};
use crate::health::is_rollout;
use crate::object::ObjectKey;
use buckle_core::Result;
use buckle_core::error::user;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The annotation `kubectl rollout restart` sets on the pod template.
pub const RESTARTED_AT: &str = "kubectl.kubernetes.io/restartedAt";
const REVISION: &str = "deployment.kubernetes.io/revision";

/// An action a controller offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Replace every pod, as the update strategy allows.
    Restart,
    /// Roll a Deployment back to its previous revision.
    Undo,
}

impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Action::Restart => "Rollout restart",
            Action::Undo => "Roll back…",
        }
    }

    pub fn destructive(&self) -> bool {
        matches!(self, Action::Undo)
    }
}

/// The actions `obj` (a live object of kind `key`) offers.
pub fn discover(key: &ObjectKey, obj: &Value) -> Vec<Action> {
    if !is_rollout(key) {
        return Vec::new();
    }
    let mut out = vec![Action::Restart];
    let revision: u64 = obj["metadata"]["annotations"][REVISION]
        .as_str()
        .and_then(|r| r.parse().ok())
        .unwrap_or(0);
    if key.kind == "Deployment" && revision > 1 {
        out.push(Action::Undo);
    }
    out
}

/// Runs `action` on `key`.
pub async fn run(kube: &Kube, key: &ObjectKey, action: &Action) -> Result<String> {
    let resource = kube.resource_of(key).await?;
    let path = resource.path(&key.namespace, Some(&key.name));
    match action {
        Action::Restart => {
            let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            kube.merge_patch(
                &path,
                &json!({"spec": {"template": {"metadata": {"annotations": {RESTARTED_AT: now}}}}}),
            )
            .await?;
            Ok(format!("restarted {key}"))
        }
        Action::Undo => {
            let live = kube.get(&path).await?.ok_or_else(|| {
                user(
                    format!("{key} does not exist."),
                    &["Apply the workload first."],
                )
            })?;
            let template = previous_template(kube, key, &live).await?;
            kube.merge_patch(&path, &json!({"spec": {"template": template}}))
                .await?;
            Ok(format!("rolled {key} back to its previous revision"))
        }
    }
}

/// The pod template of the ReplicaSet one revision before `live`'s, without
/// its `pod-template-hash` label (as `kubectl rollout undo` does).
async fn previous_template(kube: &Kube, key: &ObjectKey, live: &Value) -> Result<Value> {
    let current: u64 = live["metadata"]["annotations"][REVISION]
        .as_str()
        .and_then(|r| r.parse().ok())
        .unwrap_or(0);
    let uid = live["metadata"]["uid"].as_str().unwrap_or_default();
    let selector = live["spec"]["selector"]["matchLabels"]
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let list = kube
        .get(&with_query(
            &format!(
                "/apis/apps/v1/namespaces/{}/replicasets",
                crate::client::seg(&key.namespace)
            ),
            "labelSelector",
            &selector,
        ))
        .await?
        .unwrap_or(Value::Null);
    let previous = list["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|rs| {
            rs["metadata"]["ownerReferences"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|o| o["uid"].as_str() == Some(uid))
        })
        .filter_map(|rs| {
            let rev: u64 = rs["metadata"]["annotations"][REVISION]
                .as_str()?
                .parse()
                .ok()?;
            (rev < current).then_some((rev, rs))
        })
        .max_by_key(|(rev, _)| *rev)
        .map(|(_, rs)| rs.clone())
        .ok_or_else(|| {
            user(
                format!("{key} has no earlier revision to roll back to."),
                &["Roll back the workload to an earlier Git revision instead."],
            )
        })?;
    let mut template = previous["spec"]["template"].clone();
    if let Some(labels) = template["metadata"]["labels"].as_object_mut() {
        labels.remove("pod-template-hash");
    }
    Ok(template)
}
