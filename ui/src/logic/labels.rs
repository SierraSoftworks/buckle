//! Editing a host's server labels (design §12): rows of key and value, checked
//! as they are typed, turned into the map `PATCH /hosts/{id}` and
//! `POST /hosts/{id}/preview` take.

use std::collections::BTreeMap;

use buckle_api::ReservedLabel;

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

/// Whether the fleet reserves `key` (`fleet.labels.reserved`; names compare
/// without case): only the server sets it, and trust decisions read it.
pub fn is_reserved<S: AsRef<str>>(key: &str, reserved: &[S]) -> bool {
    reserved
        .iter()
        .any(|r| r.as_ref().eq_ignore_ascii_case(key.trim()))
}

/// The keys the fleet reserves, as a host has them.
pub fn reserved_keys(reserved: &[ReservedLabel]) -> Vec<&str> {
    reserved.iter().map(|r| r.key.as_str()).collect()
}

/// A host's operator labels as the editor shows them: the reserved ones
/// first, each with its value (blank until one is set), then the rest.
pub fn split(
    labels: &BTreeMap<String, String>,
    reserved: &[ReservedLabel],
) -> (Vec<Row>, Vec<Row>) {
    let keys = reserved_keys(reserved);
    let own = reserved
        .iter()
        .map(|r| Row {
            key: r.key.clone(),
            value: r.value.clone().unwrap_or_default(),
        })
        .collect();
    let rest = rows(labels)
        .into_iter()
        .filter(|row| !is_reserved(&row.key, &keys))
        .collect();
    (own, rest)
}

/// What is wrong with the editor's lists: each problem by the row of the
/// rest it is on, or `None` for one among the reserved labels.
pub type Problems = Vec<(Option<usize>, String)>;

/// The label map the editor's two lists describe together. A reserved
/// label without a value is left out; one that had a value cannot lose it,
/// and the other rows cannot name a reserved key.
pub fn join(
    own: &[Row],
    rest: &[Row],
    reserved: &[ReservedLabel],
) -> Result<BTreeMap<String, String>, Problems> {
    let keys = reserved_keys(reserved);
    let mut errors = Problems::new();
    let mut out = match collect(rest) {
        Ok(labels) => labels,
        Err(found) => {
            errors.extend(found.into_iter().map(|e| (Some(e.row), e.message)));
            BTreeMap::new()
        }
    };
    for (i, row) in rest.iter().enumerate() {
        if is_reserved(&row.key, &keys) {
            errors.push((
                Some(i),
                format!("'{}' is reserved: set its value above.", row.key.trim()),
            ));
        }
    }
    for row in own {
        let value = row.value.trim();
        let had = reserved
            .iter()
            .any(|r| r.key == row.key && r.value.is_some());
        if !value.is_empty() {
            out.insert(row.key.clone(), value.to_owned());
        } else if had {
            errors.push((
                None,
                format!(
                    "'{}' is reserved: its value can change, but it cannot be removed.",
                    row.key
                ),
            ));
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// The reserved labels a host has no value for yet: what an approval asks
/// for first.
pub fn unset(reserved: &[ReservedLabel]) -> Vec<&ReservedLabel> {
    reserved.iter().filter(|r| r.value.is_none()).collect()
}

/// The values given for reserved `keys` (a form's fields), or the first key
/// left without one.
pub fn reserved_values<S: AsRef<str>>(
    keys: &[S],
    given: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    keys.iter()
        .map(|k| {
            let key = k.as_ref();
            match given.get(key).map(|v| v.trim()).filter(|v| !v.is_empty()) {
                Some(v) => Ok((key.to_owned(), v.to_owned())),
                None => Err(format!(
                    "Give '{key}' a value: the fleet reserves it, so the server sets it and a host needs one to join."
                )),
            }
        })
        .collect()
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

    fn reserved(key: &str, value: Option<&str>) -> ReservedLabel {
        ReservedLabel {
            key: key.into(),
            value: value.map(Into::into),
            reported: None,
        }
    }

    #[test]
    fn reserved_labels_are_edited_apart_from_the_rest() {
        let fleet = [reserved("zone", Some("home")), reserved("tier", None)];
        let labels = map(&[("zone", "home"), ("site", "attic")]);
        let (own, rest) = split(&labels, &fleet);
        assert_eq!(own, vec![row("zone", "home"), row("tier", "")]);
        assert_eq!(rest, vec![row("site", "attic")]);
        assert_eq!(
            join(&own, &rest, &fleet),
            Ok(labels),
            "one without a value is left out"
        );
        assert_eq!(
            join(&[row("zone", "public"), row("tier", "edge")], &rest, &fleet),
            Ok(map(&[
                ("zone", "public"),
                ("tier", "edge"),
                ("site", "attic")
            ]))
        );
    }

    #[test]
    fn a_reserved_label_is_changed_only_in_its_own_row() {
        type Pairs = &'static [(&'static str, &'static str)];
        let cases: [(Pairs, Pairs, Option<usize>, &str); 3] = [
            (&[("zone", "")], &[], None, "cannot be removed"),
            (
                &[("zone", "home")],
                &[("Zone", "x")],
                Some(0),
                "set its value above",
            ),
            (
                &[("zone", "home")],
                &[("bad key", "x")],
                Some(0),
                "not a valid key",
            ),
        ];
        let rows = |pairs: Pairs| -> Vec<Row> { pairs.iter().map(|(k, v)| row(k, v)).collect() };
        for (own, rest, at, says) in cases {
            let errors =
                join(&rows(own), &rows(rest), &[reserved("zone", Some("home"))]).unwrap_err();
            assert!(
                errors.iter().any(|(row, m)| *row == at && m.contains(says)),
                "{says}: {errors:?}"
            );
        }
    }

    #[test]
    fn a_form_needs_a_value_for_every_reserved_label() {
        let fleet = [reserved("zone", None), reserved("tier", Some("edge"))];
        let missing: Vec<&str> = unset(&fleet).iter().map(|r| r.key.as_str()).collect();
        assert_eq!(missing, ["zone"]);
        assert_eq!(
            reserved_values(&missing, &map(&[("zone", " public ")])),
            Ok(map(&[("zone", "public")]))
        );
        let err = reserved_values(&missing, &map(&[("zone", " ")])).unwrap_err();
        assert!(err.contains("'zone'"), "{err}");
        assert!(is_reserved(" ZONE ", &missing));
        assert!(!is_reserved("site", &missing));
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
