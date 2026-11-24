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
    /// The namespaces the target owns. An object outside them (or one that
    /// belongs to no namespace) is refused on this target, and the cluster's
    /// own target refuses objects inside them.
    #[serde(default)]
    pub namespaces: Vec<String>,
}

impl ClusterScope {
    pub fn is_empty(&self) -> bool {
        self.namespaces.is_empty()
    }
}

/// The part of a cluster one target may touch, as an executor enforces it:
/// a scoped target stays inside its namespaces, and the cluster's own target
/// stays out of the namespaces handed to its scoped targets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bounds {
    /// A scoped target's namespaces; empty: the whole cluster.
    pub namespaces: Vec<String>,
    /// Namespaces that belong to the targets scoped to this cluster.
    pub excluded: Vec<String>,
}

impl Bounds {
    /// Whether the target is a scoped one.
    pub fn scoped(&self) -> bool {
        !self.namespaces.is_empty()
    }

    /// The namespace the target keeps its own bookkeeping in (its index):
    /// the first it owns, so a credential limited to its scope can write
    /// it; `default` for a whole cluster.
    pub fn home(&self) -> &str {
        self.namespaces.first().map_or("default", String::as_str)
    }

    /// Whether the target may hold objects in `namespace`.
    pub fn holds(&self, namespace: &str) -> bool {
        if self.scoped() {
            self.namespaces.iter().any(|n| n == namespace)
        } else {
            !self.excluded.iter().any(|n| n == namespace)
        }
    }

    /// Why `target` may not manage `what` in `namespace` (empty for an
    /// object that belongs to no namespace), when it may not.
    pub fn refusal(&self, target: &str, what: &str, namespace: &str) -> Option<String> {
        if namespace.is_empty() {
            return self.scoped().then(|| {
                format!(
                    "{what} is cluster-wide, and '{target}' is a scoped target (namespaces: {}); manage it from a workload on the cluster itself.",
                    self.namespaces.join(", ")
                )
            });
        }
        if self.holds(namespace) {
            return None;
        }
        Some(if self.scoped() {
            format!(
                "{what} is outside the scoped target '{target}' (namespaces: {}); place the workload on the target that owns '{namespace}'.",
                self.namespaces.join(", ")
            )
        } else {
            format!(
                "{what} is in the namespace '{namespace}', which belongs to a target scoped to '{target}'; place the workload on that target."
            )
        })
    }
}

/// Fills what each scoped target leaves out from the cluster it is a part
/// of: how to reach it (`api`, `ui`, `identity`), who its members and
/// executors are (`nodes`, `executors`) and how often it is read (`watch`).
/// `raw` is the `fleet.clusters` mapping as written, which says what was
/// left out.
pub(super) fn inherit(clusters: &mut IndexMap<String, ClusterDecl>, raw: &serde_yaml::Value) {
    let parents: Vec<(String, ClusterDecl)> = clusters
        .iter()
        .filter(|(_, c)| c.cluster.is_none())
        .map(|(n, c)| (n.clone(), c.clone()))
        .collect();
    for (name, decl) in clusters.iter_mut() {
        let Some((_, parent)) = decl
            .cluster
            .as_ref()
            .and_then(|p| parents.iter().find(|(n, _)| n == p))
        else {
            continue;
        };
        let written = |key: &str| raw[name.as_str()].get(key).is_some();
        if !written("api") {
            decl.api = parent.api.clone();
        }
        if !written("ui") {
            decl.ui = parent.ui.clone();
        }
        if !written("identity") {
            decl.identity = parent.identity.clone();
        }
        if !written("nodes") {
            decl.nodes = parent.nodes.clone();
        }
        if !written("executors") {
            decl.executors = parent.executors.clone();
        }
        if !written("watch") {
            decl.watch = parent.watch;
        }
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
    /// Where people open the cluster's web UI (`https://nomad.example:4646`),
    /// for the UI's links to each workload. Absent: `api`, when it is HTTP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<String>,
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
    /// Which workload identities may speak for the cluster from inside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust: Option<ClusterTrust>,
    /// How often the cluster's steward reads it when nothing wakes it.
    #[serde(default, skip_serializing_if = "ClusterWatch::is_default")]
    pub watch: ClusterWatch,
    /// The node pools a host may join, read top to bottom: a `cluster.node`
    /// `join` is for the first entry the host matches. None declared: the
    /// scheduler's default pool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pools: Vec<ClusterPool>,
    /// What a rollout does with a host's node of this cluster around a run
    /// that disrupts the node's work: take it out of scheduling first, and
    /// hand it back once the host is healthy. Absent: nothing (`never`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<NodeMaintenance>,
}

/// `maintenance` on a cluster: whether a rollout drains a host's node
/// before it applies a run that disrupts the node, and how long the node's
/// work gets to move. The cluster's steward drains and undrains the node,
/// as for a `cluster.node` task; the host holds no cluster credential.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeMaintenance {
    #[serde(default)]
    pub drain: DrainPolicy,
    /// How long the node's work gets to move before the scheduler stops
    /// it. Absent: the steward's default (one hour).
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub deadline: Option<std::time::Duration>,
}

/// Which runs take a host's node out of scheduling first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DrainPolicy {
    /// None: the node keeps its work through every run.
    #[default]
    Never,
    /// Runs that reboot the host, or change a module marked
    /// `disrupts: node`.
    Disruptive,
    /// Every run that changes something on the host.
    Always,
}

impl DrainPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            DrainPolicy::Never => "never",
            DrainPolicy::Disruptive => "disruptive",
            DrainPolicy::Always => "always",
        }
    }

    /// Whether a run drains the node first: `changes` says whether it
    /// changes anything at all, `disruptions` why it disrupts the node
    /// (see [`crate::engine::maintenance`]).
    pub fn drains(self, changes: bool, disruptions: &[String]) -> bool {
        match self {
            DrainPolicy::Never => false,
            DrainPolicy::Disruptive => changes && !disruptions.is_empty(),
            DrainPolicy::Always => changes,
        }
    }
}

impl std::fmt::Display for DrainPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry of a cluster's `pools`: a pool's name, or a name with the
/// `when` choosing its hosts. `when` is a trust selector (it reads only
/// `host.id`, verified tags and reserved labels); without one the entry
/// matches every node.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "PoolRepr", into = "PoolRepr")]
pub struct ClusterPool {
    pub name: String,
    pub when: Option<Filter>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum PoolRepr {
    Name(String),
    Entry(PoolEntry),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolEntry {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    when: Option<Filter>,
}

impl From<PoolRepr> for ClusterPool {
    fn from(repr: PoolRepr) -> Self {
        match repr {
            PoolRepr::Name(name) => Self { name, when: None },
            PoolRepr::Entry(PoolEntry { name, when }) => Self { name, when },
        }
    }
}

impl From<ClusterPool> for PoolRepr {
    fn from(pool: ClusterPool) -> Self {
        match pool.when {
            None => PoolRepr::Name(pool.name),
            when => PoolRepr::Entry(PoolEntry {
                name: pool.name,
                when,
            }),
        }
    }
}

/// The node pool a host joins a cluster in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolChoice<'a> {
    /// The cluster declares no pools: the scheduler's default one.
    Default,
    Named(&'a str),
    /// Pools are declared and the host matches none of them.
    Refused,
}

/// How often a cluster's steward (the agent leasing it) reads the cluster
/// unprompted: health every `health` when no change woke it (Nomad's
/// blocking queries and Kubernetes' watch do between), and the applied
/// revision re-planned for drift every `drift`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterWatch {
    #[serde(default = "ClusterWatch::default_health", with = "humantime_serde")]
    pub health: std::time::Duration,
    #[serde(default = "ClusterWatch::default_drift", with = "humantime_serde")]
    pub drift: std::time::Duration,
}

impl ClusterWatch {
    fn default_health() -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }

    fn default_drift() -> std::time::Duration {
        std::time::Duration::from_secs(10 * 60)
    }

    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl Default for ClusterWatch {
    fn default() -> Self {
        Self {
            health: Self::default_health(),
            drift: Self::default_drift(),
        }
    }
}

/// How an in-cluster executor proves it speaks for the cluster: a workload
/// identity token (Nomad `identity` block, projected ServiceAccount token)
/// signed by one of the cluster's keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterTrust {
    /// Where the server reads the cluster's signing keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jwks_url: Option<String>,
    /// The cluster's key set (a JWKS document, as a mapping or JSON text),
    /// for a cluster the server cannot reach.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<serde_json::Value>,
    /// The `iss` tokens must carry; absent: any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
    /// `namespace/job` (Nomad) or `sub` (Kubernetes) of the executors.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<String>,
}

/// Why a cluster's trust cannot be used, if it cannot: it must say where the
/// keys are or carry them, as an `http(s)` URL and a JWKS document.
pub fn trust_problem(jwks_url: Option<&str>, keys: Option<&str>) -> Option<String> {
    if jwks_url.is_none() && keys.is_none() {
        return Some(
            "`trust` needs `jwks_url` or `keys`, so the server knows the cluster's signing keys"
                .to_owned(),
        );
    }
    if let Some(url) = jwks_url
        && !(url.starts_with("https://") || url.starts_with("http://"))
    {
        return Some(format!("`trust.jwks_url` '{url}' must be an http(s) URL"));
    }
    if let Some(keys) = keys {
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(keys);
        let has_keys = parsed
            .as_ref()
            .ok()
            .and_then(|v| v.get("keys"))
            .and_then(|k| k.as_array())
            .is_some_and(|k| !k.is_empty());
        if !has_keys {
            return Some(
                "`trust.keys` must be a JWKS document: `{ keys: [ ... ] }` with at least one key"
                    .to_owned(),
            );
        }
    }
    None
}

impl ClusterTrust {
    /// Why this trust cannot be used, if it cannot.
    pub fn problem(&self) -> Option<String> {
        trust_problem(self.jwks_url.as_deref(), self.keys_json().as_deref())
    }

    /// The pinned key set as JSON text.
    pub fn keys_json(&self) -> Option<String> {
        self.keys.as_ref().map(|k| match k {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    }
}

impl ClusterDecl {
    /// The selector choosing executors: `executors`, else `nodes`.
    pub fn executors(&self) -> Option<&Filter> {
        self.executors.as_ref().or(self.nodes.as_ref())
    }

    /// The pool `trusted` (a host's trust context) joins the cluster in: the
    /// first entry of `pools` it matches.
    pub fn pool(&self, trusted: &crate::selectors::HostContext) -> PoolChoice<'_> {
        if self.pools.is_empty() {
            return PoolChoice::Default;
        }
        self.pools
            .iter()
            .find(|p| p.when.as_ref().is_none_or(|f| trusted.matches(f)))
            .map_or(PoolChoice::Refused, |p| PoolChoice::Named(&p.name))
    }

    /// How a rollout drains this cluster's nodes, when it drains them at
    /// all: a cluster with `nodes` of its own (not a scoped target) whose
    /// `maintenance.drain` is not `never`.
    pub fn drains(&self) -> Option<&NodeMaintenance> {
        self.maintenance.as_ref().filter(|m| {
            m.drain != DrainPolicy::Never && self.cluster.is_none() && self.nodes.is_some()
        })
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

    #[rstest::rstest]
    #[case::scoped_inside(&["edge", "public"], &[], "edge", true)]
    #[case::scoped_outside(&["edge", "public"], &[], "default", false)]
    #[case::scoped_cluster_wide(&["edge"], &[], "", false)]
    #[case::cluster_free(&[], &["edge"], "default", true)]
    #[case::cluster_in_a_scope(&[], &["edge"], "edge", false)]
    #[case::cluster_wide_on_the_cluster(&[], &["edge"], "", true)]
    #[case::unbounded(&[], &[], "anything", true)]
    fn a_target_manages_only_what_its_bounds_hold(
        #[case] namespaces: &[&str],
        #[case] excluded: &[&str],
        #[case] namespace: &str,
        #[case] allowed: bool,
    ) {
        let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
        let bounds = Bounds {
            namespaces: owned(namespaces),
            excluded: owned(excluded),
        };
        let refusal = bounds.refusal("t", "job x", namespace);
        assert_eq!(refusal.is_none(), allowed, "{refusal:?}");
    }

    #[test]
    fn a_scoped_target_keeps_its_bookkeeping_in_its_own_namespace() {
        let scoped = Bounds {
            namespaces: vec!["edge".into(), "public".into()],
            excluded: Vec::new(),
        };
        assert_eq!(scoped.home(), "edge");
        assert_eq!(Bounds::default().home(), "default");
    }
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

    #[rstest::rstest]
    #[case::disruptive("drain: disruptive\n    deadline: 10m", Some((DrainPolicy::Disruptive, Some(600))))]
    #[case::always("drain: always", Some((DrainPolicy::Always, None)))]
    // Written but `never`: today's behaviour, nothing is drained.
    #[case::never("drain: never", None)]
    #[case::only_a_deadline("deadline: 5m", None)]
    fn maintenance_says_which_runs_drain_the_node(
        #[case] maintenance: &str,
        #[case] drains: Option<(DrainPolicy, Option<u64>)>,
    ) {
        let decl: ClusterDecl = serde_yaml::from_str(&format!(
            "kind: kubernetes\nnodes: 'host.groups contains \"k3s\"'\nmaintenance:\n    {maintenance}\n"
        ))
        .unwrap();
        assert_eq!(
            decl.drains()
                .map(|m| (m.drain, m.deadline.map(|d| d.as_secs()))),
            drains
        );
    }

    #[test]
    fn a_cluster_drains_nothing_without_nodes_or_maintenance() {
        let without_maintenance: ClusterDecl =
            serde_yaml::from_str("kind: nomad\nnodes: 'true'\n").unwrap();
        assert!(without_maintenance.drains().is_none());
        let without_nodes: ClusterDecl = serde_yaml::from_str(
            "kind: nomad\nexecutors: 'true'\nmaintenance: { drain: always }\n",
        )
        .unwrap();
        assert!(without_nodes.drains().is_none());
        assert!(
            serde_yaml::from_str::<ClusterDecl>(
                "kind: nomad\nnodes: 'true'\nmaintenance: { drain: sometimes }\n"
            )
            .is_err()
        );
        assert!(
            serde_yaml::from_str::<ClusterDecl>(
                "kind: nomad\nnodes: 'true'\nmaintenance: { drain: always, grace: 1m }\n"
            )
            .is_err()
        );
    }
}
