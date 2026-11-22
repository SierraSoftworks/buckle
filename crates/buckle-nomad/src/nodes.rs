//! Node operations a cluster's steward performs for the cluster's nodes.
//!
//! A host never holds `node:write`: it asks the Buckle server, which checks
//! that the host is a node of the cluster and hands the request to the
//! steward. Each function here acts on the one live node of a given name.

use crate::client::{self, Nomad};
use buckle_core::Result;
use buckle_core::error::user;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// A node as the scheduler sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub name: String,
    /// Whether the scheduler places work on it.
    pub eligible: bool,
    pub draining: bool,
    pub pool: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Stub {
    #[serde(rename = "ID")]
    id: String,
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    scheduling_eligibility: String,
    #[serde(default)]
    drain: bool,
    #[serde(default)]
    node_pool: String,
}

/// The live node that carries one of `names` (the names of one host): one
/// that is not `down` (a reinstalled host leaves its previous node behind
/// until Nomad collects it). `None` when there is none; an error when
/// several are live, since nothing says which one the host is.
pub async fn find(nomad: &Nomad, names: &[&str]) -> Result<Option<Node>> {
    let filter = names
        .iter()
        .map(|name| format!("Name == {}", json!(name)))
        .collect::<Vec<_>>()
        .join(" or ");
    let path = client::with_query("/v1/nodes", "filter", &filter);
    let stubs: Vec<Stub> = nomad.get(&path).await?.unwrap_or_default();
    let mut live = stubs
        .into_iter()
        .filter(|n| names.contains(&n.name.as_str()) && n.status != "down");
    let Some(node) = live.next() else {
        return Ok(None);
    };
    if live.next().is_some() {
        return Err(user(
            format!(
                "More than one live Nomad node is named {}.",
                names.join(" or ")
            ),
            &["Give every client its own `name`, or remove the stale node with `nomad node gc`."],
        ));
    }
    Ok(Some(Node {
        id: node.id,
        name: node.name,
        eligible: node.scheduling_eligibility == "eligible",
        draining: node.drain,
        pool: node.node_pool,
    }))
}

/// Hands `node` back to the scheduler: stops a drain under way, or marks a
/// drained node eligible. Returns whether anything changed.
pub async fn undrain(nomad: &Nomad, node: &Node) -> Result<bool> {
    if node.draining {
        let path = format!("/v1/node/{}/drain", client::seg(&node.id));
        let _: Value = nomad
            .write(
                "POST",
                &path,
                &json!({"DrainSpec": null, "MarkEligible": true}),
            )
            .await?;
        Ok(true)
    } else if !node.eligible {
        let path = format!("/v1/node/{}/eligibility", client::seg(&node.id));
        let _: Value = nomad
            .write("POST", &path, &json!({"Eligibility": "eligible"}))
            .await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Starts draining `node` (its allocations get `deadline` to move; system
/// jobs stay until the others left) unless it is already out of scheduling:
/// a finished drain leaves a node ineligible. Returns whether a drain was
/// started; Nomad carries it out on its own.
pub async fn drain(nomad: &Nomad, node: &Node, deadline: Duration) -> Result<bool> {
    if node.draining || !node.eligible {
        return Ok(false);
    }
    let path = format!("/v1/node/{}/drain", client::seg(&node.id));
    let spec = json!({
        "DrainSpec": {"Deadline": deadline.as_nanos() as u64, "IgnoreSystemJobs": false},
        "MarkEligible": false,
    });
    let _: Value = nomad.write("POST", &path, &spec).await?;
    Ok(true)
}

/// A client introduction token for a node named `name` in `pool` (`None`:
/// Nomad's `default` pool), valid for `ttl`.
pub async fn introduction_token(
    nomad: &Nomad,
    name: &str,
    pool: Option<&str>,
    ttl: Duration,
) -> Result<SecretString> {
    #[derive(Deserialize)]
    struct Token {
        #[serde(rename = "JWT")]
        jwt: String,
    }
    let mut body = json!({"NodeName": name, "TTL": format!("{}s", ttl.as_secs().max(1))});
    if let Some(pool) = pool.filter(|p| !p.is_empty()) {
        body["NodePool"] = json!(pool);
    }
    let token: Token = nomad
        .write("POST", "/v1/acl/identity/client-introduction-token", &body)
        .await?;
    Ok(token.jwt.into())
}
