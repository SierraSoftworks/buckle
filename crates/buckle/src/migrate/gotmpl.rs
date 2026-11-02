//! Converts a v1 Go template (`*.tpl`) into a v2 minijinja template (`*.j2`).
//!
//! Only what maps one-to-one is rewritten: `{{ .KEY }}` becomes
//! `{{ vars.KEY }}` (or `{{ secrets.KEY }}` when KEY is a secret, which v1
//! merged over config of the same name) and `{{/* comment */}}` becomes
//! `{# comment #}`. Every other action (`if`, `range`, `printf`, pipelines,
//! field chains, …) is left exactly as written and reported, because choosing
//! its Jinja equivalent is a human decision. Text between actions is kept, with
//! `{%` and `{#` escaped so that Jinja does not mistake them for syntax.

use std::collections::BTreeSet;

pub struct Conversion {
    pub text: String,
    /// What a person still has to look at, one line each.
    pub todos: Vec<String>,
}

/// A name usable as `vars.NAME`: identifier characters only, not starting with a digit.
pub fn identifier(raw: &str) -> String {
    let mut name: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

/// `defined` is every key v1 defined statically for this package; `secrets`
/// those that were secrets.
pub fn convert(source: &str, secrets: &BTreeSet<String>, defined: &BTreeSet<String>) -> Conversion {
    let mut text = String::with_capacity(source.len());
    let mut todos = Vec::new();
    let mut rest = source;
    loop {
        let Some(open) = rest.find("{{") else {
            text.push_str(&escape_text(rest));
            break;
        };
        text.push_str(&escape_text(&rest[..open]));
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            todos.push("an unterminated `{{` was kept as written".to_owned());
            text.push_str(&rest[open..]);
            break;
        };
        let action = &after[..close];
        rest = &after[close + 2..];
        text.push_str(&convert_action(action, secrets, defined, &mut todos));
    }
    Conversion { text, todos }
}

/// Keeps Jinja's block and comment openers from appearing in plain text.
fn escape_text(text: &str) -> String {
    text.replace("{%", "{{ '{%' }}").replace("{#", "{{ '{#' }}")
}

fn convert_action(
    raw: &str,
    secrets: &BTreeSet<String>,
    defined: &BTreeSet<String>,
    todos: &mut Vec<String>,
) -> String {
    // Go trims with `{{- ` and ` -}}`; Jinja spells the markers `{{-` and `-}}`.
    let (left_trim, body) = match raw.strip_prefix('-') {
        Some(rest) if rest.starts_with(char::is_whitespace) => (true, rest),
        _ => (false, raw),
    };
    let (right_trim, body) = match body.strip_suffix('-') {
        Some(rest) if rest.ends_with(char::is_whitespace) => (true, rest),
        _ => (false, body),
    };
    let body = body.trim();
    let (open, close) = (
        if left_trim { "-" } else { "" },
        if right_trim { "-" } else { "" },
    );

    if let Some(comment) = body
        .strip_prefix("/*")
        .and_then(|b| b.strip_suffix("*/"))
        .filter(|c| !c.contains("#}"))
    {
        return format!("{{#{open} {} {close}#}}", comment.trim());
    }

    if let Some(key) = body.strip_prefix('.').filter(|k| is_simple_key(k)) {
        let name = identifier(key);
        let scope = if secrets.contains(key) {
            "secrets"
        } else {
            if !defined.contains(key) {
                todos.push(format!(
                    "`.{key}` is not defined by any static v1 config; it may have come from a config script"
                ));
            }
            "vars"
        };
        return format!("{{{{{open} {scope}.{name} {close}}}}}");
    }

    todos.push(format!(
        "the Go template action `{{{{{raw}}}}}` was kept as written"
    ));
    format!("{{{{{raw}}}}}")
}

fn is_simple_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn set(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    #[rstest]
    #[case("enabled = {{ .enabled }}", "enabled = {{ vars.enabled }}")]
    #[case("{{.A}}-{{ .B }}", "{{ vars.A }}-{{ vars.B }}")]
    #[case("a\n{{- .A -}}\nb", "a\n{{- vars.A -}}\nb")]
    #[case("{{ .TOKEN }}", "{{ secrets.TOKEN }}")]
    #[case("{{/* note */}}x", "{# note #}x")]
    #[case("plain text, no actions", "plain text, no actions")]
    #[case("${#array[@]} {% raw %}", "${{ '{#' }}array[@]} {{ '{%' }} raw %}")]
    fn simple_actions_are_rewritten(#[case] go: &str, #[case] jinja: &str) {
        let converted = convert(go, &set(&["TOKEN"]), &set(&["enabled", "A", "B"]));
        assert_eq!(converted.text, jinja);
        assert!(converted.todos.is_empty(), "{:?}", converted.todos);
    }

    #[rstest]
    #[case("{{ if .A }}x{{ end }}")]
    #[case("{{ range .Items }}{{ . }}{{ end }}")]
    #[case("{{ .A | printf \"%s\" }}")]
    #[case("{{ .Nested.field }}")]
    #[case("{{ $x := .A }}")]
    #[case("{{ unterminated")]
    fn other_actions_are_kept_and_reported(#[case] go: &str) {
        let converted = convert(go, &set(&[]), &set(&["A"]));
        assert_eq!(
            converted.text, go,
            "unconverted actions are kept as written"
        );
        assert!(!converted.todos.is_empty(), "and reported");
    }

    #[test]
    fn converted_text_renders_back_to_what_v1_would_have_printed() {
        use buckle_core::selectors::{HostContext, HostIdentity, Vars};
        let go = "${#x} and {% y %} and {# z #}: {{ .A }}\n";
        let converted = convert(go, &set(&[]), &set(&["A"]));
        let vars: Vars = serde_json::from_value(serde_json::json!({"A": "1"})).unwrap();
        let ctx = HostContext::new(HostIdentity::default()).with_vars(vars);
        let rendered = buckle_core::template::Templater::new()
            .render_plain(&converted.text, &ctx)
            .expect("the converted template renders");
        assert_eq!(rendered, "${#x} and {% y %} and {# z #}: 1\n");
    }

    #[test]
    fn unknown_keys_become_vars_with_a_note() {
        let converted = convert("{{ .FROM_SCRIPT }}", &set(&[]), &set(&[]));
        assert_eq!(converted.text, "{{ vars.FROM_SCRIPT }}");
        assert_eq!(converted.todos.len(), 1);
    }

    #[rstest]
    #[case("FOO", "FOO")]
    #[case("x.version", "x_version")]
    #[case("1st", "_1st")]
    #[case("a b-c", "a_b_c")]
    #[case("", "_")]
    fn identifiers_are_made_safe(#[case] raw: &str, #[case] expected: &str) {
        assert_eq!(identifier(raw), expected);
    }
}
