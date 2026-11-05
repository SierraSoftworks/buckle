//! Implicit health of a workload's objects, computed the way `kstatus` does:
//! a controller is healthy when it observed its latest generation and every
//! replica is updated and available; its pods must not be stuck crashing or
//! pulling images; anything with a `Ready` condition follows it.

use crate::client::{Kube, with_query};
use crate::object::ObjectKey;
use buckle_core::Result;
use buckle_core::health::HealthStatus;
use serde_json::Value;

/// One check's outcome.
#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: HealthStatus,
}

/// Where a rollout stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rollout {
    Done(String),
    Progressing(String),
    Failed(String),
}

/// The controllers an apply waits for and health follows.
pub fn is_rollout(key: &ObjectKey) -> bool {
    key.group() == "apps"
        && matches!(
            key.kind.as_str(),
            "Deployment" | "StatefulSet" | "DaemonSet"
        )
}

fn int(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn condition<'a>(obj: &'a Value, kind: &str) -> Option<&'a Value> {
    obj["status"]["conditions"]
        .as_array()?
        .iter()
        .find(|c| c["type"].as_str() == Some(kind))
}

/// The rollout state of a Deployment, StatefulSet or DaemonSet. With
/// `generation`, the controller must have observed at least that one.
pub fn rollout(obj: &Value, generation: Option<i64>) -> Rollout {
    if obj.is_null() {
        return Rollout::Failed("it was deleted".into());
    }
    let want = generation.unwrap_or_else(|| int(&obj["metadata"]["generation"]));
    let status = &obj["status"];
    if int(&status["observedGeneration"]) < want {
        return Rollout::Progressing("waiting for the controller to observe the change".into());
    }
    match obj["kind"].as_str().unwrap_or_default() {
        "Deployment" => {
            if let Some(c) = condition(obj, "Progressing")
                && c["status"].as_str() == Some("False")
            {
                return Rollout::Failed(
                    c["message"]
                        .as_str()
                        .unwrap_or("its progress deadline passed")
                        .to_owned(),
                );
            }
            let replicas = obj["spec"]["replicas"].as_i64().unwrap_or(1);
            let updated = int(&status["updatedReplicas"]);
            let available = int(&status["availableReplicas"]);
            let total = int(&status["replicas"]);
            if updated < replicas {
                Rollout::Progressing(format!("{updated}/{replicas} updated"))
            } else if total > updated {
                Rollout::Progressing(format!("{} old replicas terminating", total - updated))
            } else if available < replicas {
                Rollout::Progressing(format!("{available}/{replicas} available"))
            } else {
                Rollout::Done(format!("{available}/{replicas} available"))
            }
        }
        "StatefulSet" => {
            let replicas = obj["spec"]["replicas"].as_i64().unwrap_or(1);
            let ready = int(&status["readyReplicas"]);
            let rolling = obj["spec"]["updateStrategy"]["type"].as_str() != Some("OnDelete");
            let updated = int(&status["updatedReplicas"]);
            if rolling && updated < replicas {
                Rollout::Progressing(format!("{updated}/{replicas} updated"))
            } else if ready < replicas {
                Rollout::Progressing(format!("{ready}/{replicas} ready"))
            } else if rolling
                && status["updateRevision"].is_string()
                && status["currentRevision"] != status["updateRevision"]
            {
                Rollout::Progressing("finishing the update".into())
            } else {
                Rollout::Done(format!("{ready}/{replicas} ready"))
            }
        }
        "DaemonSet" => {
            let desired = int(&status["desiredNumberScheduled"]);
            let updated = int(&status["updatedNumberScheduled"]);
            let available = int(&status["numberAvailable"]);
            if updated < desired {
                Rollout::Progressing(format!("{updated}/{desired} nodes updated"))
            } else if available < desired {
                Rollout::Progressing(format!("{available}/{desired} nodes available"))
            } else {
                Rollout::Done(format!("{available}/{desired} nodes available"))
            }
        }
        other => Rollout::Done(format!("{other} applied")),
    }
}

/// Whether a CustomResourceDefinition is established (its API is served).
pub fn established(obj: &Value) -> Rollout {
    match condition(obj, "Established").and_then(|c| c["status"].as_str()) {
        Some("True") => Rollout::Done("established".into()),
        _ => match condition(obj, "NamesAccepted") {
            Some(c) if c["status"].as_str() == Some("False") => Rollout::Failed(
                c["message"]
                    .as_str()
                    .unwrap_or("its names were not accepted")
                    .to_owned(),
            ),
            _ => Rollout::Progressing("waiting to be established".into()),
        },
    }
}

/// The health of one live object (`None`: absent).
pub fn status(key: &ObjectKey, obj: Option<&Value>) -> HealthStatus {
    let Some(obj) = obj else {
        return HealthStatus::fail(format!("{key} does not exist"));
    };
    if is_rollout(key) {
        return match rollout(obj, None) {
            Rollout::Done(_) => HealthStatus::Pass,
            Rollout::Progressing(s) | Rollout::Failed(s) => HealthStatus::fail(s),
        };
    }
    match key.kind.as_str() {
        "Job" if key.group() == "batch" => match condition(obj, "Failed") {
            Some(c) if c["status"].as_str() == Some("True") => {
                HealthStatus::fail(c["message"].as_str().unwrap_or("the job failed").to_owned())
            }
            _ => HealthStatus::Pass,
        },
        "PersistentVolumeClaim" => match obj["status"]["phase"].as_str() {
            Some("Lost") => HealthStatus::fail("the claim lost its volume"),
            _ => HealthStatus::Pass,
        },
        "CustomResourceDefinition" => match established(obj) {
            Rollout::Done(_) => HealthStatus::Pass,
            Rollout::Progressing(s) | Rollout::Failed(s) => HealthStatus::fail(s),
        },
        _ => match condition(obj, "Ready").and_then(|c| c["status"].as_str()) {
            Some("False") => HealthStatus::fail(
                condition(obj, "Ready")
                    .and_then(|c| c["message"].as_str())
                    .unwrap_or("not ready")
                    .to_owned(),
            ),
            _ => HealthStatus::Pass,
        },
    }
}

/// Reasons a container is stuck rather than starting.
const STUCK: &[&str] = &[
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "CreateContainerConfigError",
    "CreateContainerError",
    "InvalidImageName",
];

/// The pods of a controller that are stuck, as `pod: reason` lines.
pub fn stuck_pods(pods: &Value) -> Vec<String> {
    let mut out = Vec::new();
    for pod in pods["items"].as_array().into_iter().flatten() {
        let name = pod["metadata"]["name"].as_str().unwrap_or("pod");
        let statuses = pod["status"]["initContainerStatuses"]
            .as_array()
            .into_iter()
            .flatten()
            .chain(
                pod["status"]["containerStatuses"]
                    .as_array()
                    .into_iter()
                    .flatten(),
            );
        for c in statuses {
            if let Some(reason) = c["state"]["waiting"]["reason"].as_str()
                && STUCK.contains(&reason)
            {
                out.push(format!(
                    "{name}/{}: {reason}",
                    c["name"].as_str().unwrap_or("container")
                ));
            }
        }
    }
    out
}

/// The implicit checks of one object: its own status and, for a
/// controller, its pods.
pub async fn object(kube: &Kube, key: &ObjectKey) -> Result<Vec<Check>> {
    let Some(resource) = kube.resource(&key.api_version, &key.kind).await? else {
        return Ok(vec![Check {
            name: key.key(),
            status: HealthStatus::fail(format!("the cluster no longer serves {}", key.kind)),
        }]);
    };
    let live = kube
        .get(&resource.path(&key.namespace, Some(&key.name)))
        .await?;
    let mut out = vec![Check {
        name: key.key(),
        status: status(key, live.as_ref()),
    }];
    if let Some(obj) = live.as_ref().filter(|_| is_rollout(key))
        && let Some(selector) = match_labels(obj)
    {
        let path = with_query(
            &format!(
                "/api/v1/namespaces/{}/pods",
                crate::client::seg(&key.namespace)
            ),
            "labelSelector",
            &selector,
        );
        let pods = kube.get(&path).await?.unwrap_or(Value::Null);
        let stuck = stuck_pods(&pods);
        out.push(Check {
            name: format!("{} pods", key.key()),
            status: if stuck.is_empty() {
                HealthStatus::Pass
            } else {
                HealthStatus::fail(stuck.join("; "))
            },
        });
    }
    Ok(out)
}

/// `spec.selector.matchLabels` as a label selector (`app=grafana,tier=web`).
fn match_labels(obj: &Value) -> Option<String> {
    let labels = obj["spec"]["selector"]["matchLabels"].as_object()?;
    if labels.is_empty() {
        return None;
    }
    Some(
        labels
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(","),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    fn deployment(status: Value) -> Value {
        json!({"kind": "Deployment", "metadata": {"generation": 2}, "spec": {"replicas": 2}, "status": status})
    }

    #[rstest]
    #[case::stale(json!({"observedGeneration": 1}), "progressing")]
    #[case::updating(json!({"observedGeneration": 2, "updatedReplicas": 1, "replicas": 2}), "progressing")]
    #[case::old_terminating(json!({"observedGeneration": 2, "updatedReplicas": 2, "replicas": 3, "availableReplicas": 2}), "progressing")]
    #[case::done(json!({"observedGeneration": 2, "updatedReplicas": 2, "replicas": 2, "availableReplicas": 2}), "done")]
    #[case::deadline(json!({"observedGeneration": 2, "conditions": [{"type": "Progressing", "status": "False", "message": "exceeded"}]}), "failed")]
    fn deployment_rollouts(#[case] status: Value, #[case] expected: &str) {
        let got = match rollout(&deployment(status), None) {
            Rollout::Done(_) => "done",
            Rollout::Progressing(_) => "progressing",
            Rollout::Failed(_) => "failed",
        };
        assert_eq!(got, expected);
    }

    #[test]
    fn stuck_pods_are_named() {
        let pods = json!({"items": [
            {"metadata": {"name": "g-1"}, "status": {"containerStatuses": [{"name": "grafana", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}},
            {"metadata": {"name": "g-2"}, "status": {"containerStatuses": [{"name": "grafana", "state": {"waiting": {"reason": "ContainerCreating"}}}]}},
        ]});
        assert_eq!(stuck_pods(&pods), vec!["g-1/grafana: CrashLoopBackOff"]);
    }

    #[rstest]
    #[case::absent(None, false)]
    #[case::ready(Some(json!({"status": {"conditions": [{"type": "Ready", "status": "True"}]}})), true)]
    #[case::not_ready(Some(json!({"status": {"conditions": [{"type": "Ready", "status": "False"}]}})), false)]
    #[case::plain(Some(json!({"data": {}})), true)]
    fn generic_objects_follow_ready(#[case] obj: Option<Value>, #[case] pass: bool) {
        let key = ObjectKey::new("v1", "ConfigMap", "default", "x");
        assert_eq!(status(&key, obj.as_ref()).is_pass(), pass);
    }
}
