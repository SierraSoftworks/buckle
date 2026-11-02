//! [`SecretField`]: the spec field type for the few task fields that may
//! legitimately hold secrets (user password hashes, `command.run` env values,
//! repository credentials).
//!
//! Ordinary spec strings are rendered by the engine with
//! [`Templater::render_plain`] before the spec is deserialised, which refuses
//! secrets. A `SecretField` instead keeps its raw template text through
//! deserialisation, and the task renders it at the point of use:
//! [`SecretField::resolve`] for the real value (a [`Rendered`], which is
//! [`Rendered::Sensitive`] when a secret was touched) and
//! [`SecretField::redacted`] for plans.
//!
//! The JSON schema of a `SecretField` is an inline string schema carrying the
//! extension keyword `"x-buckle-secret": true` ([`SECRET_SCHEMA_KEYWORD`]);
//! [`render_spec_strings`] uses it to leave such fields unrendered when the
//! engine pre-renders a spec.

use super::sinks::ProcessEnvSink;
use super::{Redacted, Rendered};
use crate::selectors::HostContext;
use crate::system::Command;
use crate::template::Templater;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::borrow::Cow;
use std::fmt;

/// The JSON-schema extension keyword marking a secret-capable field.
pub const SECRET_SCHEMA_KEYWORD: &str = "x-buckle-secret";

/// A task spec field that may reference secrets.
///
/// It deserialises from a string (a template or a literal), serialises back
/// to that raw text (never a resolved value), and its `Debug` prints the raw
/// text, which is safe because templates name secrets rather than contain
/// them.
///
/// ```
/// use buckle_core::secrets::{InstallKey, Rendered, Secret, SecretField, SecretStore};
/// use buckle_core::selectors::{HostContext, HostIdentity};
/// use buckle_core::template::Templater;
/// use std::sync::Arc;
///
/// let mut store = SecretStore::new();
/// store.insert(Secret::new("db", "hunter2".into(), &InstallKey::from_bytes([7u8; 32])));
/// let ctx = HostContext::new(HostIdentity::default()).with_secrets(Arc::new(store));
/// let templater = Templater::new();
///
/// let field: SecretField = serde_yaml::from_str("'{{ secrets.db }}'").unwrap();
/// assert!(field.references_secrets());
/// assert!(field.resolve(&templater, &ctx).unwrap().is_sensitive());
/// assert!(field.redacted(&templater, &ctx).unwrap().get().starts_with("\u{ab}secret:db@"));
/// assert_eq!(serde_json::to_string(&field).unwrap(), r#""{{ secrets.db }}""#);
///
/// let literal = SecretField::new("$6$rounds=5000$salt$hash");
/// assert!(matches!(literal.resolve(&templater, &ctx).unwrap(), Rendered::Plain(_)));
/// ```
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct SecretField(String);

impl SecretField {
    /// A field holding `raw` (a template or a literal).
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The raw template text, exactly as written in the module.
    pub fn raw(&self) -> &str {
        &self.0
    }

    /// Whether the raw text mentions `secrets.` (a textual check: the engine
    /// uses it to decide which secret names a task needs and to flag fields
    /// whose output must be treated as sensitive).
    pub fn references_secrets(&self) -> bool {
        self.0.contains("secrets.")
    }

    /// Renders with real values. A literal, or a template that touches no
    /// secret, yields [`Rendered::Plain`]; one that reveals a secret yields
    /// [`Rendered::Sensitive`]. Text without template syntax is returned
    /// as-is, without rendering, like every other spec string.
    pub fn resolve(&self, templater: &Templater, ctx: &HostContext) -> crate::Result<Rendered> {
        if Templater::needs_render(&self.0) {
            templater.render_sensitive(&self.0, ctx)
        } else {
            Ok(Rendered::Plain(self.0.clone()))
        }
    }

    /// Renders with secrets replaced by their tokens, for plans and shadows.
    pub fn redacted(
        &self,
        templater: &Templater,
        ctx: &HostContext,
    ) -> crate::Result<Redacted<String>> {
        if Templater::needs_render(&self.0) {
            templater.render_redacted(&self.0, ctx)
        } else {
            Ok(Redacted::plain(self.0.clone()))
        }
    }

    /// Resolves the field and places it in `command`'s environment as `name`
    /// through the [`ProcessEnvSink`], marking the command sensitive when a
    /// secret was revealed. The shape `command.run` needs for `env:`.
    pub fn inject_env(
        &self,
        command: &mut Command,
        name: impl Into<String>,
        templater: &Templater,
        ctx: &HostContext,
    ) -> crate::Result<()> {
        let rendered = self.resolve(templater, ctx)?;
        ProcessEnvSink.inject_rendered(command, name, &rendered);
        Ok(())
    }
}

impl From<String> for SecretField {
    fn from(raw: String) -> Self {
        Self(raw)
    }
}

impl From<&str> for SecretField {
    fn from(raw: &str) -> Self {
        Self(raw.to_owned())
    }
}

impl fmt::Debug for SecretField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretField({:?})", self.0)
    }
}

impl fmt::Display for SecretField {
    /// The raw template text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for SecretField {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SecretField {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(SecretField)
    }
}

impl schemars::JsonSchema for SecretField {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        "SecretField".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A literal or a template; may reference secrets (`{{ secrets.NAME }}`).",
            "x-buckle-secret": true,
        })
    }
}

/// Renders the template strings of a raw task spec in place, skipping every
/// value whose schema position is a [`SecretField`].
///
/// This is the hook for the engine's spec pre-render: call it with the spec
/// JSON, the kind's schema (`serde_json::to_value(task.schema())`) and a
/// closure that renders with [`Templater::render_plain`]. Only strings that
/// [`Templater::needs_render`] are passed to `render`. `$ref`s into `$defs`
/// (or `definitions`) and `allOf`/`anyOf`/`oneOf` are followed; a value whose
/// position the schema does not describe is rendered like any other string.
///
/// A rendered string whose schema position only admits integers, numbers or
/// booleans (no string) is converted to that type when it parses as one, so
/// `size_mb: "{{ vars.n }}"` reaches an integer field as `512`, not `"512"`.
///
/// ```
/// use buckle_core::secrets::field::render_spec_strings;
/// use buckle_core::secrets::SecretField;
///
/// #[derive(serde::Deserialize, schemars::JsonSchema)]
/// struct Spec {
///     name: String,
///     password: SecretField,
///     env: std::collections::BTreeMap<String, SecretField>,
/// }
///
/// let schema = serde_json::to_value(schemars::schema_for!(Spec)).unwrap();
/// let mut spec = serde_json::json!({
///     "name": "{{ vars.user }}",
///     "password": "{{ secrets.pw }}",
///     "env": { "TOKEN": "{{ secrets.token }}" },
/// });
/// render_spec_strings(&mut spec, &schema, &mut |s: &str| Ok(s.replace("{{ vars.user }}", "ada"))).unwrap();
/// assert_eq!(spec["name"], "ada");
/// assert_eq!(spec["password"], "{{ secrets.pw }}");
/// assert_eq!(spec["env"]["TOKEN"], "{{ secrets.token }}");
/// ```
pub fn render_spec_strings(
    value: &mut serde_json::Value,
    schema: &serde_json::Value,
    render: &mut dyn FnMut(&str) -> crate::Result<String>,
) -> crate::Result<()> {
    walk(value, &[schema], schema, render, 0)
}

/// Deeper than any real spec; guards against recursive schemas.
const MAX_DEPTH: usize = 64;

fn walk(
    value: &mut serde_json::Value,
    schemas: &[&serde_json::Value],
    root: &serde_json::Value,
    render: &mut dyn FnMut(&str) -> crate::Result<String>,
    depth: usize,
) -> crate::Result<()> {
    use serde_json::Value;
    let mut flat = Vec::new();
    for schema in schemas {
        flatten(schema, root, &mut flat, depth);
    }
    if flat.iter().any(|s| is_secret(s)) {
        return Ok(());
    }
    match value {
        Value::String(s) => {
            if Templater::needs_render(s) {
                let rendered = render(s)?;
                *value = coerce(rendered, &flat);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                let subs: Vec<&Value> = flat
                    .iter()
                    .filter_map(|s| {
                        s.get("prefixItems")
                            .and_then(|p| p.get(i))
                            .or_else(|| s.get("items"))
                    })
                    .collect();
                walk(item, &subs, root, render, depth + 1)?;
            }
        }
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                let subs: Vec<&Value> = flat
                    .iter()
                    .filter_map(|s| {
                        s.get("properties")
                            .and_then(|p| p.get(key.as_str()))
                            .or_else(|| s.get("additionalProperties").filter(|a| a.is_object()))
                    })
                    .collect();
                walk(item, &subs, root, render, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Converts a rendered string to the scalar type its schema position wants,
/// when every schema there excludes strings and the text parses as that type.
fn coerce(rendered: String, schemas: &[&serde_json::Value]) -> serde_json::Value {
    use serde_json::Value;
    let mut types: Vec<&str> = Vec::new();
    for schema in schemas {
        match schema.get("type") {
            Some(Value::String(t)) => types.push(t),
            Some(Value::Array(ts)) => types.extend(ts.iter().filter_map(Value::as_str)),
            _ => {}
        }
    }
    if types.is_empty() || types.contains(&"string") {
        return Value::String(rendered);
    }
    let text = rendered.trim();
    if types.contains(&"integer") {
        if let Ok(i) = text.parse::<i64>() {
            return Value::from(i);
        }
        if let Ok(u) = text.parse::<u64>() {
            return Value::from(u);
        }
    }
    if types.contains(&"number")
        && let Ok(f) = text.parse::<f64>()
        && let Some(n) = serde_json::Number::from_f64(f)
    {
        return Value::Number(n);
    }
    if types.contains(&"boolean") {
        match text.to_ascii_lowercase().as_str() {
            "true" => return Value::Bool(true),
            "false" => return Value::Bool(false),
            _ => {}
        }
    }
    if types.contains(&"null") && matches!(text, "" | "none" | "null" | "None") {
        return Value::Null;
    }
    Value::String(rendered)
}

/// Resolves `$ref`s and expands `allOf`/`anyOf`/`oneOf` into `out`.
fn flatten<'a>(
    schema: &'a serde_json::Value,
    root: &'a serde_json::Value,
    out: &mut Vec<&'a serde_json::Value>,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let schema = match schema.get("$ref").and_then(|r| r.as_str()) {
        Some(reference) => match resolve_ref(reference, root) {
            Some(target) => {
                // Sibling keywords next to `$ref` (2020-12) still apply.
                out.push(schema);
                target
            }
            None => schema,
        },
        None => schema,
    };
    out.push(schema);
    for combinator in ["allOf", "anyOf", "oneOf"] {
        if let Some(branches) = schema.get(combinator).and_then(|b| b.as_array()) {
            for branch in branches {
                flatten(branch, root, out, depth + 1);
            }
        }
    }
}

fn resolve_ref<'a>(reference: &str, root: &'a serde_json::Value) -> Option<&'a serde_json::Value> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        return Some(root);
    }
    root.pointer(pointer)
}

fn is_secret(schema: &serde_json::Value) -> bool {
    schema
        .get(SECRET_SCHEMA_KEYWORD)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{InstallKey, Secret, SecretStore};
    use crate::selectors::HostIdentity;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn ctx() -> HostContext {
        let mut store = SecretStore::new();
        store.insert(Secret::new(
            "pw",
            "hunter2".into(),
            &InstallKey::from_bytes([2u8; 32]),
        ));
        let mut vars = crate::selectors::Vars::new();
        vars.insert("user".into(), "ada".into());
        HostContext::new(HostIdentity::default())
            .with_vars(vars)
            .with_secrets(Arc::new(store))
    }

    #[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
    struct Inner {
        token: SecretField,
        label: String,
    }

    #[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
    #[serde(untagged)]
    enum Auth {
        Token { bearer: SecretField },
        Basic { user: String, password: SecretField },
    }

    #[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
    struct Spec {
        name: String,
        password: Option<SecretField>,
        env: BTreeMap<String, SecretField>,
        args: Vec<String>,
        inner: Inner,
        inners: Vec<Inner>,
        auth: Auth,
        #[serde(flatten)]
        extra: Flattened,
    }

    #[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
    struct Flattened {
        flat_secret: SecretField,
        flat_plain: String,
    }

    #[test]
    fn schema_is_an_inline_marked_string() {
        let schema = serde_json::to_value(schemars::schema_for!(SecretField)).unwrap();
        assert_eq!(schema["type"], "string");
        assert_eq!(schema[SECRET_SCHEMA_KEYWORD], true);
    }

    #[test]
    fn spec_pre_render_skips_secret_fields_everywhere() {
        let schema = serde_json::to_value(schemars::schema_for!(Spec)).unwrap();
        let t = "{{ secrets.pw }}";
        let v = "{{ vars.user }}";
        let mut spec = serde_json::json!({
            "name": v,
            "password": t,
            "env": {"A": t, "B": v},
            "args": [v, "-x"],
            "inner": {"token": t, "label": v},
            "inners": [{"token": t, "label": v}],
            "auth": {"user": v, "password": t},
            "flat_secret": t,
            "flat_plain": v,
            "unknown": v,
        });
        let templater = Templater::new();
        let ctx = ctx();
        render_spec_strings(&mut spec, &schema, &mut |s: &str| {
            templater.render_plain(s, &ctx)
        })
        .unwrap();
        for plain in [
            "/name",
            "/args/0",
            "/inner/label",
            "/inners/0/label",
            "/flat_plain",
            "/unknown",
        ] {
            assert_eq!(spec.pointer(plain).unwrap(), "ada", "{plain}");
        }
        for secret in [
            "/password",
            "/env/A",
            "/env/B",
            "/inner/token",
            "/inners/0/token",
            "/auth/password",
            "/flat_secret",
        ] {
            assert!(
                spec.pointer(secret)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .contains("{{"),
                "{secret}"
            );
        }
        assert_eq!(spec["args"][1], "-x");
        // The untagged enum's plain field is rendered only when no branch marks it secret.
        assert_eq!(spec["auth"]["user"], "ada");
        let parsed: Spec = serde_json::from_value(spec).unwrap();
        assert!(parsed.password.unwrap().references_secrets());
    }

    #[test]
    fn rendered_strings_take_the_scalar_type_their_field_wants() {
        #[derive(serde::Deserialize, schemars::JsonSchema)]
        #[allow(dead_code)]
        struct Spec {
            size_mb: u64,
            offset: i32,
            ratio: f64,
            enabled: bool,
            maybe: Option<u32>,
            name: String,
            mode: Option<String>,
            sizes: Vec<u16>,
        }
        let schema = serde_json::to_value(schemars::schema_for!(Spec)).unwrap();
        let mut spec = serde_json::json!({
            "size_mb": "{{ n }}",
            "offset": "{{ neg }}",
            "ratio": "{{ r }}",
            "enabled": "{{ yes }}",
            "maybe": "{{ empty }}",
            "name": "{{ n }}",
            "mode": "{{ n }}",
            "sizes": ["{{ n }}", 7],
        });
        let values: BTreeMap<&str, &str> = [
            ("{{ n }}", "512"),
            ("{{ neg }}", "-3"),
            ("{{ r }}", "0.5"),
            ("{{ yes }}", "True"),
            ("{{ empty }}", ""),
        ]
        .into();
        render_spec_strings(&mut spec, &schema, &mut |s: &str| Ok(values[s].to_owned())).unwrap();
        assert_eq!(
            spec,
            serde_json::json!({
                "size_mb": 512,
                "offset": -3,
                "ratio": 0.5,
                "enabled": true,
                "maybe": null,
                "name": "512",
                "mode": "512",
                "sizes": [512, 7],
            })
        );
        let parsed: Spec = serde_json::from_value(spec).unwrap();
        assert_eq!(parsed.size_mb, 512);
        // Text that does not parse stays a string, so the kind reports a type error.
        let mut bad = serde_json::json!({"size_mb": "{{ n }}"});
        render_spec_strings(&mut bad, &schema, &mut |_| Ok("lots".into())).unwrap();
        assert_eq!(bad["size_mb"], "lots");
    }

    #[test]
    fn render_errors_propagate() {
        let schema = serde_json::to_value(schemars::schema_for!(Inner)).unwrap();
        let mut spec = serde_json::json!({"token": "x", "label": "{{ nope }}"});
        let err = render_spec_strings(&mut spec, &schema, &mut |_| {
            Err(crate::error::user("bad template", &[]))
        })
        .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
    }

    #[test]
    fn literal_and_secret_free_fields_stay_plain() {
        let templater = Templater::new();
        let ctx = ctx();
        let user = SecretField::new("{{ vars.user }}");
        assert!(!user.references_secrets());
        assert!(
            matches!(user.resolve(&templater, &ctx).unwrap(), Rendered::Plain(ref s) if s == "ada")
        );
        assert_eq!(user.redacted(&templater, &ctx).unwrap().get(), "ada");
        let literal = SecretField::from("no template here");
        assert!(
            matches!(literal.resolve(&templater, &ctx).unwrap(), Rendered::Plain(ref s) if s == "no template here")
        );
    }

    #[test]
    fn inject_env_marks_commands_sensitive_only_for_secrets() {
        let templater = Templater::new();
        let ctx = ctx();
        let mut cmd = Command::new("x");
        SecretField::new("{{ vars.user }}")
            .inject_env(&mut cmd, "USER", &templater, &ctx)
            .unwrap();
        assert!(!cmd.sensitive_env);
        SecretField::new("{{ secrets.pw }}")
            .inject_env(&mut cmd, "PW", &templater, &ctx)
            .unwrap();
        assert!(cmd.sensitive_env);
        assert_eq!(cmd.env["PW"], "hunter2");
        assert!(!format!("{cmd:?}").contains("hunter2"));
    }

    #[test]
    fn missing_secrets_are_render_errors() {
        let err = SecretField::new("{{ secrets.absent }}")
            .resolve(&Templater::new(), &ctx())
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
    }
}
