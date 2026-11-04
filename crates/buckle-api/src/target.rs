//! Targets: what a revision is applied to. A host is a target with an agent;
//! a cluster is a virtual target (a Nomad or Kubernetes API) that hosts speak
//! for. Both share the `/api/v1/targets` routes and the `HostSummary` row;
//! `kind` and `cluster` say which is which.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What kind of thing a target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    #[default]
    Host,
    Cluster,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TargetKind::Host => "host",
            TargetKind::Cluster => "cluster",
        }
    }
}

impl std::fmt::Display for TargetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TargetKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "host" => Ok(TargetKind::Host),
            "cluster" => Ok(TargetKind::Cluster),
            other => Err(format!("'{other}' is not a target kind (host, cluster)")),
        }
    }
}

/// The scheduler a cluster target speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterKind {
    Nomad,
    Kubernetes,
}

impl ClusterKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ClusterKind::Nomad => "nomad",
            ClusterKind::Kubernetes => "kubernetes",
        }
    }
}

impl std::fmt::Display for ClusterKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The part of a cluster a scoped target may touch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterScope {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
}

impl ClusterScope {
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty()
    }
}

/// What makes a target a cluster: the same fields as a `fleet.clusters`
/// entry in `buckle.yaml`, so a declared cluster and one created through
/// `POST /api/v1/targets` are described alike. Selectors are selector text
/// over `host.*`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterSpec {
    pub kind: ClusterKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The cluster's API as an executor reaches it. Absent: the executor's
    /// local default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Which hosts are members of the cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<String>,
    /// Which hosts may speak for the cluster. Absent: `nodes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executors: Option<String>,
    /// How an executor authenticates: `buckle_jwt` (default), `local`,
    /// `workload` or `secret://NAME`.
    #[serde(default = "ClusterSpec::default_identity")]
    pub identity: String,
    /// For a scoped target: the cluster it is a part of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default, skip_serializing_if = "ClusterScope::is_empty")]
    pub scope: ClusterScope,
    /// How executors running inside the cluster prove they speak for it
    /// (`agents.auth: workload_identity`). Absent: none may.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<Box<ClusterTrust>>,
}

/// Which workload identities the server accepts as the cluster's in-cluster
/// executor: tokens signed by one of the cluster's keys, for `buckle`, naming
/// one of `subjects`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterTrust {
    /// Where the server reads the cluster's signing keys (Nomad:
    /// `/.well-known/jwks.json`, Kubernetes: `/openid/v1/jwks`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_url: Option<String>,
    /// The key set itself (JWKS JSON), for a cluster the server cannot reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<String>,
    /// The `iss` the tokens must carry; absent: any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// Who may speak for the cluster: `namespace/job` for Nomad, the `sub`
    /// for Kubernetes. Empty: the kind's default executor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<String>,
}

impl ClusterTrust {
    /// The subjects a token may name, with the kind's default.
    pub fn subjects(&self, kind: ClusterKind) -> Vec<String> {
        if !self.subjects.is_empty() {
            return self.subjects.clone();
        }
        vec![match kind {
            ClusterKind::Nomad => "default/buckle-executor".to_owned(),
            ClusterKind::Kubernetes => {
                "system:serviceaccount:buckle-system:buckle-executor".to_owned()
            }
        }]
    }
}

impl ClusterSpec {
    pub fn default_identity() -> String {
        "buckle_jwt".to_owned()
    }

    /// The selector choosing executors: `executors`, else `nodes`.
    pub fn executors(&self) -> Option<&str> {
        self.executors.as_deref().or(self.nodes.as_deref())
    }
}

/// The cluster half of a target's summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterInfo {
    #[serde(flatten)]
    pub spec: ClusterSpec,
    /// Whether `buckle.yaml` declares the cluster (its spec and seed labels
    /// follow the repository) or an operator created it through the API.
    #[serde(default)]
    pub declared: bool,
}

/// `POST /api/v1/targets`: creates a target that has no agent to register
/// it. Only clusters can be created this way; hosts register themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTarget {
    pub id: String,
    #[serde(default)]
    pub kind: TargetKind,
    /// Required when `kind` is `cluster`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<ClusterSpec>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_cluster_spec_defaults_its_identity_and_scope() {
        let spec: ClusterSpec = serde_json::from_value(json!({
            "kind": "nomad", "nodes": "host.groups contains \"nomad\""
        }))
        .unwrap();
        assert_eq!(spec.identity, "buckle_jwt");
        assert!(spec.scope.is_empty());
        assert_eq!(spec.executors(), Some("host.groups contains \"nomad\""));
        let text = serde_json::to_value(&spec).unwrap();
        assert_eq!(text.get("scope"), None);
        assert_eq!(text["identity"], "buckle_jwt");
    }

    #[test]
    fn target_kinds_are_snake_case_and_default_to_host() {
        assert_eq!(
            serde_json::to_string(&TargetKind::Cluster).unwrap(),
            "\"cluster\""
        );
        assert_eq!(TargetKind::default(), TargetKind::Host);
        assert_eq!("cluster".parse::<TargetKind>(), Ok(TargetKind::Cluster));
        assert!("pod".parse::<TargetKind>().is_err());
        let create: CreateTarget = serde_json::from_value(json!({ "id": "home-nomad" })).unwrap();
        assert_eq!(create.kind, TargetKind::Host);
        assert_eq!(create.cluster, None);
    }
}
