//! Secret-aware Jinja templating with [`minijinja`].
//!
//! Three render modes share one environment and one set of filters:
//!
//! * [`Templater::render_plain`] — for task spec fields. A reference to a
//!   secret is an error, so a secret cannot end up in a plain `String` that
//!   gets logged or planned.
//! * [`Templater::render_redacted`] — secrets render as their token. Used for
//!   plan previews and the state shadow.
//! * [`Templater::render_sensitive`] — secrets render as their value. The
//!   result is [`Sensitive`] and can only leave through a sink.

use crate::secrets::{Redacted, Secret, SecretStore, Sensitive};
use crate::selectors::HostContext;
use minijinja::value::{Enumerator, Object, Value};
use minijinja::{Environment, UndefinedBehavior};
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Secrets are an error.
    Plain,
    /// Secrets render as `«secret:NAME@FP»`.
    Redacted,
    /// Secrets render as their value.
    Real,
}

/// A configured minijinja environment. One per revision: templates are added
/// by path so `{% include %}` and `{% import %}` resolve within the module.
pub struct Templater {
    env: Environment<'static>,
}

impl Default for Templater {
    fn default() -> Self {
        Self::new()
    }
}

impl Templater {
    pub fn new() -> Self {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env.set_syntax(
            minijinja::syntax::SyntaxConfig::builder()
                .keep_trailing_newline(true)
                .build()
                .expect("default syntax with trailing newlines kept is valid"),
        );
        register_filters(&mut env);
        Self { env }
    }

    /// Makes a named template available for includes and imports.
    pub fn add_template(
        &mut self,
        name: impl Into<String>,
        source: impl Into<String>,
    ) -> crate::Result<()> {
        use crate::error::HumanizableResult;
        self.env
            .add_template_owned(name.into(), source.into())
            .to_human_error()
    }

    /// Whether a string needs rendering at all.
    pub fn needs_render(s: &str) -> bool {
        s.contains("{{") || s.contains("{%")
    }

    /// Compiles `source` without rendering it, reporting syntax errors.
    pub fn check(&self, source: &str) -> crate::Result<()> {
        use crate::error::HumanizableResult;
        self.env
            .template_from_str(source)
            .to_human_error()
            .map(|_| ())
    }

    /// The dotted variable paths a template reads from its context
    /// (`vars.nomad_version`, `host.facts.arch`, `secrets.token`), so the
    /// engine can resolve vars lazily and detect reference cycles.
    pub fn referenced_paths(&self, source: &str) -> crate::Result<Vec<String>> {
        use crate::error::HumanizableResult;
        let template = self.env.template_from_str(source).to_human_error()?;
        let mut paths: Vec<String> = template.undeclared_variables(true).into_iter().collect();
        paths.sort();
        Ok(paths)
    }

    /// Renders with secrets forbidden. For task spec fields.
    pub fn render_plain(&self, source: &str, ctx: &HostContext) -> crate::Result<String> {
        self.render(source, ctx, RenderMode::Plain)
    }

    /// Renders with secrets replaced by their tokens.
    pub fn render_redacted(
        &self,
        source: &str,
        ctx: &HostContext,
    ) -> crate::Result<Redacted<String>> {
        self.render(source, ctx, RenderMode::Redacted)
            .map(Redacted::new)
    }

    /// Renders with real secret values. The output is sensitive if (and only if)
    /// a secret was referenced; callers get a [`crate::secrets::Rendered`] so
    /// secret-free templates stay printable.
    pub fn render_sensitive(
        &self,
        source: &str,
        ctx: &HostContext,
    ) -> crate::Result<crate::secrets::Rendered> {
        let view = SecretsView::new(ctx.secrets.clone(), RenderMode::Real);
        let touched = view.touched.clone();
        let out = self.render_with_view(source, ctx, view)?;
        if touched.load(std::sync::atomic::Ordering::Relaxed) {
            Ok(crate::secrets::Rendered::Sensitive(Sensitive::new(out)))
        } else {
            Ok(crate::secrets::Rendered::Plain(out))
        }
    }

    fn render(&self, source: &str, ctx: &HostContext, mode: RenderMode) -> crate::Result<String> {
        self.render_with_view(source, ctx, SecretsView::new(ctx.secrets.clone(), mode))
    }

    fn render_with_view(
        &self,
        source: &str,
        ctx: &HostContext,
        view: SecretsView,
    ) -> crate::Result<String> {
        let mode = view.mode;
        let touched = view.touched.clone();
        // The JSON context (host, vars, module, task) plus the secrets object.
        let mut map: std::collections::BTreeMap<String, Value> = match ctx.template_json() {
            serde_json::Value::Object(obj) => obj
                .into_iter()
                .map(|(k, v)| (k, Value::from(minijinja::value::Serde(v))))
                .collect(),
            _ => Default::default(),
        };
        map.insert("secrets".into(), Value::from_object(view));
        self.env
            .render_str(source, Value::from(minijinja::value::Serde(map)))
            .map_err(|e| {
                // minijinja's error Debug dumps the referenced variables, which
                // would include secret values: scrub through the secrets module.
                let err = crate::secrets::redact::template_error(e, &ctx.secrets);
                if mode == RenderMode::Plain && touched.load(std::sync::atomic::Ordering::Relaxed) {
                    crate::error::wrap_user(
                        err,
                        "The template references a secret's value, but this field only accepts plain values.",
                        &[
                            "Use the secret in a file template, or in a task field that is declared as secret-capable.",
                            "`secrets.NAME.present` and `secrets.NAME.fingerprint` are always allowed.",
                        ],
                    )
                } else {
                    err
                }
            })
    }
}

/// The `secrets` template variable.
#[derive(Debug)]
struct SecretsView {
    store: Arc<SecretStore>,
    mode: RenderMode,
    touched: Arc<std::sync::atomic::AtomicBool>,
}

impl SecretsView {
    fn new(store: Arc<SecretStore>, mode: RenderMode) -> Self {
        Self {
            store,
            mode,
            touched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }
}

impl Object for SecretsView {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let name = key.as_str()?;
        let secret = self.store.get(name)?.clone();
        Some(Value::from_object(SecretValue {
            secret,
            mode: self.mode,
            touched: self.touched.clone(),
        }))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Values(self.store.names().map(Value::from).collect())
    }
}

/// One secret as seen by a template. Rendering it reveals or redacts according
/// to the mode; `.present` and `.fingerprint` are always available.
struct SecretValue {
    secret: Secret,
    mode: RenderMode,
    touched: Arc<std::sync::atomic::AtomicBool>,
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.secret.token())
    }
}

impl Object for SecretValue {
    fn repr(self: &Arc<Self>) -> minijinja::value::ObjectRepr {
        minijinja::value::ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        match key.as_str()? {
            "present" => Some(Value::from(true)),
            "fingerprint" => Some(Value::from(self.secret.fingerprint().as_hex())),
            "name" => Some(Value::from(self.secret.name())),
            _ => None,
        }
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.mode {
            RenderMode::Plain => {
                // Signal the error through the formatter; the caller maps it to a user error.
                self.touched
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                Err(fmt::Error)
            }
            RenderMode::Redacted => f.write_str(&self.secret.token()),
            RenderMode::Real => {
                self.touched
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                f.write_str(self.secret.reveal(crate::secrets::sinks::Witness::new()))
            }
        }
    }

    fn is_true(self: &Arc<Self>) -> bool {
        true
    }
}

fn register_filters(env: &mut Environment<'static>) {
    // Ansible-flavoured conveniences that the ported templates use.
    env.add_filter("to_json", |v: Value| -> Result<String, minijinja::Error> {
        serde_json::to_string(&v).map_err(|e| {
            minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string())
        })
    });
    env.add_filter(
        "to_nice_json",
        |v: Value| -> Result<String, minijinja::Error> {
            serde_json::to_string_pretty(&v).map_err(|e| {
                minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string())
            })
        },
    );
    env.add_filter("to_yaml", |v: Value| -> Result<String, minijinja::Error> {
        serde_yaml::to_string(&v).map_err(|e| {
            minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, e.to_string())
        })
    });
    env.add_filter("bool", |v: Value| -> bool {
        match v.as_str() {
            Some(s) => matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "true" | "yes" | "on" | "1"
            ),
            None => v.is_true(),
        }
    });
    env.add_filter("ternary", |v: Value, yes: Value, no: Value| -> Value {
        if v.is_true() { yes } else { no }
    });
    env.add_filter(
        "regex_replace",
        |v: String, pattern: String, replacement: String| -> Result<String, minijinja::Error> {
            let re = regex::Regex::new(&pattern).map_err(|e| {
                minijinja::Error::new(
                    minijinja::ErrorKind::InvalidOperation,
                    format!("invalid regex: {e}"),
                )
            })?;
            Ok(re.replace_all(&v, replacement.as_str()).into_owned())
        },
    );
    env.add_filter(
        "regex_search",
        |v: String, pattern: String| -> Result<bool, minijinja::Error> {
            let re = regex::Regex::new(&pattern).map_err(|e| {
                minijinja::Error::new(
                    minijinja::ErrorKind::InvalidOperation,
                    format!("invalid regex: {e}"),
                )
            })?;
            Ok(re.is_match(&v))
        },
    );
    env.add_filter("basename", |v: String| -> String {
        std::path::Path::new(&v)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    env.add_filter("dirname", |v: String| -> String {
        std::path::Path::new(&v)
            .parent()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    env.add_filter("comment", |v: String, prefix: Option<String>| -> String {
        let prefix = prefix.unwrap_or_else(|| "# ".to_owned());
        v.lines()
            .map(|l| format!("{prefix}{l}"))
            .collect::<Vec<_>>()
            .join("\n")
    });
    env.add_global(
        "buckle_managed",
        Value::from("Managed by Buckle. Local changes will be overwritten."),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::{InstallKey, Secret, SecretStore};
    use crate::selectors::{HostIdentity, Vars};
    use rstest::rstest;

    fn ctx() -> HostContext {
        let mut vars = Vars::new();
        vars.insert("name".into(), "nomad".into());
        vars.insert("ports".into(), serde_json::json!([80, 443]));
        vars.insert("attrs".into(), serde_json::json!({"a": 1, "b": "two"}));
        let mut secrets = SecretStore::new();
        secrets.insert(Secret::new(
            "token",
            "hunter2".into(),
            &InstallKey::from_bytes([3u8; 32]),
        ));
        HostContext::new(HostIdentity {
            id: "pi-1".into(),
            ..Default::default()
        })
        .with_facts(serde_json::json!({"arch": "aarch64"}))
        .with_vars(vars)
        .with_secrets(Arc::new(secrets))
    }

    #[rstest]
    #[case("{{ vars.name }}-{{ host.facts.arch }}", "nomad-aarch64")]
    #[case("{% for p in vars.ports %}{{ p }} {% endfor %}", "80 443 ")]
    #[case(
        "{% for k, v in vars.attrs | items %}{{ k }}={{ v }};{% endfor %}",
        "a=1;b=two;"
    )]
    #[case("{{ vars.missing | default('x') }}", "x")]
    #[case("{{ 'v1.2.3' | regex_replace('^v', '') }}", "1.2.3")]
    #[case("{{ vars.ports | to_json }}", "[80,443]")]
    #[case("{{ true | ternary('yes', 'no') }}", "yes")]
    #[case("{{ 'a\\nb' | comment }}", "# a\n# b")]
    #[case("{{ '/etc/nomad.d/nomad.hcl' | basename }}", "nomad.hcl")]
    #[case("{{ 'yes' | bool }}", "True")]
    #[case(
        "{{ secrets.token.present }} {{ secrets.token.fingerprint | length }}",
        "True 8"
    )]
    fn plain_rendering_covers_the_ansible_idioms(#[case] tpl: &str, #[case] expected: &str) {
        let out = Templater::new()
            .render_plain(tpl, &ctx())
            .unwrap_or_else(|e| panic!("{tpl}: {}", e.message()));
        assert_eq!(out, expected);
    }

    #[test]
    fn plain_rendering_refuses_secret_values() {
        let err = Templater::new()
            .render_plain("x={{ secrets.token }}", &ctx())
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        assert!(!err.message().contains("hunter2"));
    }

    #[test]
    fn redacted_rendering_substitutes_tokens() {
        let out = Templater::new()
            .render_redacted("x={{ secrets.token }}", &ctx())
            .unwrap();
        assert!(out.get().starts_with("x=\u{ab}secret:token@"));
        assert!(!out.get().contains("hunter2"));
    }

    #[test]
    fn sensitive_rendering_reveals_and_taints() {
        let rendered = Templater::new()
            .render_sensitive("x={{ secrets.token }}", &ctx())
            .unwrap();
        match rendered {
            crate::secrets::Rendered::Sensitive(s) => {
                assert_eq!(s.expose(crate::secrets::sinks::Witness::new()), "x=hunter2");
                assert_eq!(format!("{s:?}"), "Sensitive(..)");
            }
            other => panic!("expected sensitive output, got {other:?}"),
        }
    }

    #[test]
    fn sensitive_rendering_of_a_secret_free_template_stays_plain() {
        let rendered = Templater::new()
            .render_sensitive("{{ vars.name }}", &ctx())
            .unwrap();
        assert!(matches!(rendered, crate::secrets::Rendered::Plain(ref s) if s == "nomad"));
    }

    #[test]
    fn undefined_variables_are_errors() {
        let err = Templater::new()
            .render_plain("{{ vars.nope }}", &ctx())
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        assert!(
            Templater::new()
                .render_plain("{{ secrets.nope }}", &ctx())
                .is_err()
        );
    }

    #[test]
    fn needs_render_detects_template_syntax() {
        assert!(Templater::needs_render("a {{ b }}"));
        assert!(Templater::needs_render("{% if x %}y{% endif %}"));
        assert!(!Templater::needs_render("/etc/plain/path"));
    }

    #[test]
    fn referenced_paths_lists_dotted_reads() {
        let t = Templater::new();
        let paths = t
            .referenced_paths("{{ vars.a }}-{{ host.facts.arch }}{% if vars.b.c %}x{% endif %}")
            .unwrap();
        assert_eq!(paths, vec!["host.facts.arch", "vars.a", "vars.b.c"]);
        assert!(t.check("{{ unclosed").is_err());
        assert!(t.check("{{ fine }}").is_ok());
    }

    #[test]
    fn includes_resolve_registered_templates() {
        let mut t = Templater::new();
        t.add_template("nomad/partial.j2", "part={{ vars.name }}")
            .unwrap();
        let out = t
            .render_plain("{% include 'nomad/partial.j2' %}!", &ctx())
            .unwrap();
        assert_eq!(out, "part=nomad!");
    }
}
