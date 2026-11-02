//! Editing a host's server labels (design §12): rows of key and value, checked
//! as they are typed, turned into the map `PATCH /hosts/{id}` and
//! `POST /hosts/{id}/preview` take.

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Row {
    pub key: String,
    pub value: String,
}

pub fn rows(labels: &BTreeMap<String, String>) -> Vec<Row> {
    labels
        .iter()
        .map(|(key, value)| Row {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

/// What is wrong with one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowError {
    pub row: usize,
    pub message: String,
}

/// Whether a label key is one filt-rs selectors can address as
/// `host.labels.<key>`: letters, digits, `_`, `-`, `.` and `/`, starting
/// with a letter or digit.
pub fn valid_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/'))
}

/// The label map the rows describe, or every problem with them. Rows with
/// neither key nor value are ignored, so an empty trailing row is harmless.
pub fn collect(rows: &[Row]) -> Result<BTreeMap<String, String>, Vec<RowError>> {
    let mut out = BTreeMap::new();
    let mut errors = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let key = row.key.trim();
        let value = row.value.trim();
        if key.is_empty() && value.is_empty() {
            continue;
        }
        if key.is_empty() {
            errors.push(RowError {
                row: i,
                message: "A label needs a key.".into(),
            });
        } else if !valid_key(key) {
            errors.push(RowError {
                row: i,
                message: format!(
                    "'{key}' is not a valid key: use letters, digits, '_', '-', '.' and '/'."
                ),
            });
        } else if out.insert(key.to_string(), value.to_string()).is_some() {
            errors.push(RowError {
                row: i,
                message: format!("'{key}' is set twice."),
            });
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// The merged view: operator labels win over the agent's seed labels.
pub fn effective(
    agent: &BTreeMap<String, String>,
    server: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = agent.clone();
    out.extend(server.iter().map(|(k, v)| (k.clone(), v.clone())));
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Added {
        key: String,
        value: String,
    },
    Removed {
        key: String,
        value: String,
    },
    Changed {
        key: String,
        from: String,
        to: String,
    },
}

/// What saving would change, for the confirmation and the audit note.
pub fn changes(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>) -> Vec<Change> {
    let mut out = Vec::new();
    for (key, value) in before {
        match after.get(key) {
            None => out.push(Change::Removed {
                key: key.clone(),
                value: value.clone(),
            }),
            Some(to) if to != value => out.push(Change::Changed {
                key: key.clone(),
                from: value.clone(),
                to: to.clone(),
            }),
            Some(_) => {}
        }
    }
    for (key, value) in after {
        if !before.contains_key(key) {
            out.push(Change::Added {
                key: key.clone(),
                value: value.clone(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn row(k: &str, v: &str) -> Row {
        Row {
            key: k.into(),
            value: v.into(),
        }
    }

    #[test]
    fn rows_round_trip_and_blank_rows_are_ignored() {
        let labels = map(&[("role", "worker"), ("site", "home")]);
        let mut r = rows(&labels);
        r.push(Row::default());
        assert_eq!(collect(&r), Ok(labels));
        assert_eq!(
            collect(&[row("  site ", " cloud ")]),
            Ok(map(&[("site", "cloud")]))
        );
    }

    #[test]
    fn bad_and_duplicate_keys_are_reported_by_row() {
        let errors = collect(&[
            row("site", "home"),
            row("", "orphan"),
            row("bad key", "x"),
            row("site", "cloud"),
        ])
        .unwrap_err();
        let at: Vec<usize> = errors.iter().map(|e| e.row).collect();
        assert_eq!(at, vec![1, 2, 3]);
        assert!(errors[2].message.contains("twice"));
        assert!(valid_key("topology.kubernetes.io/zone"));
        assert!(!valid_key("-leading"));
        assert!(!valid_key(""));
    }

    #[test]
    fn operator_labels_win_and_changes_are_listed() {
        let agent = map(&[("site", "home"), ("arch", "arm64")]);
        let server = map(&[("site", "cloud")]);
        assert_eq!(
            effective(&agent, &server),
            map(&[("arch", "arm64"), ("site", "cloud")])
        );
        let c = changes(
            &map(&[("a", "1"), ("b", "2")]),
            &map(&[("b", "3"), ("c", "4")]),
        );
        assert_eq!(
            c,
            vec![
                Change::Removed {
                    key: "a".into(),
                    value: "1".into()
                },
                Change::Changed {
                    key: "b".into(),
                    from: "2".into(),
                    to: "3".into()
                },
                Change::Added {
                    key: "c".into(),
                    value: "4".into()
                },
            ]
        );
    }
}
