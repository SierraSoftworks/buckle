//! Owned-field extraction: the part of an object that Buckle's field manager
//! owns, read from `metadata.managedFields` (`FieldsV1`), so a diff shows
//! what Buckle changes and not server defaults or other controllers' fields.
//!
//! `FieldsV1` is a trie: `f:<name>` is an object field, `k:<json>` a list
//! element by its merge keys, `v:<json>` a set element by value, `i:<n>` an
//! element by index, and `.` the node itself. An empty node owns the whole
//! value below it.

use crate::client::FIELD_MANAGER;
use serde_json::{Map, Value};

/// The fields `manager` owns through apply on `obj` (merged across its
/// entries for the main resource; `status` subresource entries are skipped).
pub fn owned(obj: &Value, manager: &str) -> Value {
    let mut set = Value::Object(Map::new());
    for entry in obj["metadata"]["managedFields"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if entry["manager"].as_str() != Some(manager)
            || entry["operation"].as_str() != Some("Apply")
            || entry["subresource"].as_str().is_some_and(|s| !s.is_empty())
        {
            continue;
        }
        union(&mut set, &entry["fieldsV1"]);
    }
    set
}

/// The fields Buckle owns on `obj`.
pub fn buckle_owned(obj: &Value) -> Value {
    owned(obj, FIELD_MANAGER)
}

/// Merges field set `b` into `a`.
pub fn union(a: &mut Value, b: &Value) {
    let (Some(a), Some(b)) = (a.as_object_mut(), b.as_object()) else {
        return;
    };
    for (k, v) in b {
        match a.get_mut(k) {
            Some(existing) => union(existing, v),
            None => {
                a.insert(k.clone(), v.clone());
            }
        }
    }
}

/// The part of `value` that `fields` describes. `None` when none of it is
/// present.
pub fn extract(value: &Value, fields: &Value) -> Option<Value> {
    let Some(set) = fields.as_object() else {
        return Some(value.clone());
    };
    if set.keys().all(|k| k == ".") {
        return Some(value.clone());
    }
    match value {
        Value::Object(obj) => {
            let mut out = Map::new();
            for (k, sub) in set {
                let Some(name) = k.strip_prefix("f:") else {
                    continue;
                };
                if let Some(child) = obj.get(name).and_then(|c| extract(c, sub)) {
                    out.insert(name.to_owned(), child);
                }
            }
            Some(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                let sub = set
                    .iter()
                    .find_map(|(k, sub)| element_matches(k, i, item).then_some(sub));
                if let Some(child) = sub.and_then(|s| extract(item, s)) {
                    out.push(child);
                }
            }
            Some(Value::Array(out))
        }
        other => Some(other.clone()),
    }
}

fn element_matches(key: &str, index: usize, item: &Value) -> bool {
    if let Some(keys) = key.strip_prefix("k:") {
        let Ok(Value::Object(want)) = serde_json::from_str::<Value>(keys) else {
            return false;
        };
        return want.iter().all(|(k, v)| item.get(k) == Some(v));
    }
    if let Some(v) = key.strip_prefix("v:") {
        return serde_json::from_str::<Value>(v).is_ok_and(|v| &v == item);
    }
    if let Some(i) = key.strip_prefix("i:") {
        return i.parse::<usize>() == Ok(index);
    }
    false
}

/// The leaf paths that differ between `before` and `after`
/// (`spec.template.spec.containers[grafana].image`), for a plan's summary.
pub fn changed_paths(before: &Value, after: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk("", before, after, &mut out);
    out
}

fn walk(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
    if a == b {
        return;
    }
    let join = |k: &str| {
        if path.is_empty() {
            k.to_owned()
        } else {
            format!("{path}.{k}")
        }
    };
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                walk(
                    &join(k),
                    x.get(k).unwrap_or(&Value::Null),
                    y.get(k).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
            for (i, (p, q)) in x.iter().zip(y).enumerate() {
                let label = q["name"]
                    .as_str()
                    .or(p["name"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| i.to_string());
                walk(&format!("{path}[{label}]"), p, q, out);
            }
        }
        _ => out.push(if path.is_empty() {
            "(object)".into()
        } else {
            path.to_owned()
        }),
    }
}

/// Drops fields that change on every write and say nothing about intent.
pub fn strip_volatile(obj: &mut Value) {
    if let Some(meta) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        for k in [
            "managedFields",
            "resourceVersion",
            "generation",
            "uid",
            "creationTimestamp",
            "selfLink",
        ] {
            meta.remove(k);
        }
    }
    if let Some(o) = obj.as_object_mut() {
        o.remove("status");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn deployment(image: &str, replicas: u64) -> Value {
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "grafana", "labels": {"app": "grafana"}, "uid": "x",
                "managedFields": [
                    {"manager": "buckle", "operation": "Apply", "fieldsV1": {
                        "f:metadata": {"f:labels": {"f:app": {}}},
                        "f:spec": {"f:template": {"f:spec": {"f:containers": {
                            "k:{\"name\":\"grafana\"}": {".": {}, "f:image": {}, "f:name": {}}
                        }}}}
                    }},
                    {"manager": "kube-controller-manager", "operation": "Update", "fieldsV1": {"f:spec": {"f:replicas": {}}}}
                ]},
            "spec": {
                "replicas": replicas,
                "progressDeadlineSeconds": 600,
                "template": {"spec": {"containers": [
                    {"name": "grafana", "image": image, "imagePullPolicy": "IfNotPresent"},
                    {"name": "sidecar", "image": "injected"}
                ]}}
            }
        })
    }

    #[test]
    fn only_buckle_owned_fields_are_extracted() {
        let obj = deployment("grafana:11.3.1", 3);
        let got = extract(&obj, &buckle_owned(&obj)).unwrap();
        assert_eq!(
            got,
            json!({
                "metadata": {"labels": {"app": "grafana"}},
                "spec": {"template": {"spec": {"containers": [{"name": "grafana", "image": "grafana:11.3.1"}]}}}
            })
        );
    }

    #[test]
    fn changes_outside_owned_fields_do_not_show() {
        let before = deployment("grafana:11.3.1", 1);
        let after = deployment("grafana:11.4.0", 5);
        let set = buckle_owned(&after);
        let paths = changed_paths(
            &extract(&before, &set).unwrap(),
            &extract(&after, &set).unwrap(),
        );
        assert_eq!(paths, vec!["spec.template.spec.containers[grafana].image"]);
    }

    #[test]
    fn set_elements_and_union() {
        let mut a = json!({"f:a": {}});
        union(&mut a, &json!({"f:b": {"f:c": {}}, "f:a": {}}));
        assert_eq!(a, json!({"f:a": {}, "f:b": {"f:c": {}}}));
        let v = json!({"finalizers": ["x", "y"]});
        let got = extract(&v, &json!({"f:finalizers": {"v:\"y\"": {}}})).unwrap();
        assert_eq!(got, json!({"finalizers": ["y"]}));
    }

    #[test]
    fn volatile_metadata_is_dropped() {
        let mut obj = deployment("a", 1);
        obj["status"] = json!({"x": 1});
        strip_volatile(&mut obj);
        assert!(obj["metadata"].get("managedFields").is_none());
        assert!(obj["metadata"].get("uid").is_none());
        assert!(obj.get("status").is_none());
    }
}
