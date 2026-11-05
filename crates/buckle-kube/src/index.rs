//! The target's index: one ConfigMap (`default/buckle-index-<target>`)
//! listing every object Buckle applied for the target, so objects of a
//! workload (or a manifest document) that went away are found without
//! listing every resource type in the cluster. The objects themselves carry
//! the ownership labels; an index entry whose object lost them (or was
//! deleted) is not an orphan.

use crate::client::{Kube, seg};
use crate::object::{ObjectKey, label_value};
use buckle_core::Result;
use buckle_core::error::system;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The namespace of every target's index ConfigMap.
pub const NAMESPACE: &str = "default";

/// The ConfigMap name of `target`'s index.
pub fn name(target: &str) -> String {
    format!("buckle-index-{}", label_value(&target.to_ascii_lowercase()))
}

fn path(target: &str) -> String {
    format!(
        "/api/v1/namespaces/{NAMESPACE}/configmaps/{}",
        seg(&name(target))
    )
}

/// One applied object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(flatten)]
    pub key: ObjectKey,
    pub workload: String,
}

/// The index as read, with the version it was read at.
#[derive(Debug, Clone, Default)]
pub struct Index {
    pub entries: Vec<Entry>,
    /// Empty when the ConfigMap does not exist yet.
    resource_version: String,
}

/// Reads `target`'s index; empty when it does not exist yet.
pub async fn read(kube: &Kube, target: &str) -> Result<Index> {
    let Some(cm) = kube.get(&path(target)).await? else {
        return Ok(Index::default());
    };
    let entries = cm["data"]["entries"]
        .as_str()
        .map(|s| serde_json::from_str(s).unwrap_or_default())
        .unwrap_or_default();
    Ok(Index {
        entries,
        resource_version: cm["metadata"]["resourceVersion"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    })
}

/// Read-modify-writes the index under its resourceVersion, retrying when
/// another workload of the same target wrote it in between.
pub async fn update(kube: &Kube, target: &str, change: impl Fn(&mut Vec<Entry>)) -> Result<()> {
    let _held = kube.index_lock.lock().await;
    for attempt in 0..8u32 {
        if attempt > 0 {
            // Another executor of the same target wrote in between.
            tokio::time::sleep(std::time::Duration::from_millis(25 << attempt.min(5))).await;
        }
        let mut index = read(kube, target).await?;
        let before = index.entries.clone();
        change(&mut index.entries);
        if index.entries == before {
            return Ok(());
        }
        index.entries.sort_by(|a, b| a.key.cmp(&b.key));
        let mut body = json!({
            "apiVersion": "v1",
            "kind": "ConfigMap",
            "metadata": {
                "name": name(target),
                "namespace": NAMESPACE,
                "labels": {crate::object::TARGET_LABEL: label_value(target)},
            },
            "data": {"entries": serde_json::to_string(&index.entries).unwrap_or_default()},
        });
        let written = if index.resource_version.is_empty() {
            kube.create(&format!("/api/v1/namespaces/{NAMESPACE}/configmaps"), &body)
                .await?
        } else {
            body["metadata"]["resourceVersion"] = json!(index.resource_version);
            kube.replace(&path(target), &body).await?
        };
        if written {
            return Ok(());
        }
    }
    Err(system(
        format!(
            "The index ConfigMap {NAMESPACE}/{} kept changing while it was written.",
            name(target)
        ),
        &["Retry the apply; another executor may be applying the same target."],
    ))
}

/// Adds or replaces the entry for `entry.key`.
pub fn upsert(entries: &mut Vec<Entry>, entry: Entry) {
    entries.retain(|e| e.key != entry.key);
    entries.push(entry);
}

/// Drops the entry for `key`.
pub fn forget(entries: &mut Vec<Entry>, key: &ObjectKey) {
    entries.retain(|e| &e.key != key);
}
