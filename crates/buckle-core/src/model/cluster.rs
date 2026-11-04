//! `fleet.clusters` in `buckle.yaml`: the clusters a revision declares as
//! targets. A cluster is a virtual target (design: cluster workloads, D1 and
//! D6): nothing enrols for it; the server hands its workloads to an
//! executor, a host agent matching `executors`, with a credential Buckle
//! issues for the occasion.

use crate::Filter;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// The scheduler a cluster target speaks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClusterKind {
    Nomad,
    Kubernetes,
}

impl ClusterKind {
    /// The name as written in `buckle.yaml` and reported by the API.
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

impl std::str::FromStr for ClusterKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "nomad" => Ok(ClusterKind::Nomad),
            "kubernetes" | "k8s" => Ok(ClusterKind::Kubernetes),
            other => Err(format!(
                "'{other}' is not a cluster kind; expected nomad or kubernetes"
            )),
        }
    }
}

/// How an executor authenticates to the cluster. Written as one string:
/// `buckle_jwt` (the default), `local`, `workload` or `secret://NAME`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ClusterIdentity {
    /// A short-lived JWT the Buckle server signs, exchanged through the
    /// cluster's JWT trust (a Nomad `jwt` auth method, a Kubernetes JWT
    /// authenticator).
    #[default]
    BuckleJwt,
    /// The executor host's own credential (root's kubeconfig on a k3s
    /// server, a Nomad management token in the agent's environment).
    Local,
    /// The executor runs inside the cluster and uses the identity its
    /// scheduler gave it (a Nomad workload identity, a projected
    /// ServiceAccount token).
    Workload,
    /// A static credential from the named `buckle.yaml` secret.
    Secret(String),
}

impl ClusterIdentity {
    /// Parses the `identity` field.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if let Some(name) = raw.strip_prefix("secret://") {
            if name.is_empty() {
                return Err("`secret://` must name a secret declared in buckle.yaml".to_owned());
            }
            return Ok(ClusterIdentity::Secret(name.to_owned()));
        }
        match raw.to_ascii_lowercase().as_str() {
            "" | "buckle_jwt" | "buckle-jwt" | "jwt" => Ok(ClusterIdentity::BuckleJwt),
            "local" => Ok(ClusterIdentity::Local),
            "workload" => Ok(ClusterIdentity::Workload),
            other => Err(format!(
                "'{other}' is not a cluster identity; expected buckle_jwt, local, workload or secret://NAME"
            )),
        }
    }

    /// The secret a `secret://NAME` identity reads, if it is one.
    pub fn secret(&self) -> Option<&str> {
        match self {
            ClusterIdentity::Secret(name) => Some(name),
            _ => None,
        }
    }
}

impl std::fmt::Display for ClusterIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClusterIdentity::BuckleJwt => f.write_str("buckle_jwt"),
            ClusterIdentity::Local => f.write_str("local"),
            ClusterIdentity::Workload => f.write_str("workload"),
            ClusterIdentity::Secret(name) => write!(f, "secret://{name}"),
        }
    }
}

impl Serialize for ClusterIdentity {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ClusterIdentity {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        ClusterIdentity::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// The part of a cluster a scoped target may touch (D12).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterScope {
    /// The namespaces the target owns; workloads whose objects fall outside
    /// them are skipped on this target.
    #[serde(default)]
    pub namespaces: Vec<String>,
}

impl ClusterScope {
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty()
    }
}

/// One entry of `fleet.clusters`: a cluster target as the repository declares
/// it. The same shape is created through `POST /api/v1/targets` for clusters
/// Buckle did not build; where both exist the repository wins on the fields
/// it sets.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterDecl {
    pub kind: ClusterKind,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// The cluster's API as an executor reaches it (`http://127.0.0.1:4646`,
    /// `https://10.0.0.1:6443`). Absent: the executor's local default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Which hosts are members of the cluster: what a workload's `module:`
    /// edge and a module's `workload:` edge refer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<Filter>,
    /// Which hosts may speak for the cluster. Defaults to `nodes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executors: Option<Filter>,
    #[serde(default)]
    pub identity: ClusterIdentity,
    /// Labels, as a host's agent labels: operators may add to them through
    /// the API and the UI, and `cluster.labels.*` selects on the result.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub labels: IndexMap<String, serde_yaml::Value>,
    /// For a scoped target: the cluster (another `fleet.clusters` entry, or
    /// an API-created target) this one is a part of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster: Option<String>,
    #[serde(default, skip_serializing_if = "ClusterScope::is_empty")]
    pub scope: ClusterScope,
}

impl ClusterDecl {
    /// The selector choosing executors: `executors`, else `nodes`.
    pub fn executors(&self) -> Option<&Filter> {
        self.executors.as_ref().or(self.nodes.as_ref())
    }

    /// The labels as text, the way host labels travel (`true`, `3`, `home`).
    pub fn text_labels(&self) -> std::collections::BTreeMap<String, String> {
        self.labels
            .iter()
            .map(|(k, v)| (k.clone(), yaml_scalar_text(v)))
            .collect()
    }
}

/// A YAML scalar as the text an operator would have typed for a host label.
pub fn yaml_scalar_text(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Null => String::new(),
        other => serde_yaml::to_string(other)
            .map(|s| s.trim().to_owned())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("buckle_jwt", ClusterIdentity::BuckleJwt)]
    #[case("", ClusterIdentity::BuckleJwt)]
    #[case("Local", ClusterIdentity::Local)]
    #[case("workload", ClusterIdentity::Workload)]
    #[case("secret://nomad/home/token", ClusterIdentity::Secret("nomad/home/token".into()))]
    fn identities_parse_from_their_written_form(
        #[case] raw: &str,
        #[case] expected: ClusterIdentity,
    ) {
        assert_eq!(ClusterIdentity::parse(raw).unwrap(), expected);
        let yaml = serde_yaml::to_string(&expected).unwrap();
        let again: ClusterIdentity = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(again, expected);
    }

    #[rstest]
    #[case("secret://")]
    #[case("password")]
    fn bad_identities_are_rejected(#[case] raw: &str) {
        assert!(ClusterIdentity::parse(raw).is_err());
    }

    #[test]
    fn a_declaration_parses_and_defaults_executors_to_nodes() {
        let decl: ClusterDecl = serde_yaml::from_str(
            r#"
kind: nomad
api: http://127.0.0.1:4646
nodes: 'host.groups contains "nomad-clients"'
labels: { cluster: home, canary: true, rank: 3 }
"#,
        )
        .unwrap();
        assert_eq!(decl.kind, ClusterKind::Nomad);
        assert_eq!(decl.identity, ClusterIdentity::BuckleJwt);
        assert_eq!(
            decl.executors().map(|f| f.raw().to_owned()),
            Some("host.groups contains \"nomad-clients\"".to_owned())
        );
        let labels = decl.text_labels();
        assert_eq!(labels["cluster"], "home");
        assert_eq!(labels["canary"], "true");
        assert_eq!(labels["rank"], "3");
    }

    #[test]
    fn a_scoped_target_names_its_cluster() {
        let decl: ClusterDecl = serde_yaml::from_str(
            "kind: kubernetes\ncluster: lab-k3s\nscope: { namespaces: [team-a] }\nexecutors: 'host.labels.role == \"k3s-server\"'\nidentity: local\n",
        )
        .unwrap();
        assert_eq!(decl.cluster.as_deref(), Some("lab-k3s"));
        assert_eq!(decl.scope.namespaces, vec!["team-a"]);
        assert_eq!(decl.identity, ClusterIdentity::Local);
        assert!(decl.nodes.is_none() && decl.executors().is_some());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(serde_yaml::from_str::<ClusterDecl>("kind: nomad\ntoken: abc\n").is_err());
        assert!(serde_yaml::from_str::<ClusterDecl>("kind: swarm\n").is_err());
    }
}
