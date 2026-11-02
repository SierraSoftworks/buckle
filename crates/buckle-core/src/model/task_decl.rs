//! A task declaration as written in `module.yaml`: a map with exactly one
//! non-reserved key naming the kind, plus the reserved common fields.

use super::RemovalPolicy;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Retry policy for a task whose failures may be transient.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    #[serde(default)]
    pub limit: u32,
    #[serde(default = "RetryPolicy::default_delay", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub delay: Duration,
}

impl RetryPolicy {
    fn default_delay() -> Duration {
        Duration::from_secs(5)
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            limit: 0,
            delay: Self::default_delay(),
        }
    }
}

/// The reserved keys that may appear beside the kind key.
pub const RESERVED_KEYS: &[&str] = &[
    "id", "when", "notify", "tags", "removal", "retry", "after", "name",
];

#[derive(Debug, Clone, PartialEq)]
pub struct TaskDecl {
    /// Task kind, e.g. `file.template`.
    pub kind: String,
    /// The kind's spec, untouched; the engine templates string scalars and the
    /// kind deserialises it into its typed `Spec`.
    pub spec: serde_yaml::Value,
    /// Optional stable identifier overriding the kind's derived key.
    pub id: Option<String>,
    pub name: Option<String>,
    pub when: Option<crate::Filter>,
    /// Handlers (in this module, or `module:handler`) to run if the task changes something.
    pub notify: Vec<String>,
    pub tags: Vec<String>,
    /// Overrides the kind's default removal policy.
    pub removal: Option<RemovalPolicy>,
    pub retry: RetryPolicy,
    /// Task ids this task must run after; absent means after the previous task.
    pub after: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize)]
struct Reserved {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    when: Option<crate::Filter>,
    #[serde(default)]
    notify: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    removal: Option<RemovalPolicy>,
    #[serde(default)]
    retry: RetryPolicy,
    #[serde(default)]
    after: Option<Vec<String>>,
}

impl<'de> Deserialize<'de> for TaskDecl {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map: IndexMap<String, serde_yaml::Value> = IndexMap::deserialize(d)?;
        let kinds: Vec<String> = map
            .keys()
            .filter(|k| !RESERVED_KEYS.contains(&k.as_str()))
            .cloned()
            .collect();
        let kind = match kinds.as_slice() {
            [k] => k.clone(),
            [] => {
                return Err(serde::de::Error::custom(
                    "a task must name exactly one kind (e.g. `file.copy: {...}`), found none",
                ));
            }
            many => {
                return Err(serde::de::Error::custom(format!(
                    "a task must name exactly one kind, found {}; did you forget to indent the kind's fields?",
                    many.join(", ")
                )));
            }
        };
        let spec = map.shift_remove(&kind).unwrap_or(serde_yaml::Value::Null);
        let reserved_value = serde_yaml::Value::Mapping(
            map.into_iter()
                .map(|(k, v)| (serde_yaml::Value::String(k), v))
                .collect(),
        );
        let reserved: Reserved =
            serde_yaml::from_value(reserved_value).map_err(serde::de::Error::custom)?;
        Ok(TaskDecl {
            kind,
            spec,
            id: reserved.id,
            name: reserved.name,
            when: reserved.when,
            notify: reserved.notify,
            tags: reserved.tags,
            removal: reserved.removal,
            retry: reserved.retry,
            after: reserved.after,
        })
    }
}

impl Serialize for TaskDecl {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(None)?;
        map.serialize_entry(&self.kind, &self.spec)?;
        if let Some(id) = &self.id {
            map.serialize_entry("id", id)?;
        }
        if let Some(name) = &self.name {
            map.serialize_entry("name", name)?;
        }
        if let Some(when) = &self.when {
            map.serialize_entry("when", when)?;
        }
        if !self.notify.is_empty() {
            map.serialize_entry("notify", &self.notify)?;
        }
        if !self.tags.is_empty() {
            map.serialize_entry("tags", &self.tags)?;
        }
        if let Some(removal) = &self.removal {
            map.serialize_entry("removal", removal)?;
        }
        if self.retry != RetryPolicy::default() {
            map.serialize_entry("retry", &self.retry)?;
        }
        if let Some(after) = &self.after {
            map.serialize_entry("after", after)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn parses_kind_and_reserved_fields() {
        let decl: TaskDecl = serde_yaml::from_str(
            "file.copy: { src: a, dest: /b }\nid: copy-a\nnotify: [reload]\ntags: [config]\nretry: { limit: 2, delay: 250ms }\n",
        )
        .unwrap();
        assert_eq!(decl.kind, "file.copy");
        assert_eq!(decl.id.as_deref(), Some("copy-a"));
        assert_eq!(decl.notify, vec!["reload"]);
        assert_eq!(
            decl.retry,
            RetryPolicy {
                limit: 2,
                delay: Duration::from_millis(250)
            }
        );
        assert_eq!(decl.spec["dest"], serde_yaml::Value::String("/b".into()));
    }

    #[rstest]
    #[case("id: only\n")]
    #[case("file.copy: {}\napt.package: {}\n")]
    #[case("file.copy: {}\nbogus_reserved_lookalike_is_a_second_kind: 1\n")]
    fn rejects_zero_or_multiple_kinds(#[case] yaml: &str) {
        assert!(serde_yaml::from_str::<TaskDecl>(yaml).is_err());
    }

    #[test]
    fn serialises_back_to_the_same_shape() {
        let decl: TaskDecl =
            serde_yaml::from_str("systemd.unit: { name: nomad }\nremoval: leave\n").unwrap();
        let yaml = serde_yaml::to_string(&decl).unwrap();
        assert!(yaml.contains("systemd.unit:"));
        assert!(yaml.contains("removal: leave"));
        assert!(!yaml.contains("retry"), "defaults are omitted");
    }
}
