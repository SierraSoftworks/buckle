//! The filt-rs context for a target: everything a `when:` selector, a group
//! definition or a phase selector may reference, as flat dotted keys.
//!
//! * on a host: `host.id`, `host.labels.<k>`, `host.groups` (tuple),
//!   `host.facts.<path.to.leaf>` (facts tree, leaves only; objects are null)
//! * on a cluster target: `cluster.id`, `cluster.kind`, `cluster.labels.<k>`,
//!   `cluster.groups`, `cluster.facts.<path>`, the same way
//! * on a workload placement (one workload on one cluster): `workload.name`,
//!   `workload.labels.<k>`, `workload.tags` and `workload.groups`, beside the
//!   hosting cluster's `cluster.*`
//! * `vars.<name>` (scalars and lists)
//! * `secrets.<name>` (a redacting secret value), `secrets.<name>.present`, `secrets.<name>.fingerprint`
//! * `module.name`, `module.tags`, `task.kind`, `task.id`, `task.tags` when evaluating inside a module/task
//!
//! The namespaces are disjoint: `host.*` is null on a cluster and
//! `cluster.*` is null on a host, and a selector applies only to the targets
//! whose namespace it names ([`HostContext::applies`]), so `!(host.labels.x)`
//! never leaks onto a cluster. A placement reads both `cluster.*` (its
//! cluster) and `workload.*`; `host.*` is null there too.

use crate::model::ClusterKind;
use crate::secrets::SecretStore;
use crate::{FilterValue, Filterable};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock};

/// What a rollout deploys to: a host (an enrolled agent), a cluster (a
/// virtual target an executor speaks for) or a workload placement (one
/// workload on one cluster, the unit a rollout moves clusters by).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    #[default]
    Host,
    Cluster,
    Workload,
}

impl TargetKind {
    /// The selector namespace of this kind of target.
    pub fn namespace(self) -> &'static str {
        match self {
            TargetKind::Host => "host",
            TargetKind::Cluster => "cluster",
            TargetKind::Workload => "workload",
        }
    }

    /// The namespace the target's identity (id, labels, groups, facts) is
    /// read through: a placement's identity is its cluster's.
    pub fn identity_namespace(self) -> &'static str {
        match self {
            TargetKind::Host => "host",
            TargetKind::Cluster | TargetKind::Workload => "cluster",
        }
    }

    /// Whether the target lives on a cluster (a cluster row or a placement).
    pub fn on_cluster(self) -> bool {
        matches!(self, TargetKind::Cluster | TargetKind::Workload)
    }

    pub fn as_str(self) -> &'static str {
        self.namespace()
    }
}

impl std::fmt::Display for TargetKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

static HOST_NS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\bhost\.").expect("static regex compiles"));
static CLUSTER_NS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\bcluster\.").expect("static regex compiles"));
static WORKLOAD_NS: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)\bworkload\.").expect("static regex compiles"));

/// Which target namespaces a selector names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Namespaces {
    pub host: bool,
    pub cluster: bool,
    pub workload: bool,
}

impl Namespaces {
    /// Whether a selector naming these namespaces applies to a target of
    /// `kind`: it names that kind's namespace, or names none (`true`,
    /// `vars.x == 1`), so a selector written for another kind alone is
    /// simply false here. A placement is also what `cluster.*` selects (every
    /// placement on the matching clusters), and `host.*` never selects one;
    /// a whole cluster is never selected by `workload.*`.
    pub fn applies_to(self, kind: TargetKind) -> bool {
        match kind {
            TargetKind::Host => self.host || (!self.cluster && !self.workload),
            TargetKind::Cluster => self.cluster || (!self.host && !self.workload),
            TargetKind::Workload => !self.host,
        }
    }

    /// Whether the selector names namespaces no one target reads together
    /// (`host.*` beside `cluster.*` or `workload.*`): the other half is
    /// always null, so it selects nothing it was meant to.
    pub fn mixed(self) -> bool {
        self.host && (self.cluster || self.workload)
    }
}

/// The target namespaces (`host.*`, `cluster.*`, `workload.*`) a selector's
/// text names.
pub fn selector_namespaces(raw: &str) -> Namespaces {
    Namespaces {
        host: HOST_NS.is_match(raw),
        cluster: CLUSTER_NS.is_match(raw),
        workload: WORKLOAD_NS.is_match(raw),
    }
}

/// Labels as reported by the agent and managed on the server: string keys to
/// scalar JSON values (strings, numbers, booleans).
pub type Labels = IndexMap<String, serde_json::Value>;

/// A label value given as text (`--labels`, agent.yaml): `true`/`false`
/// become booleans, `3` and `1.5` numbers, a quoted `"3"` or `'x'` the string
/// inside the quotes, anything else the text itself.
pub fn label_value(raw: &str) -> serde_json::Value {
    let raw = raw.trim();
    match serde_yaml::from_str::<serde_yaml::Value>(raw) {
        Ok(serde_yaml::Value::Bool(b)) => serde_json::Value::Bool(b),
        Ok(serde_yaml::Value::Number(n)) => n
            .as_i64()
            .map(serde_json::Value::from)
            .or_else(|| n.as_u64().map(serde_json::Value::from))
            .or_else(|| {
                n.as_f64()
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
            })
            .unwrap_or_else(|| serde_json::Value::String(raw.to_owned())),
        Ok(serde_yaml::Value::String(s)) => serde_json::Value::String(s),
        _ => serde_json::Value::String(raw.to_owned()),
    }
}

/// Labels from text values, typed with [`label_value`].
pub fn typed_labels<'a>(pairs: impl IntoIterator<Item = (&'a String, &'a String)>) -> Labels {
    pairs
        .into_iter()
        .map(|(k, v)| (k.clone(), label_value(v)))
        .collect()
}

/// Variables after overlay resolution.
pub type Vars = IndexMap<String, serde_json::Value>;

/// Identity and attributes of the target being planned: a host, or a
/// cluster target (whose id and labels live here too, read as `cluster.*`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostIdentity {
    pub id: String,
    #[serde(default)]
    pub labels: Labels,
}

/// The evaluation context for selectors and templates on one target.
#[derive(Clone, Default)]
pub struct HostContext {
    pub host: HostIdentity,
    /// Which namespace the identity is read through: `host.*`, or
    /// `cluster.*` for a cluster and for a placement on it.
    pub target: TargetKind,
    /// For a cluster target, its scheduler (`cluster.kind`).
    pub cluster_kind: Option<ClusterKind>,
    /// For a placement: the workload, read as `workload.*`.
    pub workload: Option<WorkloadScope>,
    pub groups: Vec<String>,
    /// The facts tree as collected by fact providers, mounted at `host.facts`.
    pub facts: serde_json::Value,
    pub vars: Vars,
    pub secrets: Arc<SecretStore>,
    pub module: Option<ModuleScope>,
    pub task: Option<TaskScope>,
    /// Collects what templates rendered in this context emit (see
    /// [`crate::template::Emitted`]); `None` leaves renders untraced.
    pub emitted: Option<Arc<crate::template::Emitted>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModuleScope {
    pub name: String,
    pub tags: Vec<String>,
}

/// The workload of a placement, as `workload.*` reads it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkloadScope {
    pub name: String,
    #[serde(default)]
    pub labels: Labels,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskScope {
    pub kind: String,
    pub id: String,
    pub tags: Vec<String>,
}

impl std::fmt::Debug for HostContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostContext")
            .field("host", &self.host)
            .field("target", &self.target)
            .field("cluster_kind", &self.cluster_kind)
            .field("workload", &self.workload)
            .field("groups", &self.groups)
            .field("vars", &self.vars.keys().collect::<Vec<_>>())
            .field("secrets", &self.secrets.names().collect::<Vec<_>>())
            .field("module", &self.module)
            .field("task", &self.task)
            .finish()
    }
}

impl HostContext {
    pub fn new(host: HostIdentity) -> Self {
        Self {
            host,
            ..Default::default()
        }
    }

    /// The context of a cluster target: `identity` is read as `cluster.id`
    /// and `cluster.labels.*`, and `host.*` is null.
    pub fn for_cluster(identity: HostIdentity, kind: ClusterKind) -> Self {
        Self {
            host: identity,
            target: TargetKind::Cluster,
            cluster_kind: Some(kind),
            ..Default::default()
        }
    }

    /// The context of a placement of the workload `name` on the cluster
    /// `cluster` describes: `cluster.*` still reads the cluster, and
    /// `workload.*` the workload. Groups are the cluster's until
    /// re-evaluated for the placement.
    pub fn for_workload(
        cluster: &HostContext,
        name: &str,
        labels: Labels,
        tags: &[String],
    ) -> Self {
        let mut c = cluster.clone();
        c.target = TargetKind::Workload;
        c.workload = Some(WorkloadScope {
            name: name.to_owned(),
            labels,
            tags: tags.to_vec(),
        });
        c.module = None;
        c.task = None;
        c
    }

    /// Whether `filter` applies to this kind of target at all (see
    /// [`Namespaces::applies_to`]).
    pub fn applies(&self, filter: &crate::Filter) -> bool {
        selector_namespaces(filter.raw()).applies_to(self.target)
    }

    pub fn with_facts(mut self, facts: serde_json::Value) -> Self {
        self.facts = facts;
        self
    }

    pub fn with_groups(mut self, groups: Vec<String>) -> Self {
        self.groups = groups;
        self
    }

    pub fn with_vars(mut self, vars: Vars) -> Self {
        self.vars = vars;
        self
    }

    pub fn with_secrets(mut self, secrets: Arc<SecretStore>) -> Self {
        self.secrets = secrets;
        self
    }

    /// A copy scoped to a module (for module-level `when` and templates).
    pub fn in_module(&self, name: &str, tags: &[String]) -> Self {
        let mut c = self.clone();
        c.module = Some(ModuleScope {
            name: name.to_owned(),
            tags: tags.to_vec(),
        });
        c.task = None;
        c
    }

    /// A copy whose renders are traced into `emitted`.
    pub fn with_emitted(mut self, emitted: Arc<crate::template::Emitted>) -> Self {
        self.emitted = Some(emitted);
        self
    }

    /// The host values a traced context's templates emitted, by path; `None`
    /// when renders were not traced.
    pub fn emitted_paths(&self) -> Option<Vec<String>> {
        self.emitted.as_ref().map(|e| e.paths())
    }

    /// `planned`'s diff text with the host values a traced context's
    /// templates emitted as placeholders ([`crate::template::Emitted::display`]).
    pub fn emitted_display(&self, planned: &crate::plan::Planned) -> Option<String> {
        self.emitted.as_ref().and_then(|e| e.display(planned))
    }

    /// The target's string values by template path, as the fleet view names
    /// them: `host.id`, `host.labels.site`, `host.facts.net.ipv4[0]` (always
    /// under `host`, which is how the server reads every target's context).
    pub fn host_leaves(&self) -> Vec<(String, String)> {
        let host = serde_json::json!({
            "id": self.host.id,
            "labels": self.host.labels,
            "facts": self.facts,
        });
        let mut out = Vec::new();
        flatten_leaves("host", &host, &mut out);
        out
    }

    /// A copy scoped to a task within the current module.
    pub fn in_task(&self, kind: &str, id: &str, tags: &[String]) -> Self {
        let mut c = self.clone();
        c.task = Some(TaskScope {
            kind: kind.to_owned(),
            id: id.to_owned(),
            tags: tags.to_vec(),
        });
        c
    }

    /// Evaluates a selector against this host, treating an evaluation error as
    /// "no match". Use [`HostContext::try_matches`] where an error must not be
    /// mistaken for a definite `false` (module selection, which drives removal).
    pub fn matches(&self, filter: &crate::Filter) -> bool {
        self.try_matches(filter).unwrap_or(false)
    }

    /// Evaluates a selector against this target, reporting evaluation
    /// errors. A selector written for the other kind of target alone (a
    /// `host.*` selector on a cluster, a `cluster.*` one on a host) is
    /// `false` without being evaluated. The error text names the filter,
    /// never the values it compared.
    pub fn try_matches(&self, filter: &crate::Filter) -> Result<bool, String> {
        if !self.applies(filter) {
            return Ok(false);
        }
        filter.matches(self).map_err(|e| {
            format!(
                "the selector `{}` could not be evaluated: {e}",
                filter.raw()
            )
        })
    }

    /// The template context (`host` or `cluster`, `workload` on a placement,
    /// `vars`, `module`, `task`) as JSON. Secrets are added by the templater
    /// as a special object, never as JSON.
    pub fn template_json(&self) -> serde_json::Value {
        let mut host = serde_json::Map::new();
        host.insert("id".into(), serde_json::Value::String(self.host.id.clone()));
        if let Some(kind) = self.cluster_kind {
            host.insert("kind".into(), serde_json::Value::String(kind.to_string()));
        }
        host.insert(
            "labels".into(),
            serde_json::Value::Object(self.host.labels.clone().into_iter().collect()),
        );
        host.insert(
            "groups".into(),
            serde_json::Value::Array(
                self.groups
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        host.insert("facts".into(), self.facts.clone());
        let mut root = serde_json::Map::new();
        root.insert(
            self.target.identity_namespace().into(),
            serde_json::Value::Object(host),
        );
        if let Some(w) = &self.workload {
            let mut workload = serde_json::Map::new();
            workload.insert("name".into(), serde_json::Value::String(w.name.clone()));
            workload.insert(
                "labels".into(),
                serde_json::Value::Object(w.labels.clone().into_iter().collect()),
            );
            workload.insert(
                "tags".into(),
                serde_json::Value::Array(
                    w.tags
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
            workload.insert(
                "groups".into(),
                serde_json::Value::Array(
                    self.groups
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
            root.insert("workload".into(), serde_json::Value::Object(workload));
        }
        root.insert(
            "vars".into(),
            serde_json::Value::Object(self.vars.clone().into_iter().collect()),
        );
        if let Some(m) = &self.module {
            root.insert("module".into(), serde_json::to_value(m).unwrap_or_default());
        }
        if let Some(t) = &self.task {
            root.insert("task".into(), serde_json::to_value(t).unwrap_or_default());
        }
        serde_json::Value::Object(root)
    }
}

/// String leaves of `value` by template path. Keys that are not identifiers
/// are written `["key"]`; buckle-api's fleet plan names paths the same way.
fn flatten_leaves(path: &str, value: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::String(s) => out.push((path.to_owned(), s.clone())),
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let simple = !k.is_empty()
                    && !k.starts_with(|c: char| c.is_ascii_digit())
                    && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                let child = if simple {
                    format!("{path}.{k}")
                } else {
                    format!("{path}[{}]", serde_json::Value::String(k.clone()))
                };
                flatten_leaves(&child, v, out);
            }
        }
        serde_json::Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                flatten_leaves(&format!("{path}[{i}]"), v, out);
            }
        }
        _ => {}
    }
}

/// Converts a JSON leaf to a filter value. Objects become null because filt-rs
/// has no nested values; address their leaves with dotted keys instead.
pub fn json_to_filter_value(v: &serde_json::Value) -> FilterValue<'static> {
    match v {
        serde_json::Value::Null => FilterValue::Null,
        serde_json::Value::Bool(b) => FilterValue::Bool(*b),
        serde_json::Value::Number(n) => FilterValue::Number(n.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(s) => FilterValue::String(s.clone().into()),
        serde_json::Value::Array(items) => {
            FilterValue::Tuple(items.iter().map(json_to_filter_value).collect())
        }
        serde_json::Value::Object(_) => FilterValue::Null,
    }
}

/// Walks `a.b.c` through nested JSON objects. Keys may themselves contain dots
/// (e.g. `disk./` is not supported; facts providers flatten such keys).
pub fn lookup_path<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = root;
    if path.is_empty() {
        return Some(current);
    }
    for part in path.split('.') {
        current = match current {
            serde_json::Value::Object(map) => map.get(part)?,
            serde_json::Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

impl Filterable for HostContext {
    fn get(&self, key: &str) -> FilterValue<'_> {
        let lower = key.to_ascii_lowercase();
        // The identity is read through the namespace of the target's kind
        // only (a placement's through its cluster's); the other namespaces
        // are null here.
        let identity = lower
            .strip_prefix("host.")
            .filter(|_| self.target == TargetKind::Host)
            .or_else(|| {
                lower
                    .strip_prefix("cluster.")
                    .filter(|_| self.target.on_cluster())
            });
        if let Some(rest) = identity {
            return match rest {
                "id" => self.host.id.as_str().into(),
                "kind" => match self.cluster_kind {
                    Some(kind) => kind.as_str().into(),
                    None => FilterValue::Null,
                },
                "groups" => FilterValue::Tuple(
                    self.groups
                        .iter()
                        .map(|g| FilterValue::String(g.as_str().into()))
                        .collect(),
                ),
                _ => {
                    if let Some(label) = rest.strip_prefix("labels.") {
                        self.host
                            .labels
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(label))
                            .map(|(_, v)| json_to_filter_value(v))
                            .unwrap_or(FilterValue::Null)
                    } else if let Some(fact) = rest.strip_prefix("facts.") {
                        // Facts are matched case-insensitively segment by segment.
                        lookup_path_ci(&self.facts, fact)
                            .map(json_to_filter_value)
                            .unwrap_or(FilterValue::Null)
                    } else {
                        FilterValue::Null
                    }
                }
            };
        }
        if let Some(rest) = lower.strip_prefix("workload.") {
            let Some(w) = &self.workload else {
                return FilterValue::Null;
            };
            return match rest {
                "name" => w.name.as_str().into(),
                "tags" => FilterValue::Tuple(
                    w.tags
                        .iter()
                        .map(|t| FilterValue::String(t.as_str().into()))
                        .collect(),
                ),
                "groups" => FilterValue::Tuple(
                    self.groups
                        .iter()
                        .map(|g| FilterValue::String(g.as_str().into()))
                        .collect(),
                ),
                _ => rest
                    .strip_prefix("labels.")
                    .and_then(|label| {
                        w.labels
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(label))
                            .map(|(_, v)| json_to_filter_value(v))
                    })
                    .unwrap_or(FilterValue::Null),
            };
        }
        if lower.starts_with("host.") || lower.starts_with("cluster.") {
            return FilterValue::Null;
        }
        if let Some(var) = lower.strip_prefix("vars.") {
            return self
                .vars
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(var))
                .map(|(_, v)| json_to_filter_value(v))
                .or_else(|| {
                    // Allow dotted access into structured vars.
                    let (head, tail) = var.split_once('.')?;
                    let (_, v) = self
                        .vars
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(head))?;
                    lookup_path_ci(v, tail).map(json_to_filter_value)
                })
                .unwrap_or(FilterValue::Null);
        }
        if let Some(rest) = lower.strip_prefix("secrets.") {
            let (name, attr) = match rest.rsplit_once('.') {
                Some((n, a)) if a == "present" || a == "fingerprint" => (n, Some(a)),
                _ => (rest, None),
            };
            let secret = self
                .secrets
                .iter()
                .find(|s| s.name().eq_ignore_ascii_case(name));
            return match (secret, attr) {
                (Some(_), Some("present")) => FilterValue::Bool(true),
                (None, Some("present")) => FilterValue::Bool(false),
                (Some(s), Some("fingerprint")) => {
                    FilterValue::String(s.fingerprint().as_hex().into())
                }
                (Some(s), None) => s.as_filter_value(),
                _ => FilterValue::Null,
            };
        }
        if let Some(rest) = lower.strip_prefix("module.") {
            return match (&self.module, rest) {
                (Some(m), "name") => m.name.as_str().into(),
                (Some(m), "tags") => FilterValue::Tuple(
                    m.tags
                        .iter()
                        .map(|t| FilterValue::String(t.as_str().into()))
                        .collect(),
                ),
                _ => FilterValue::Null,
            };
        }
        if let Some(rest) = lower.strip_prefix("task.") {
            return match (&self.task, rest) {
                (Some(t), "kind") => t.kind.as_str().into(),
                (Some(t), "id") => t.id.as_str().into(),
                (Some(t), "tags") => FilterValue::Tuple(
                    t.tags
                        .iter()
                        .map(|x| FilterValue::String(x.as_str().into()))
                        .collect(),
                ),
                _ => FilterValue::Null,
            };
        }
        FilterValue::Null
    }
}

fn lookup_path_ci<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut current = root;
    for part in path.split('.') {
        current = match current {
            serde_json::Value::Object(map) => map
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(part))
                .map(|(_, v)| v)?,
            serde_json::Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case("true", serde_json::json!(true))]
    #[case("False", serde_json::json!(false))]
    #[case("3", serde_json::json!(3))]
    #[case("1.5", serde_json::json!(1.5))]
    #[case("\"3\"", serde_json::json!("3"))]
    #[case("'true'", serde_json::json!("true"))]
    #[case("worker", serde_json::json!("worker"))]
    #[case("yes", serde_json::json!("yes"))]
    #[case("", serde_json::json!(""))]
    #[case("[a]", serde_json::json!("[a]"))]
    fn label_values_parse_as_typed_scalars(#[case] raw: &str, #[case] expected: serde_json::Value) {
        assert_eq!(label_value(raw), expected);
    }
    use crate::Filter;
    use crate::secrets::{InstallKey, Secret, SecretStore};
    use rstest::rstest;

    fn ctx() -> HostContext {
        let mut labels = Labels::new();
        labels.insert("role".into(), "worker".into());
        labels.insert("canary".into(), true.into());
        let mut vars = Vars::new();
        vars.insert("swap_mb".into(), 1024.into());
        vars.insert("ports".into(), serde_json::json!([80, 443]));
        vars.insert("nomad".into(), serde_json::json!({"datacenter": "dc1"}));
        let mut secrets = SecretStore::new();
        secrets.insert(Secret::new(
            "db_password",
            "hunter2".into(),
            &InstallKey::from_bytes([1u8; 32]),
        ));
        HostContext::new(HostIdentity {
            id: "pi-1".into(),
            labels,
        })
        .with_groups(vec!["home".into(), "nomad".into()])
        .with_facts(serde_json::json!({
            "os": {"family": "debian", "codename": "bookworm"},
            "arch": "aarch64",
            "mem": {"total_mb": 8192},
            "tailscale": null,
            "disks": [{"mount": "/", "free_percent": 42}]
        }))
        .with_vars(vars)
        .with_secrets(Arc::new(secrets))
        .in_module("nomad", &["config".into()])
        .in_task("file.template", "nomad.hcl", &["network".into()])
    }

    #[rstest]
    #[case(r#"host.id == "pi-1""#, true)]
    #[case(r#"host.labels.role == "worker""#, true)]
    #[case(r#"host.labels.Role == "WORKER""#, true)]
    #[case(r#"host.labels.canary == true"#, true)]
    #[case(r#"host.labels.missing == true"#, false)]
    #[case(r#"host.groups contains "home""#, true)]
    #[case(r#"host.groups contains "cloud""#, false)]
    #[case(
        r#"host.facts.os.family == "debian" && host.facts.arch in ["x86_64", "aarch64"]"#,
        true
    )]
    #[case(r#"host.facts.mem.total_mb >= 4096"#, true)]
    #[case(r#"host.facts.tailscale.hostname == "x""#, false)]
    #[case(r#"host.facts.disks.0.free_percent > 10"#, true)]
    #[case(r#"vars.swap_mb == 1024"#, true)]
    #[case(r#"vars.ports contains 443"#, true)]
    #[case(r#"vars.nomad.datacenter == "dc1""#, true)]
    #[case(r#"secrets.db_password.present"#, true)]
    #[case(r#"secrets.other.present"#, false)]
    #[case(r#"secrets.db_password == "hunter2""#, true)]
    #[case(r#"secrets.db_password == "wrong""#, false)]
    #[case(r#"module.name == "nomad" && module.tags contains "config""#, true)]
    #[case(
        r#"task.kind == "file.template" && task.tags contains "network""#,
        true
    )]
    #[case(r#"host.facts.os == "debian""#, false)]
    fn selectors_resolve_against_the_host_context(#[case] expr: &str, #[case] expected: bool) {
        let filter = Filter::new(expr).expect("filter parses");
        assert_eq!(ctx().matches(&filter), expected, "{expr}");
    }

    fn cluster_ctx() -> HostContext {
        let mut labels = Labels::new();
        labels.insert("cluster".into(), "home".into());
        HostContext::for_cluster(
            HostIdentity {
                id: "home-nomad".into(),
                labels,
            },
            ClusterKind::Nomad,
        )
        .with_groups(vec!["home-cluster".into()])
        .with_facts(serde_json::json!({"version": "1.9.3", "nodes": 3}))
    }

    #[rstest]
    #[case(r#"cluster.id == "home-nomad""#, true)]
    #[case(r#"cluster.kind == "nomad""#, true)]
    #[case(r#"cluster.kind in ["nomad", "kubernetes"]"#, true)]
    #[case(r#"cluster.labels.cluster == "home""#, true)]
    #[case(r#"cluster.groups contains "home-cluster""#, true)]
    #[case(r#"cluster.facts.nodes >= 3"#, true)]
    #[case(r#"cluster.labels.missing == true"#, false)]
    #[case(r#"host.id == "home-nomad""#, false)]
    #[case(r#"host.groups contains "home-cluster""#, false)]
    #[case(r#"!(host.labels.canary == true)"#, false)]
    #[case(r#"true"#, true)]
    #[case(
        r#"host.groups contains "home" || cluster.labels.cluster == "home""#,
        true
    )]
    fn cluster_selectors_resolve_against_a_cluster_context(
        #[case] expr: &str,
        #[case] expected: bool,
    ) {
        let filter = Filter::new(expr).expect("filter parses");
        assert_eq!(cluster_ctx().matches(&filter), expected, "{expr}");
    }

    #[rstest]
    #[case(r#"cluster.kind == "nomad""#, false)]
    #[case(r#"!(cluster.labels.canary == true)"#, false)]
    #[case(r#"cluster.id == "pi-1""#, false)]
    #[case(r#"host.id == "pi-1" || cluster.kind == "nomad""#, true)]
    #[case(r#"true"#, true)]
    #[case(r#"vars.swap_mb == 1024"#, true)]
    fn cluster_selectors_never_match_a_host(#[case] expr: &str, #[case] expected: bool) {
        let filter = Filter::new(expr).expect("filter parses");
        assert_eq!(ctx().matches(&filter), expected, "{expr}");
    }

    #[rstest]
    #[case("host.labels.x == 1", true, false, false)]
    #[case("cluster.kind == \"nomad\"", false, true, false)]
    #[case("HOST.id == \"a\" && Cluster.id == \"b\"", true, true, false)]
    #[case("vars.a == 1", false, false, false)]
    #[case("host.facts.fqdn == \"cluster.local\"", true, true, false)]
    #[case("workload.labels.tier == \"canary\"", false, false, true)]
    #[case(
        "cluster.kind == \"nomad\" && workload.name == \"grafana\"",
        false,
        true,
        true
    )]
    #[case(
        "host.labels.a == 1 && workload.tags contains \"db\"",
        true,
        false,
        true
    )]
    fn selector_namespaces_are_read_from_the_text(
        #[case] raw: &str,
        #[case] host: bool,
        #[case] cluster: bool,
        #[case] workload: bool,
    ) {
        assert_eq!(
            selector_namespaces(raw),
            Namespaces {
                host,
                cluster,
                workload
            }
        );
    }

    #[test]
    fn a_cluster_context_templates_under_cluster() {
        let json = cluster_ctx().template_json();
        assert_eq!(json["cluster"]["id"], "home-nomad");
        assert_eq!(json["cluster"]["kind"], "nomad");
        assert_eq!(json["cluster"]["labels"]["cluster"], "home");
        assert!(json.get("host").is_none());
    }

    #[test]
    fn secret_fingerprint_is_exposed_but_value_is_redacted_on_display() {
        let c = ctx();
        let fp = match c.get("secrets.db_password.fingerprint") {
            FilterValue::String(s) => s.into_owned(),
            other => panic!("expected a string, got {other:?}"),
        };
        assert_eq!(fp.len(), 8);
        let value = c.get("secrets.db_password");
        assert!(!format!("{value}").contains("hunter2"));
        assert!(!format!("{value:?}").contains("hunter2"));
    }

    #[test]
    fn template_json_contains_host_vars_and_scopes_but_no_secrets() {
        let json = ctx().template_json();
        assert_eq!(json["host"]["id"], "pi-1");
        assert_eq!(json["host"]["facts"]["arch"], "aarch64");
        assert_eq!(json["vars"]["swap_mb"], 1024);
        assert_eq!(json["module"]["name"], "nomad");
        assert_eq!(json["task"]["id"], "nomad.hcl");
        assert!(json.get("secrets").is_none());
        assert!(!json.to_string().contains("hunter2"));
    }

    #[test]
    fn debug_output_lists_secret_names_not_values() {
        let dbg = format!("{:?}", ctx());
        assert!(dbg.contains("db_password"));
        assert!(!dbg.contains("hunter2"));
    }
}
