//! The target's index variable: the one object Buckle writes that is not a
//! managed object. It lists the Nomad entities that carry no metadata of
//! their own (variables and host volumes), so that the executor can find
//! what a removed workload left behind without keeping state.
//!
//! It lives at `buckle/<target>/index` in the `default` namespace and holds
//! no secret: item names and fingerprint digests only.

use crate::client::{self, Nomad};
use crate::primitives::Kind;
use buckle_core::Result;
use buckle_core::error::system;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The namespace the index variable lives in.
pub const NAMESPACE: &str = "default";

/// The index variable's path for `target`.
pub fn path(target: &str) -> String {
    format!("buckle/{target}/index")
}

/// One entity the index lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub kind: Kind,
    pub namespace: String,
    pub name: String,
    pub module: String,
    pub revision: String,
    pub digest: String,
    /// For variables: the item names Buckle wrote, so that an item dropped
    /// from the workload is removed while items operators added stay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// For volumes: the id Nomad assigned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

impl Entry {
    pub fn is(&self, kind: Kind, namespace: &str, name: &str) -> bool {
        self.kind == kind && self.namespace == namespace && self.name == name
    }
}

/// The index as read, with the index to check-and-set against.
#[derive(Debug, Clone, Default)]
pub struct Index {
    pub entries: Vec<Entry>,
    pub modify_index: u64,
}

impl Index {
    pub fn find(&self, kind: Kind, namespace: &str, name: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.is(kind, namespace, name))
    }
}

#[derive(Deserialize)]
struct Variable {
    #[serde(rename = "Items", default)]
    items: std::collections::BTreeMap<String, String>,
    #[serde(rename = "ModifyIndex", default)]
    modify_index: u64,
}

fn url(target: &str) -> String {
    format!(
        "/v1/var/{}?namespace={}",
        client::path(&path(target)),
        NAMESPACE
    )
}

/// Reads the index; an absent index is empty.
pub async fn read(nomad: &Nomad, target: &str) -> Result<Index> {
    let Some(var) = nomad.get::<Variable>(&url(target)).await? else {
        return Ok(Index::default());
    };
    let entries = match var.items.get("entries") {
        Some(text) => serde_json::from_str(text).map_err(|e| {
            system(
                format!("The index variable {} could not be read ({e}).", path(target)),
                &["Delete the variable; the next apply lists the target's variables and volumes again."],
            )
        })?,
        None => Vec::new(),
    };
    Ok(Index {
        entries,
        modify_index: var.modify_index,
    })
}

/// Read-modify-writes the index with check-and-set, retrying when another
/// workload of the same target wrote it in between.
pub async fn update(nomad: &Nomad, target: &str, change: impl Fn(&mut Vec<Entry>)) -> Result<()> {
    for _ in 0..8 {
        let mut index = read(nomad, target).await?;
        let before = index.entries.clone();
        change(&mut index.entries);
        if index.entries == before {
            return Ok(());
        }
        index
            .entries
            .sort_by(|a, b| (a.kind, &a.namespace, &a.name).cmp(&(b.kind, &b.namespace, &b.name)));
        let body = json!({
            "Namespace": NAMESPACE,
            "Path": path(target),
            "Items": {"entries": serde_json::to_string(&index.entries).unwrap_or_default()},
        });
        let resp = nomad
            .send(
                "PUT",
                &format!("{}&cas={}", url(target), index.modify_index),
                Some(serde_json::to_vec(&body).unwrap_or_default()),
            )
            .await?;
        if resp.status == 409 {
            continue;
        }
        nomad.ok("PUT", &url(target), resp)?;
        return Ok(());
    }
    Err(system(
        format!(
            "The index variable {} kept changing while it was written.",
            path(target)
        ),
        &["Retry the apply; another executor may be applying the same target."],
    ))
}

/// Adds or replaces `entry`.
pub fn upsert(entries: &mut Vec<Entry>, entry: Entry) {
    entries.retain(|e| !e.is(entry.kind, &entry.namespace, &entry.name));
    entries.push(entry);
}

/// Drops the entry for `kind`/`namespace`/`name`.
pub fn forget(entries: &mut Vec<Entry>, kind: Kind, namespace: &str, name: &str) {
    entries.retain(|e| !e.is(kind, namespace, name));
}
