//! HCL on the executor's side: workload `vars` as an HCL variables file for
//! `/v1/jobs/parse`, and entity spec files (namespaces, node pools, volumes,
//! quotas) decoded into the JSON shapes of Nomad's API, as `nomad namespace
//! apply` and friends do client-side.

use buckle_core::Result;
use buckle_core::error::user;
use indexmap::IndexMap;
use serde_json::{Map, Value};

/// Encodes `vars` as an HCL variables file (`name = value` lines), the form
/// `nomad job run -var-file` reads. Strings escape HCL's template sequences
/// (`${`, `%{`) so a value is passed through literally.
pub fn var_file(vars: &IndexMap<String, Value>) -> Result<String> {
    let mut out = String::new();
    for (name, value) in vars {
        if !is_identifier(name) {
            return Err(user(
                format!("'{name}' cannot be passed to Nomad as an HCL variable."),
                &[
                    "HCL variable names start with a letter or underscore and contain letters, digits, '_' and '-'.",
                ],
            ));
        }
        out.push_str(name);
        out.push_str(" = ");
        encode(value, &mut out);
        out.push('\n');
    }
    Ok(out)
}

/// A value as a `-var` flag holds it (a job submission's `VariableFlags`):
/// strings as they are, anything else as an HCL expression.
pub fn flag(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => {
            let mut out = String::new();
            encode(other, &mut out);
            out
        }
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn encode(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => quote(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                encode(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                quote(k, out);
                out.push_str(" = ");
                encode(v, out);
            }
            out.push('}');
        }
    }
}

fn quote(s: &str, out: &mut String) {
    out.push('"');
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' | '%' if chars.peek() == Some(&'{') => {
                out.push(c);
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Decodes an HCL file into JSON: attributes become fields, blocks become
/// objects (labelled blocks nest under their labels, repeated blocks become
/// arrays), as `hcl-rs` maps them.
pub fn decode(file: &str, text: &str) -> Result<Value> {
    hcl::from_str::<Value>(text).map_err(|e| {
        user(
            format!("{file} is not valid HCL: {e}"),
            &["Check the file against Nomad's documentation for its kind."],
        )
    })
}

/// How a decoded spec maps onto an API struct: fields are converted from
/// `snake_case` to `PascalCase` except where `renames` says otherwise, and
/// the children of `opaque` fields (user maps such as `meta`) keep their keys.
pub struct Shape {
    pub renames: &'static [(&'static str, &'static str)],
    pub opaque: &'static [&'static str],
    /// Blocks that the API holds as a list even when written once.
    pub lists: &'static [&'static str],
}

/// Converts a decoded spec to its API form under `shape`.
pub fn to_api(value: &Value, shape: &Shape) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let name = shape
                    .renames
                    .iter()
                    .find(|(from, _)| from == k)
                    .map(|(_, to)| (*to).to_owned())
                    .unwrap_or_else(|| pascal(k));
                let converted = if shape.opaque.contains(&k.as_str()) {
                    v.clone()
                } else {
                    to_api(v, shape)
                };
                let converted = if shape.lists.contains(&k.as_str()) && !converted.is_array() {
                    Value::Array(vec![converted])
                } else {
                    converted
                };
                out.insert(name, converted);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| to_api(v, shape)).collect()),
        other => other.clone(),
    }
}

/// `enabled_task_drivers` → `EnabledTaskDrivers`, `plugin_id` → `PluginID`.
pub fn pascal(snake: &str) -> String {
    snake
        .split('_')
        .map(|part| match part {
            "id" => "ID".to_owned(),
            "cpu" => "CPU".to_owned(),
            "acl" => "ACL".to_owned(),
            p => {
                let mut c = p.chars();
                match c.next() {
                    Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect()
}

/// Parses a size such as `10GiB`, `512MB` or `1073741824` into bytes.
pub fn bytes(size: &Value) -> Option<u64> {
    match size {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let s = s.trim();
            let split = s
                .find(|c: char| !c.is_ascii_digit() && c != '.')
                .unwrap_or(s.len());
            let (num, unit) = s.split_at(split);
            let num: f64 = num.parse().ok()?;
            let mult: f64 = match unit.trim().to_ascii_lowercase().as_str() {
                "" | "b" => 1.0,
                "kb" => 1e3,
                "kib" => 1024.0,
                "mb" => 1e6,
                "mib" => 1024.0 * 1024.0,
                "gb" => 1e9,
                "gib" => 1024.0 * 1024.0 * 1024.0,
                "tb" => 1e12,
                "tib" => 1024.0_f64.powi(4),
                _ => return None,
            };
            Some((num * mult) as u64)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vars_become_an_hcl_variables_file() {
        let vars: IndexMap<String, Value> = [
            ("version", json!("18.6.1")),
            ("count", json!(2)),
            ("tls", json!(true)),
            ("dcs", json!(["home", "lab"])),
            ("tags", json!({"a b": "c"})),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect();
        assert_eq!(
            var_file(&vars).unwrap(),
            "version = \"18.6.1\"\ncount = 2\ntls = true\ndcs = [\"home\", \"lab\"]\ntags = {\"a b\" = \"c\"}\n"
        );
    }

    #[rstest::rstest]
    #[case("plain", r#""plain""#)]
    #[case("${var.x} and %{ if }", r#""$${var.x} and %%{ if }""#)]
    #[case("a\"b\\c\nd", r#""a\"b\\c\nd""#)]
    #[case("$5 and 100%", r#""$5 and 100%""#)]
    fn strings_are_passed_literally(#[case] input: &str, #[case] expected: &str) {
        let mut out = String::new();
        quote(input, &mut out);
        assert_eq!(out, expected);
    }

    #[test]
    fn invalid_variable_names_are_rejected() {
        let vars: IndexMap<String, Value> = [("1bad".to_owned(), json!("x"))].into_iter().collect();
        assert!(var_file(&vars).is_err());
    }

    #[test]
    fn specs_decode_and_map_onto_api_fields() {
        let text = r#"
name        = "team-a"
description = "Team A's jobs"
meta { owner_team = "team-a" }
capabilities { enabled_task_drivers = ["docker"] }
node_pool_config {
  default = "pis"
  allowed = ["pis"]
}
"#;
        let shape = Shape {
            renames: &[("node_pool_config", "NodePoolConfiguration")],
            opaque: &["meta"],
            lists: &[],
        };
        let api = to_api(&decode("team-a.namespace.hcl", text).unwrap(), &shape);
        assert_eq!(
            api,
            json!({
                "Name": "team-a",
                "Description": "Team A's jobs",
                "Meta": {"owner_team": "team-a"},
                "Capabilities": {"EnabledTaskDrivers": ["docker"]},
                "NodePoolConfiguration": {"Default": "pis", "Allowed": ["pis"]},
            })
        );
    }

    #[test]
    fn invalid_hcl_names_the_file() {
        let err = decode("x.hcl", "name = ").unwrap_err();
        assert!(err.message().contains("x.hcl"));
    }

    #[rstest::rstest]
    #[case(json!("1GiB"), Some(1 << 30))]
    #[case(json!("500MB"), Some(500_000_000))]
    #[case(json!(42), Some(42))]
    #[case(json!("10 parsecs"), None)]
    fn sizes_parse(#[case] input: Value, #[case] expected: Option<u64>) {
        assert_eq!(bytes(&input), expected);
    }
}
