//! Objects as a workload's manifests declare them: identity, parsing of
//! (multi-document) YAML, and the ownership labels Buckle stamps on them.

use buckle_core::Result;
use buckle_core::error::user;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The label naming the cluster target that owns an object.
pub const TARGET_LABEL: &str = "buckle.sierrasoftworks.com/target";
/// The label naming the workload that declares an object.
pub const WORKLOAD_LABEL: &str = "buckle.sierrasoftworks.com/workload";

/// An object's identity in the cluster.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjectKey {
    #[serde(rename = "apiVersion")]
    pub api_version: String,
    pub kind: String,
    /// Empty for cluster-scoped objects.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    pub name: String,
}

impl ObjectKey {
    pub fn new(
        api_version: impl Into<String>,
        kind: impl Into<String>,
        namespace: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            api_version: api_version.into(),
            kind: kind.into(),
            namespace: namespace.into(),
            name: name.into(),
        }
    }

    /// The key of `obj` (its namespace as written; [`crate::Primitive`]
    /// planning settles the default).
    pub fn of(obj: &Value) -> Option<Self> {
        Some(Self::new(
            obj["apiVersion"].as_str()?,
            obj["kind"].as_str()?,
            obj["metadata"]["namespace"].as_str().unwrap_or_default(),
            obj["metadata"]["name"].as_str()?,
        ))
    }

    /// The API group (`apps`; empty for the core group).
    pub fn group(&self) -> &str {
        self.api_version
            .split_once('/')
            .map(|(g, _)| g)
            .unwrap_or_default()
    }

    /// The resource key used in plans: `monitoring/Deployment/grafana`, or
    /// `Namespace/monitoring` for cluster-scoped objects.
    pub fn key(&self) -> String {
        if self.namespace.is_empty() {
            format!("{}/{}", self.kind, self.name)
        } else {
            format!("{}/{}/{}", self.namespace, self.kind, self.name)
        }
    }

    pub fn is(&self, kind: &str) -> bool {
        self.kind == kind
    }

    /// Whether removing the object can take data or other workloads with it.
    pub fn holds_data(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "Namespace"
                | "PersistentVolumeClaim"
                | "PersistentVolume"
                | "Secret"
                | "ConfigMap"
                | "CustomResourceDefinition"
                | "StorageClass"
        )
    }
}

impl std::fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.namespace.is_empty() {
            write!(f, "{} {}", self.kind, self.name)
        } else {
            write!(f, "{} {}/{}", self.kind, self.namespace, self.name)
        }
    }
}

/// The objects in a manifest file: every YAML (or JSON) document, with
/// `kind: List` documents unpacked and empty documents skipped.
pub fn parse_manifests(text: &str, file: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for (i, doc) in serde_yaml::Deserializer::from_str(text).enumerate() {
        let value = serde_yaml::Value::deserialize(doc).map_err(|e| {
            user(
                format!("{file} is not valid YAML (document {}): {e}", i + 1),
                &["Each document of a manifest file is one Kubernetes object."],
            )
        })?;
        if value.is_null() {
            continue;
        }
        let json: Value = serde_json::to_value(&value).map_err(|e| {
            user(
                format!(
                    "{file} (document {}) cannot be read as an object: {e}",
                    i + 1
                ),
                &["Mapping keys of a manifest are strings."],
            )
        })?;
        if json["kind"].as_str() == Some("List") && json["items"].is_array() {
            out.extend(json["items"].as_array().cloned().unwrap_or_default());
        } else {
            out.push(json);
        }
    }
    for (i, obj) in out.iter().enumerate() {
        if !obj.is_object() || ObjectKey::of(obj).is_none() {
            return Err(user(
                format!(
                    "Object {} of {file} has no apiVersion, kind or metadata.name.",
                    i + 1
                ),
                &[
                    "Every object needs apiVersion, kind and metadata.name (generateName is not supported).",
                ],
            ));
        }
    }
    Ok(out)
}

/// The stamps on a live object: the owning target and workload.
pub fn stamps(obj: &Value) -> Option<(String, String)> {
    let labels = &obj["metadata"]["labels"];
    Some((
        labels[TARGET_LABEL].as_str()?.to_owned(),
        labels[WORKLOAD_LABEL]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    ))
}

/// Sets the ownership labels on an intent.
pub fn stamp(obj: &mut Value, target: &str, workload: &str) {
    let meta = obj.as_object_mut().map(|o| {
        o.entry("metadata")
            .or_insert_with(|| Value::Object(Map::new()))
    });
    let Some(Value::Object(meta)) = meta else {
        return;
    };
    let labels = meta
        .entry("labels")
        .or_insert_with(|| Value::Object(Map::new()));
    if !labels.is_object() {
        *labels = Value::Object(Map::new());
    }
    if let Some(labels) = labels.as_object_mut() {
        labels.insert(TARGET_LABEL.into(), Value::String(label_value(target)));
        labels.insert(WORKLOAD_LABEL.into(), Value::String(label_value(workload)));
    }
}

/// A label value: at most 63 characters of `[A-Za-z0-9._-]`, starting and
/// ending alphanumeric.
pub fn label_value(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(63)
        .collect();
    while s.ends_with(|c: char| !c.is_ascii_alphanumeric()) {
        s.pop();
    }
    while s.starts_with(|c: char| !c.is_ascii_alphanumeric()) {
        s.remove(0);
    }
    s
}

/// Who owns a live object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owner {
    Absent,
    /// Present without Buckle's labels: adopted on apply.
    Unmanaged,
    /// Stamped by this target, by the named workload.
    Us(String),
    /// Stamped by another target: a plan error, so two targets never fight.
    Other(String),
}

impl Owner {
    pub fn of(live: Option<&Value>, target: &str) -> Owner {
        match live {
            None => Owner::Absent,
            Some(obj) => match stamps(obj) {
                None => Owner::Unmanaged,
                Some((t, w)) if t == label_value(target) => Owner::Us(w),
                Some((t, _)) => Owner::Other(t),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn multi_document_files_and_lists_become_objects() {
        let text = r#"
apiVersion: v1
kind: Service
metadata: { name: grafana }
---
---
apiVersion: v1
kind: List
items:
  - { apiVersion: apps/v1, kind: Deployment, metadata: { name: grafana, namespace: monitoring } }
"#;
        let objs = parse_manifests(text, "grafana.yaml").unwrap();
        let keys: Vec<String> = objs
            .iter()
            .map(|o| ObjectKey::of(o).unwrap().key())
            .collect();
        assert_eq!(
            keys,
            vec!["Service/grafana", "monitoring/Deployment/grafana"]
        );
        assert_eq!(ObjectKey::of(&objs[1]).unwrap().group(), "apps");
    }

    #[rstest]
    #[case::no_name("apiVersion: v1\nkind: Service\nmetadata: {}\n")]
    #[case::scalar("just text\n")]
    #[case::bad_yaml("a: [\n")]
    fn objects_without_identity_are_refused(#[case] text: &str) {
        assert!(parse_manifests(text, "x.yaml").is_err());
    }

    #[test]
    fn stamps_round_trip_and_decide_ownership() {
        let mut obj =
            serde_json::json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "a"}});
        assert_eq!(Owner::of(Some(&obj), "lab"), Owner::Unmanaged);
        stamp(&mut obj, "lab", "grafana");
        assert_eq!(Owner::of(Some(&obj), "lab"), Owner::Us("grafana".into()));
        assert_eq!(Owner::of(Some(&obj), "other"), Owner::Other("lab".into()));
        assert_eq!(Owner::of(None, "lab"), Owner::Absent);
    }

    #[rstest]
    #[case("lab-k3s", "lab-k3s")]
    #[case("-a b/c-", "a_b_c")]
    fn label_values_are_valid(#[case] raw: &str, #[case] expected: &str) {
        assert_eq!(label_value(raw), expected);
    }
}
