//! The audit page's filter box (design §12): a filt-rs expression, checked
//! with the same parser the server uses before it is sent, and built up from
//! clicks on a row's actor, action or subject.

#[cfg(any(test, debug_assertions))]
use buckle_api::AuditEntry;
use filt_rs::Filter;
#[cfg(any(test, debug_assertions))]
use filt_rs::{FilterValue, Filterable};

/// The audit entry properties the server's filter can see.
pub const AUDIT_FIELDS: &[&str] = &[
    "id", "at", "actor", "action", "outcome", "subject", "message", "trace_id", "detail.*",
];

/// What the box can say about an expression before it is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Nothing to filter by.
    Empty,
    Valid,
    /// It would be refused; the message says where and why.
    Invalid {
        message: String,
        advice: Vec<String>,
    },
    /// It uses an operator this build cannot check (`matches` needs the regex
    /// engine the UI does not carry), so the server decides.
    Unchecked,
}

pub fn check(expression: &str) -> Check {
    let trimmed = expression.trim();
    if trimmed.is_empty() {
        return Check::Empty;
    }
    match Filter::new(trimmed) {
        Ok(_) => Check::Valid,
        Err(_) if uses_matches(trimmed) => Check::Unchecked,
        Err(e) => Check::Invalid {
            message: e.description(),
            advice: e.advice().into_iter().map(str::to_string).collect(),
        },
    }
}

/// Whether the expression mentions the `matches` operator outside a string.
fn uses_matches(expression: &str) -> bool {
    let mut in_string = false;
    let mut escaped = false;
    let mut word = String::new();
    for c in expression.chars().chain(std::iter::once(' ')) {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            word.clear();
        } else if c.is_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            if word == "matches" {
                return true;
            }
            word.clear();
        }
    }
    false
}

/// A filt-rs string literal for `value`.
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `field == "value"`.
pub fn equals(field: &str, value: &str) -> String {
    format!("{field} == {}", quote(value))
}

/// Narrows an existing expression with another clause. An empty expression is
/// replaced; an existing one is parenthesised only when it has a top-level
/// `||`, which `&&` would otherwise bind into.
pub fn and(existing: &str, clause: &str) -> String {
    let existing = existing.trim();
    if existing.is_empty() {
        return clause.to_string();
    }
    if existing.contains(clause) {
        return existing.to_string();
    }
    if has_top_level_or(existing) {
        format!("({existing}) && {clause}")
    } else {
        format!("{existing} && {clause}")
    }
}

fn has_top_level_or(expression: &str) -> bool {
    let (mut depth, mut in_string, mut escaped, mut last) = (0i32, false, false, ' ');
    for c in expression.chars() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            last = c;
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            '|' if last == '|' && depth == 0 => return true,
            _ => {}
        }
        last = c;
    }
    false
}

/// An audit entry as the server's filter sees it (`store/audit.rs` on the
/// server), so demo mode filters exactly as a server would. Demo mode only
/// exists in debug builds, and so does this.
#[cfg(any(test, debug_assertions))]
pub struct AuditView<'a>(pub &'a AuditEntry);

#[cfg(any(test, debug_assertions))]
impl Filterable for AuditView<'_> {
    fn get(&self, key: &str) -> FilterValue<'_> {
        let e = self.0;
        match key.to_ascii_lowercase().as_str() {
            "id" => FilterValue::Number(e.id as f64),
            "at" => FilterValue::DateTime(e.at),
            "actor" => e.actor.as_str().into(),
            "action" => e.action.as_str().into(),
            "outcome" => e.outcome.as_str().into(),
            "subject" => e.subject.as_deref().into(),
            "message" => e.message.as_str().into(),
            "trace_id" => e.trace_id.as_deref().into(),
            other => match (other.strip_prefix("detail."), &e.detail) {
                (Some(path), Some(detail)) => {
                    let mut v = detail;
                    for part in path.split('.') {
                        match v.get(part) {
                            Some(next) => v = next,
                            None => return FilterValue::Null,
                        }
                    }
                    json_value(v)
                }
                _ => FilterValue::Null,
            },
        }
    }
}

#[cfg(any(test, debug_assertions))]
fn json_value(v: &serde_json::Value) -> FilterValue<'_> {
    match v {
        serde_json::Value::Null => FilterValue::Null,
        serde_json::Value::Bool(b) => FilterValue::Bool(*b),
        serde_json::Value::Number(n) => FilterValue::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => s.as_str().into(),
        serde_json::Value::Array(a) => FilterValue::Tuple(a.iter().map(json_value).collect()),
        serde_json::Value::Object(_) => FilterValue::String(v.to_string().into()),
    }
}

/// Filters entries the way `GET /audit?filter=` does; an expression that does
/// not parse matches nothing.
#[cfg(any(test, debug_assertions))]
pub fn filter_entries<'a>(entries: &'a [AuditEntry], expression: &str) -> Vec<&'a AuditEntry> {
    let expression = expression.trim();
    if expression.is_empty() {
        return entries.iter().collect();
    }
    let Ok(filter) = Filter::new(expression) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|e| filter.matches(&AuditView(e)).unwrap_or(false))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use serde_json::json;

    fn entry(id: u64, actor: &str, action: &str, subject: Option<&str>) -> AuditEntry {
        AuditEntry {
            id,
            at: Utc::now() - Duration::minutes(id as i64),
            actor: actor.into(),
            action: action.into(),
            outcome: "success".into(),
            subject: subject.map(Into::into),
            message: format!("{actor} did {action}"),
            detail: Some(json!({"phase": "canary", "hosts": 3})),
            trace_id: None,
        }
    }

    #[test]
    fn expressions_are_checked_with_the_servers_parser() {
        assert_eq!(check("  "), Check::Empty);
        assert_eq!(
            check(r#"actor == "alice" && action startswith "rollout.""#),
            Check::Valid
        );
        assert_eq!(check("at > now() - 1h"), Check::Valid);
        match check(r#"actor == "alice" &&"#) {
            Check::Invalid { message, .. } => assert!(!message.is_empty()),
            other => panic!("expected invalid, got {other:?}"),
        }
        match check("(actor == \"a\"") {
            Check::Invalid { .. } => {}
            other => panic!("expected invalid, got {other:?}"),
        }
        assert_eq!(check(r#"message matches r"^ali""#), Check::Unchecked);
        // `matches` inside a string literal is just text.
        assert!(matches!(
            check(r#"message == "matches" &&"#),
            Check::Invalid { .. }
        ));
    }

    #[test]
    fn clicked_values_become_quoted_clauses() {
        assert_eq!(quote(r#"say "hi" \o/"#), r#""say \"hi\" \\o/""#);
        assert_eq!(equals("actor", "alice"), r#"actor == "alice""#);
        assert_eq!(and("", r#"actor == "a""#), r#"actor == "a""#);
        assert_eq!(
            and(r#"x == 1"#, r#"actor == "a""#),
            r#"x == 1 && actor == "a""#
        );
        assert_eq!(and("a || b", "c"), "(a || b) && c");
        assert_eq!(and("(a || b) && d", "c"), "(a || b) && d && c");
        assert_eq!(and(r#"m == "x || y""#, "c"), r#"m == "x || y" && c"#);
        assert_eq!(
            and("a && c", "c"),
            "a && c",
            "the same clause twice is once"
        );
        for built in [
            and("a || b", &equals("subject", "pi-1")),
            equals("actor", "o\"k"),
        ] {
            assert_eq!(check(&built), Check::Valid, "{built}");
        }
    }

    #[test]
    fn demo_filtering_sees_what_the_server_sees() {
        let entries = vec![
            entry(1, "alice", "rollout.approve", Some("42")),
            entry(2, "controller", "rollout.pause", Some("42")),
            entry(3, "bob", "host.update", Some("pi-1")),
        ];
        let ids = |expr: &str| -> Vec<u64> {
            filter_entries(&entries, expr)
                .iter()
                .map(|e| e.id)
                .collect()
        };
        assert_eq!(ids(""), vec![1, 2, 3]);
        assert_eq!(ids(r#"action startswith "rollout.""#), vec![1, 2]);
        assert_eq!(ids(r#"subject == "pi-1" || actor == "ALICE""#), vec![1, 3]);
        assert_eq!(
            ids(r#"detail.phase == "canary" && detail.hosts > 2"#),
            vec![1, 2, 3]
        );
        assert_eq!(ids("at > now() - 150s"), vec![1, 2]);
        assert_eq!(ids("this is not a filter ((("), Vec::<u64>::new());
    }
}
