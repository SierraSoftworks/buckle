//! An action as written in `module.yaml` (or `workload.yaml`): a task an
//! operator runs on demand, shaped like a handler, plus `confirm` and
//! `description`.
//!
//! ```yaml
//! actions:
//!   upgrade:
//!     apt.upgrade: { autoremove: true }
//!     notify: [reboot-if-required]
//!   reboot: { system.reboot: {}, confirm: true }
//! ```

use super::TaskDecl;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq)]
pub struct ActionDecl {
    /// The task the action runs (its `notify`, `when`, `retry` and `name`
    /// apply as they do to a task).
    pub task: TaskDecl,
    /// Ask the operator to confirm (with a note for the audit log) first.
    pub confirm: bool,
    /// One line on what the action does, for the UI.
    pub description: Option<String>,
}

impl ActionDecl {
    /// The label the UI shows: the task's `name`, else the action's.
    pub fn label<'a>(&'a self, name: &'a str) -> &'a str {
        self.task.name.as_deref().unwrap_or(name)
    }

    /// The task keys this action sets that an action does not support: an
    /// action owns no resource, so it has no stable id, removal or ordering.
    pub fn unsupported_keys(&self) -> Vec<&'static str> {
        let t = &self.task;
        let mut out = Vec::new();
        if t.id.is_some() {
            out.push("id");
        }
        if t.removal.is_some() {
            out.push("removal");
        }
        if t.after.is_some() {
            out.push("after");
        }
        out
    }
}

impl<'de> Deserialize<'de> for ActionDecl {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut map: IndexMap<String, serde_yaml::Value> = IndexMap::deserialize(d)?;
        let confirm = match map.shift_remove("confirm") {
            None => false,
            Some(v) => serde_yaml::from_value(v)
                .map_err(|e| serde::de::Error::custom(format!("`confirm`: {e}")))?,
        };
        let description = match map.shift_remove("description") {
            None => None,
            Some(v) => serde_yaml::from_value(v)
                .map_err(|e| serde::de::Error::custom(format!("`description`: {e}")))?,
        };
        let rest = serde_yaml::Value::Mapping(
            map.into_iter()
                .map(|(k, v)| (serde_yaml::Value::String(k), v))
                .collect(),
        );
        let task: TaskDecl = serde_yaml::from_value(rest).map_err(serde::de::Error::custom)?;
        Ok(ActionDecl {
            task,
            confirm,
            description,
        })
    }
}

impl Serialize for ActionDecl {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let task = serde_yaml::to_value(&self.task).map_err(serde::ser::Error::custom)?;
        let mut map = s.serialize_map(None)?;
        if let serde_yaml::Value::Mapping(m) = task {
            for (k, v) in m {
                map.serialize_entry(&k, &v)?;
            }
        }
        if self.confirm {
            map.serialize_entry("confirm", &true)?;
        }
        if let Some(d) = &self.description {
            map.serialize_entry("description", d)?;
        }
        map.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn parses_the_task_and_the_action_keys() {
        let a: ActionDecl = serde_yaml::from_str(
            "apt.upgrade: { autoremove: true }\nnotify: [reboot]\nconfirm: true\ndescription: Upgrade every package\nname: Upgrade packages\n",
        )
        .unwrap();
        assert_eq!(a.task.kind, "apt.upgrade");
        assert_eq!(a.task.notify, vec!["reboot"]);
        assert!(a.confirm);
        assert_eq!(a.description.as_deref(), Some("Upgrade every package"));
        assert_eq!(a.label("upgrade"), "Upgrade packages");
        assert!(a.unsupported_keys().is_empty());
    }

    #[test]
    fn round_trips() {
        let a: ActionDecl =
            serde_yaml::from_str("systemd.restart: { name: nomad }\nconfirm: true\n").unwrap();
        let again: ActionDecl = serde_yaml::from_str(&serde_yaml::to_string(&a).unwrap()).unwrap();
        assert_eq!(a, again);
        assert_eq!(again.label("restart"), "restart");
    }

    #[rstest]
    #[case("confirm: true\n")]
    #[case("systemd.restart: {}\nconfirm: maybe\n")]
    #[case("systemd.restart: {}\napt.upgrade: {}\n")]
    fn rejects_malformed_actions(#[case] yaml: &str) {
        assert!(serde_yaml::from_str::<ActionDecl>(yaml).is_err());
    }

    #[test]
    fn reports_task_keys_an_action_cannot_use() {
        let a: ActionDecl =
            serde_yaml::from_str("command.run: { run: [\"true\"] }\nid: x\nremoval: leave\n")
                .unwrap();
        assert_eq!(a.unsupported_keys(), vec!["id", "removal"]);
    }
}
