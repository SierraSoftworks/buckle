//! Field paths into a scheduler object, as a workload's `ignore` writes them:
//! `TaskGroups[*].Tasks[*].Config.image`, `Meta[autoupdate.*]`,
//! `spec.replicas`. A matched field is one another tool owns: the live
//! value is adopted into the desired object before it is diffed or sent, so
//! a difference there is neither a change nor drift (Terraform's
//! `ignore_changes`).
//!
//! Grammar: segments joined by `.`; a segment is a key, `*` (every key or
//! element), or a key followed by `[…]` selectors. Inside `[…]`: `*` (every
//! element or key), a number (an array index), or text (an object key, which
//! may hold dots, or the `Name`/`name` of an element in an array of objects).
//! Text ending in `*` is a prefix match.

use crate::error::{Result, user};
use serde_json::Value;

/// One parsed `ignore` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    raw: String,
    segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// Every key of an object, or every element of an array.
    Any,
    /// An object key, or the `Name` of an element in an array of objects.
    Key(String),
    /// Keys (or names) starting with the text.
    Prefix(String),
    /// An array element.
    Index(usize),
}

impl Segment {
    fn parse(text: &str) -> Option<Segment> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        Some(if text == "*" {
            Segment::Any
        } else if let Ok(i) = text.parse::<usize>() {
            Segment::Index(i)
        } else if let Some(prefix) = text.strip_suffix('*') {
            Segment::Prefix(prefix.to_owned())
        } else {
            Segment::Key(text.to_owned())
        })
    }

    fn matches_key(&self, key: &str) -> bool {
        match self {
            Segment::Any => true,
            Segment::Key(k) => k == key,
            Segment::Prefix(p) => key.starts_with(p.as_str()),
            Segment::Index(_) => false,
        }
    }

    fn matches_element(&self, index: usize, element: &Value) -> bool {
        match self {
            Segment::Any => true,
            Segment::Index(i) => *i == index,
            Segment::Key(_) | Segment::Prefix(_) => element
                .get("Name")
                .or_else(|| element.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|name| self.matches_key(name)),
        }
    }
}

impl FieldPath {
    pub fn parse(raw: &str) -> Result<FieldPath> {
        let invalid = |why: &str| {
            user(
                format!("The field path '{raw}' is not valid: {why}."),
                &[
                    "Write segments joined by dots, with `[*]`, `[3]` or `[name]` to select inside a field: `TaskGroups[*].Tasks[*].Config.image`, `Meta[autoupdate.*]`, `spec.replicas`.",
                ],
            )
        };
        let mut segments = Vec::new();
        let mut rest = raw.trim();
        if rest.is_empty() {
            return Err(invalid("it is empty"));
        }
        while !rest.is_empty() {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            let head = &rest[..end];
            if !head.is_empty() {
                segments.push(Segment::parse(head).ok_or_else(|| invalid("a segment is empty"))?);
            } else if end == 0 && rest.starts_with('.') {
                return Err(invalid("a segment is empty"));
            }
            rest = &rest[end..];
            while let Some(after) = rest.strip_prefix('[') {
                let close = after
                    .find(']')
                    .ok_or_else(|| invalid("a `[` is not closed"))?;
                segments.push(
                    Segment::parse(&after[..close])
                        .ok_or_else(|| invalid("a `[]` selector is empty"))?,
                );
                rest = &after[close + 1..];
            }
            if let Some(after) = rest.strip_prefix('.') {
                if after.is_empty() {
                    return Err(invalid("it ends with a dot"));
                }
                rest = after;
            } else if !rest.is_empty() {
                return Err(invalid("a `]` must be followed by `.` or `[`"));
            }
        }
        Ok(FieldPath {
            raw: raw.trim().to_owned(),
            segments,
        })
    }

    /// Parses every path, reporting the first that is not valid.
    pub fn parse_all(raw: &[String]) -> Result<Vec<FieldPath>> {
        raw.iter().map(|p| FieldPath::parse(p)).collect()
    }

    /// The path as written.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Copies the live values under this path into `desired`: a field the
    /// live object has takes its value, one it lacks is removed, so that the
    /// two agree wherever the path reaches. Containers the path walks through
    /// are matched on both sides; a container only one side has is left as
    /// it is.
    pub fn adopt(&self, desired: &mut Value, live: &Value) {
        adopt(&self.segments, desired, live);
    }
}

impl std::fmt::Display for FieldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Applies every path's [`FieldPath::adopt`] to `desired`.
pub fn adopt_all(paths: &[FieldPath], desired: &mut Value, live: &Value) {
    for p in paths {
        p.adopt(desired, live);
    }
}

fn adopt(segments: &[Segment], desired: &mut Value, live: &Value) {
    let Some((head, tail)) = segments.split_first() else {
        *desired = live.clone();
        return;
    };
    match desired {
        Value::Object(d) => {
            let Value::Object(l) = live else {
                return;
            };
            let keys: Vec<String> = d
                .keys()
                .chain(l.keys())
                .filter(|k| head.matches_key(k))
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            for key in keys {
                match (d.contains_key(&key), l.get(&key)) {
                    (true, Some(lv)) => adopt(tail, d.get_mut(&key).unwrap(), lv),
                    (true, None) if tail.is_empty() => {
                        d.remove(&key);
                    }
                    (false, Some(lv)) if tail.is_empty() => {
                        d.insert(key, lv.clone());
                    }
                    _ => {}
                }
            }
        }
        Value::Array(d) => {
            let Value::Array(l) = live else {
                return;
            };
            for (i, dv) in d.iter_mut().enumerate() {
                if !head.matches_element(i, dv) {
                    continue;
                }
                // Elements pair up by index, or by name when both are named.
                let lv = match head {
                    Segment::Key(_) | Segment::Prefix(_) => l.iter().find(|e| {
                        let name = |v: &Value| {
                            v.get("Name")
                                .or_else(|| v.get("name"))
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        };
                        name(e).is_some() && name(e) == name(dv)
                    }),
                    _ => l.get(i),
                };
                if let Some(lv) = lv {
                    adopt(tail, dv, lv);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;

    #[rstest]
    #[case("spec.replicas", 2)]
    #[case("TaskGroups[*].Tasks[*].Config.image", 6)]
    #[case("Meta[autoupdate.*]", 2)]
    #[case("Meta.*", 2)]
    #[case("TaskGroups[api].Count", 3)]
    #[case("a[0][1].b", 4)]
    fn paths_parse(#[case] raw: &str, #[case] segments: usize) {
        let p = FieldPath::parse(raw).unwrap();
        assert_eq!(p.segments.len(), segments, "{p:?}");
        assert_eq!(p.to_string(), raw);
    }

    #[rstest]
    #[case("")]
    #[case("a..b")]
    #[case("a.")]
    #[case("a[")]
    #[case("a[]")]
    #[case("a[0]b")]
    fn bad_paths_are_rejected(#[case] raw: &str) {
        assert!(FieldPath::parse(raw).is_err(), "{raw}");
    }

    fn job(image: &str, meta: Value) -> Value {
        json!({
            "ID": "paste",
            "Meta": meta,
            "TaskGroups": [
                {"Name": "api", "Count": 1, "Tasks": [{"Name": "paste", "Config": {"image": image}}]},
                {"Name": "worker", "Count": 2, "Tasks": [{"Name": "w", "Config": {"image": image}}]}
            ]
        })
    }

    #[test]
    fn adopting_copies_live_values_under_the_path_only() {
        let mut desired = job("paste:main", json!({"autoupdate.version": "docker:x"}));
        let live = job(
            "paste:main@sha256:abc",
            json!({"autoupdate.version": "docker:x", "autoupdate.state": "pinned", "buckle.target": "t"}),
        );
        let paths = FieldPath::parse_all(&[
            "TaskGroups[*].Tasks[*].Config.image".into(),
            "Meta[autoupdate.*]".into(),
        ])
        .unwrap();
        adopt_all(&paths, &mut desired, &live);
        assert_eq!(
            desired["TaskGroups"][0]["Tasks"][0]["Config"]["image"],
            "paste:main@sha256:abc"
        );
        assert_eq!(desired["Meta"]["autoupdate.state"], "pinned");
        assert!(desired["Meta"].get("buckle.target").is_none());
        assert_eq!(desired["TaskGroups"][1]["Count"], 2);
    }

    #[test]
    fn adopting_removes_a_field_the_live_object_lacks_and_pairs_elements_by_name() {
        let mut desired =
            json!({"TaskGroups": [{"Name": "api", "Count": 1}, {"Name": "worker", "Count": 2}]});
        let live = json!({"TaskGroups": [{"Name": "worker", "Count": 5}, {"Name": "api"}]});
        FieldPath::parse("TaskGroups[worker].Count")
            .unwrap()
            .adopt(&mut desired, &live);
        FieldPath::parse("TaskGroups[api].Count")
            .unwrap()
            .adopt(&mut desired, &live);
        assert_eq!(desired["TaskGroups"][1]["Count"], 5);
        assert!(desired["TaskGroups"][0].get("Count").is_none());
    }

    #[test]
    fn adopting_leaves_containers_only_one_side_has() {
        let mut desired = json!({"spec": {"replicas": 1}});
        let live = json!({"status": {}});
        FieldPath::parse("spec.replicas")
            .unwrap()
            .adopt(&mut desired, &live);
        assert_eq!(desired, json!({"spec": {"replicas": 1}}));
    }
}
