//! The filt-rs context for a host: everything a `when:` selector, a group
//! definition or a phase selector may reference, as flat dotted keys.
//!
//! * `host.id`, `host.labels.<k>`, `host.groups` (tuple)
//! * `host.facts.<path.to.leaf>` (facts tree, leaves only; objects are null)
//! * `vars.<name>` (scalars and lists)
//! * `secrets.<name>` (a redacting secret value), `secrets.<name>.present`, `secrets.<name>.fingerprint`
//! * `module.name`, `module.tags`, `task.kind`, `task.id`, `task.tags` when evaluating inside a module/task

use crate::secrets::SecretStore;
use crate::{FilterValue, Filterable};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Labels as reported by the agent and managed on the server: string keys to
/// scalar JSON values (strings, numbers, booleans).
pub type Labels = IndexMap<String, serde_json::Value>;

/// Variables after overlay resolution.
pub type Vars = IndexMap<String, serde_json::Value>;

/// Identity and attributes of the host being planned.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostIdentity {
    pub id: String,
    #[serde(default)]
    pub labels: Labels,
}

/// The evaluation context for selectors and templates on one host.
#[derive(Clone, Default)]
pub struct HostContext {
    pub host: HostIdentity,
    pub groups: Vec<String>,
    /// The facts tree as collected by fact providers, mounted at `host.facts`.
    pub facts: serde_json::Value,
    pub vars: Vars,
    pub secrets: Arc<SecretStore>,
    pub module: Option<ModuleScope>,
    pub task: Option<TaskScope>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModuleScope {
    pub name: String,
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

    /// Evaluates a selector against this host. Evaluation itself cannot fail;
    /// parse errors were reported when the filter was built.
    pub fn matches(&self, filter: &crate::Filter) -> bool {
        filter.matches(self).unwrap_or(false)
    }

    /// The template context (`host`, `vars`, `module`, `task`) as JSON. Secrets
    /// are added by the templater as a special object, never as JSON.
    pub fn template_json(&self) -> serde_json::Value {
        let mut host = serde_json::Map::new();
        host.insert("id".into(), serde_json::Value::String(self.host.id.clone()));
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
        root.insert("host".into(), serde_json::Value::Object(host));
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
        if let Some(rest) = lower.strip_prefix("host.") {
            return match rest {
                "id" => self.host.id.as_str().into(),
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
