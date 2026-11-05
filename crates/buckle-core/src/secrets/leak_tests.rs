//! Property-style leak tests: for a set of awkward secret values, no
//! formatting, serialisation, plan, span or error path may contain the
//! plaintext. Each test runs once per value (rstest `#[values]`).

use super::cache::SecretCache;
use super::sinks::{ApiSink, ProcessEnvSink, TransportSink, Witness};
use super::*;
use crate::plan::{Diff, ModulePlan, Plan, Planned, ResourceKey, RevisionId, Step, TaskRef};
use crate::selectors::{HostContext, HostIdentity, Vars};
use crate::system::Command;
use crate::template::Templater;
use rstest::rstest;
use std::sync::{Arc, Mutex};

/// The values every test runs against.
#[derive(Debug, Clone, Copy)]
enum Awkward {
    Empty,
    Simple,
    Newlines,
    TemplateSyntax,
    Unicode,
    VeryLong,
    Quotes,
    UrlChars,
}

impl Awkward {
    fn value(self) -> String {
        match self {
            Awkward::Empty => String::new(),
            Awkward::Simple => "hunter2-SENTINEL".into(),
            Awkward::Newlines => "line1-SENTINEL\nline2-SENTINEL\r\nlast".into(),
            Awkward::TemplateSyntax => "{{ not.a.template }}-SENTINEL {% raw %}".into(),
            Awkward::Unicode => "pässwörd-🔑-SENTINEL-日本".into(),
            Awkward::VeryLong => "x7Q-SENTINEL-".repeat(2000),
            Awkward::Quotes => "quote\"back\\slash'tab\t-SENTINEL".into(),
            Awkward::UrlChars => "p@ss word/+=?&%-SENTINEL".into(),
        }
    }
}

fn key() -> InstallKey {
    InstallKey::from_bytes([11u8; 32])
}

fn store_with(value: &str) -> SecretStore {
    let mut store = SecretStore::new();
    store.insert(Secret::new("s", value.to_owned(), &key()));
    store
}

fn ctx_with(store: SecretStore) -> HostContext {
    let mut vars = Vars::new();
    vars.insert("name".into(), "nomad".into());
    HostContext::new(HostIdentity {
        id: "pi-1".into(),
        ..Default::default()
    })
    .with_vars(vars)
    .with_secrets(Arc::new(store))
    .in_module("nomad", &[])
    .in_task("file.template", "/etc/nomad.d/nomad.hcl", &[])
}

/// The forms in which a leaked value could appear in formatted output.
fn leak_forms(value: &str) -> Vec<String> {
    if value.is_empty() {
        return Vec::new();
    }
    let debug = format!("{value:?}");
    let json = serde_json::to_string(value).expect("strings serialise");
    let mut forms = vec![
        value.to_owned(),
        debug[1..debug.len() - 1].to_owned(),
        json[1..json.len() - 1].to_owned(),
    ];
    // Line-by-line, in case a format splits multi-line values (YAML blocks, diffs).
    forms.extend(
        value
            .split(['\n', '\r'])
            .filter(|l| l.len() >= 8)
            .map(str::to_owned),
    );
    forms
}

#[track_caller]
fn assert_no_leak(haystack: &str, value: &str, what: &str) {
    for form in leak_forms(value) {
        assert!(
            !haystack.contains(&form),
            "{what} leaked the secret (form {:?}) in: {}",
            &form[..form.len().min(40)],
            &haystack[..haystack.len().min(400)]
        );
    }
}

// Every test below takes the full list of `Awkward` values with `#[values]`
// (rstest cannot read the list from a macro or constant).

#[rstest]
fn secret_formats_and_serialises_as_its_token(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let secret = Secret::new("s", value.clone(), &key());
    let token = secret.token();
    for (what, out) in [
        ("Debug", format!("{secret:?}")),
        ("Display", format!("{secret}")),
        ("alternate Debug", format!("{secret:#?}")),
        ("json", serde_json::to_string(&secret).unwrap()),
        ("yaml", serde_yaml::to_string(&secret).unwrap()),
        ("filter Display", format!("{}", secret.as_filter_value())),
        ("filter Debug", format!("{:?}", secret.as_filter_value())),
    ] {
        assert_no_leak(&out, &value, what);
        if !what.starts_with("filter") {
            assert!(out.contains(&token), "{what}: {out}");
        }
    }
}

#[rstest]
fn sensitive_and_rendered_never_format_content(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let sensitive = Sensitive::new(value.clone());
    let bytes = Sensitive::new(value.clone().into_bytes());
    let rendered = Rendered::Sensitive(sensitive.clone());
    for (what, out) in [
        ("Debug", format!("{sensitive:?}")),
        ("Display", format!("{sensitive}")),
        ("bytes Debug", format!("{bytes:?}")),
        ("Rendered Debug", format!("{rendered:?}")),
        (
            "digest",
            format!("{:?} {}", sensitive.digest(), rendered.digest()),
        ),
    ] {
        assert_no_leak(&out, &value, what);
    }
    assert_eq!(sensitive.digest(), crate::Digest::of(value.as_bytes()));
    assert!(rendered.as_plain().is_none());
}

#[rstest]
fn secret_store_shows_names_and_fingerprints_only(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let store = store_with(&value);
    let fp = store.get("s").unwrap().fingerprint().as_hex();
    for (what, out) in [
        ("Debug", format!("{store:?}")),
        ("Display", format!("{store}")),
        ("json", serde_json::to_string(&store).unwrap()),
        ("yaml", serde_yaml::to_string(&store).unwrap()),
    ] {
        assert_no_leak(&out, &value, what);
        assert!(out.contains(&fp), "{what}: {out}");
    }
    let json: serde_json::Value = serde_json::to_value(&store).unwrap();
    assert_eq!(json, serde_json::json!({ "s": fp }));
}

#[rstest]
fn secret_field_formats_as_its_template(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let ctx = ctx_with(store_with(&value));
    let templater = Templater::new();
    let raw = "user:{{ secrets.s }}@db";
    let field: SecretField = serde_yaml::from_str(&format!("'{raw}'")).unwrap();
    for (what, out) in [
        ("Debug", format!("{field:?}")),
        ("Display", format!("{field}")),
        ("json", serde_json::to_string(&field).unwrap()),
        ("yaml", serde_yaml::to_string(&field).unwrap()),
        (
            "redacted",
            field.redacted(&templater, &ctx).unwrap().into_inner(),
        ),
        (
            "resolved Debug",
            format!("{:?}", field.resolve(&templater, &ctx).unwrap()),
        ),
    ] {
        assert_no_leak(&out, &value, what);
    }
    // serde round trip preserves the raw template, through JSON and YAML.
    let json_back: SecretField =
        serde_json::from_str(&serde_json::to_string(&field).unwrap()).unwrap();
    let yaml_back: SecretField =
        serde_yaml::from_str(&serde_yaml::to_string(&field).unwrap()).unwrap();
    assert_eq!(json_back.raw(), raw);
    assert_eq!(yaml_back, field);
    // And it resolves to the real value through the sensitive render.
    match field.resolve(&templater, &ctx).unwrap() {
        Rendered::Sensitive(s) => assert_eq!(s.expose(Witness::new()), &format!("user:{value}@db")),
        Rendered::Plain(_) => panic!("a secret-referencing field must resolve as sensitive"),
    }
}

#[rstest]
fn host_context_never_carries_values(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    use crate::Filterable;
    let value = v.value();
    let ctx = ctx_with(store_with(&value));
    for (what, out) in [
        ("template_json", ctx.template_json().to_string()),
        ("Debug", format!("{ctx:?}")),
        ("filter Display", format!("{}", ctx.get("secrets.s"))),
        ("filter Debug", format!("{:?}", ctx.get("secrets.s"))),
    ] {
        assert_no_leak(&out, &value, what);
    }
}

#[rstest]
fn plans_built_from_redacted_renders_hold_only_tokens(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let old_value = v.value();
    let new_value = format!("{old_value}-ROTATED");
    let templater = Templater::new();
    let template = "servers = [\"{{ secrets.s }}\"]\nname = \"{{ vars.name }}\"\n";
    let old = templater
        .render_redacted(template, &ctx_with(store_with(&old_value)))
        .unwrap();
    let new = templater
        .render_redacted(template, &ctx_with(store_with(&new_value)))
        .unwrap();
    let preview = Redacted::shadow_diff(&old, &new);
    let step = Step {
        task: TaskRef {
            module: "nomad".into(),
            id: "nomad.hcl".into(),
            kind: "file.template".into(),
            name: None,
        },
        resource: ResourceKey::new("nomad", "file.template", "/etc/nomad.d/nomad.hcl"),
        planned: Planned::change(Diff::unified("1 line changed", &old, &new)),
        locks: vec![],
        notifies: vec![],
        always_runs: false,
        removal: Default::default(),
        facts: None,
    };
    let module = ModulePlan {
        module: "nomad".into(),
        needs: vec![],
        steps: vec![step],
        handlers: vec![],
        provides_facts: vec![],
    };
    let plan = Plan::new(RevisionId::new("r1"), "pi-1", vec![module], vec![], vec![]);
    let json = serde_json::to_string(&plan).unwrap();
    for (what, out) in [
        ("plan json", json.clone()),
        ("plan yaml", serde_yaml::to_string(&plan).unwrap()),
        ("plan Debug", format!("{plan:?}")),
        ("shadow diff", preview.get().clone()),
    ] {
        assert_no_leak(&out, &old_value, what);
        assert_no_leak(&out, &new_value, what);
    }
    // A rotation shows as a fingerprint change on the line that uses it.
    assert!(preview.get().contains("-servers = [\"\u{ab}secret:s@"));
    assert!(preview.get().contains("+servers = [\"\u{ab}secret:s@"));
    assert!(json.contains("secret:s@"));
}

/// A layer that records every span attribute and event field into a buffer,
/// formatted with `Debug` the way any real exporter would see it.
struct CaptureLayer(Arc<Mutex<String>>);

struct FieldWriter<'a>(&'a mut String);

impl tracing::field::Visit for FieldWriter<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.push_str(&format!("{}={:?};", field.name(), value));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.push_str(&format!("{}={};", field.name(), value));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        _id: &tracing::span::Id,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut buf = self.0.lock().unwrap();
        attrs.record(&mut FieldWriter(&mut buf));
    }

    fn on_record(
        &self,
        _id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut buf = self.0.lock().unwrap();
        values.record(&mut FieldWriter(&mut buf));
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut buf = self.0.lock().unwrap();
        event.record(&mut FieldWriter(&mut buf));
    }
}

#[derive(Clone)]
struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[rstest]
fn tracing_spans_and_events_record_tokens(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    use tracing_subscriber::layer::SubscriberExt;
    let value = v.value();
    let store = store_with(&value);
    let secret = store.get("s").unwrap().clone();
    let field = SecretField::new("{{ secrets.s }}");
    let sensitive = Sensitive::new(value.clone());
    let mut command = Command::new("psql");
    ProcessEnvSink.inject(&mut command, "PGPASSWORD", &secret);

    let fields = Arc::new(Mutex::new(String::new()));
    let fmt_out = Arc::new(Mutex::new(Vec::new()));
    let writer = BufWriter(fmt_out.clone());
    let subscriber = tracing_subscriber::registry()
        .with(CaptureLayer(fields.clone()))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_span_events(tracing_subscriber::fmt::format::FmtSpan::FULL)
                .with_writer(move || writer.clone()),
        );
    tracing::subscriber::with_default(subscriber, || {
        let span = tracing::info_span!(
            "apply",
            secret = %secret,
            secret_dbg = ?secret,
            later = tracing::field::Empty
        );
        let _guard = span.enter();
        span.record("later", tracing::field::display(&secret));
        tracing::info!(secret = %secret, secret_dbg = ?secret, "using {} / {:?}", secret, secret);
        tracing::warn!(store = ?store, store_display = %store, field = ?field, field_display = %field);
        tracing::error!(sensitive = ?sensitive, sensitive_display = %sensitive, command = ?command);
    });
    let fields = fields.lock().unwrap().clone();
    let formatted = String::from_utf8(fmt_out.lock().unwrap().clone()).unwrap();
    assert!(fields.contains(&secret.token()), "{fields}");
    assert!(formatted.contains(&secret.token()), "{formatted}");
    assert!(formatted.contains("PGPASSWORD"), "{formatted}");
    assert_no_leak(&fields, &value, "span/event fields");
    assert_no_leak(&formatted, &value, "fmt subscriber");
}

#[rstest]
fn redact_known_scrubs_values_and_encodings_from_output(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    let value = v.value();
    let store = store_with(&value);
    let b64 = STANDARD.encode(&value);
    let b64url = URL_SAFE_NO_PAD.encode(&value);
    let url = percent_encoding::utf8_percent_encode(&value, percent_encoding::NON_ALPHANUMERIC)
        .to_string();
    let output = format!(
        "starting\nconnecting with {value} to db\nAuthorization: Bearer {b64}\nurl-safe={b64url}\nGET /?token={url}&x=1\ndone\n"
    );
    let scrubbed = redact_known(&output, &store);
    let bytes = super::redact::redact_known_bytes(output.as_bytes(), &store);
    assert_eq!(scrubbed.get(), bytes.get());
    if value.is_empty() {
        assert_eq!(scrubbed.get(), &output);
        return;
    }
    let out = scrubbed.get();
    assert_no_leak(out, &value, "scrubbed output");
    for encoded in [&b64, &b64url, &url] {
        assert!(!out.contains(encoded.as_str()), "encoding survived: {out}");
    }
    let token = store.get("s").unwrap().token();
    assert_eq!(out.matches(&token).count(), 4, "{out}");
    assert!(out.starts_with("starting\nconnecting with "));
    assert!(out.ends_with("&x=1\ndone\n"));
}

#[rstest]
fn command_debug_shows_injected_keys_only(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let store = store_with(&value);
    let ctx = ctx_with(store.clone());
    let templater = Templater::new();
    let mut command = Command::new("psql").arg("-c").arg("select 1");
    ProcessEnvSink.inject(&mut command, "PGPASSWORD", store.get("s").unwrap());
    SecretField::new("postgres://u:{{ secrets.s }}@db")
        .inject_env(&mut command, "DATABASE_URL", &templater, &ctx)
        .unwrap();
    assert!(command.sensitive_env);
    let dbg = format!("{command:?} {command:#?} {}", command.display_line());
    assert!(
        dbg.contains("PGPASSWORD") && dbg.contains("DATABASE_URL"),
        "{dbg}"
    );
    assert_no_leak(&dbg, &value, "Command Debug");
    assert_eq!(command.env["PGPASSWORD"], value);
}

#[rstest]
fn render_modes_keep_their_promises(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let store = store_with(&value);
    let token = store.get("s").unwrap().token();
    let ctx = ctx_with(store);
    let templater = Templater::new();
    let template = "password = {{ secrets.s }}\n";

    // Plain refuses, and nothing about the error carries the value.
    let err = templater.render_plain(template, &ctx).unwrap_err();
    assert!(err.is(crate::error::Kind::User));
    let shown = format!(
        "{} | {} | {err} | {}",
        err.message(),
        err.description(),
        crate::error::pretty(&err)
    );
    assert_no_leak(&shown, &value, "render_plain error");

    // Redacted substitutes the token.
    let redacted = templater.render_redacted(template, &ctx).unwrap();
    assert_eq!(redacted.get(), &format!("password = {token}\n"));
    assert_no_leak(redacted.get(), &value, "render_redacted");

    // Sensitive reveals, behind a witness.
    match templater.render_sensitive(template, &ctx).unwrap() {
        Rendered::Sensitive(s) => {
            assert_eq!(s.expose(Witness::new()), &format!("password = {value}\n"));
            assert_no_leak(&format!("{s:?} {s}"), &value, "Sensitive");
        }
        Rendered::Plain(_) => panic!("a secret-referencing template must render as sensitive"),
    }
}

#[rstest]
fn export_import_round_trips_under_the_same_key(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let mut store = store_with(&value);
    store.insert(Secret::new("other", "second-SENTINEL".into(), &key()));
    let wire = store.export(&TransportSink);
    assert_eq!(wire.len(), 2);
    assert_eq!(wire[0].0, "s");
    assert_eq!(wire[0].1, store.get("s").unwrap().fingerprint());
    assert_eq!(wire[0].2, value.as_bytes());

    let back =
        SecretStore::import(&key(), wire.iter().map(|(n, _, v)| (n.clone(), v.clone()))).unwrap();
    let kept = SecretStore::import_fingerprinted(wire.clone()).unwrap();
    for imported in [&back, &kept] {
        assert_eq!(
            imported.names().collect::<Vec<_>>(),
            store.names().collect::<Vec<_>>()
        );
        for secret in store.iter() {
            let other = imported.get(secret.name()).unwrap();
            assert_eq!(other.fingerprint(), secret.fingerprint());
            assert_eq!(other.reveal(Witness::new()), secret.reveal(Witness::new()));
        }
    }
    // Under a different key, import re-fingerprints; import_fingerprinted keeps the sender's.
    let foreign = InstallKey::from_bytes([12u8; 32]);
    let refp = SecretStore::import(
        &foreign,
        wire.iter().map(|(n, _, v)| (n.clone(), v.clone())),
    )
    .unwrap();
    assert_ne!(
        refp.get("s").unwrap().fingerprint(),
        store.get("s").unwrap().fingerprint()
    );
}

#[test]
fn import_rejects_non_utf8_values_as_system_errors() {
    let err = SecretStore::import(&key(), [("s".to_owned(), vec![0xff, 0xfe])]).unwrap_err();
    assert!(err.is(crate::error::Kind::System));
    assert!(err.message().contains("'s'"));
    let fp = Secret::new("s", "x".into(), &key()).fingerprint();
    assert!(SecretStore::import_fingerprinted([("s".to_owned(), fp, vec![0xc3])]).is_err());
}

#[rstest]
fn cache_round_trips_awkward_values(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let store = store_with(&value);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("secrets.cbor");
    SecretCache::save(&path, &store, &key(), &TransportSink).unwrap();
    let back = SecretCache::load(&path, &key()).unwrap().unwrap();
    let secret = back.get("s").unwrap();
    assert_eq!(secret.reveal(Witness::new()), value);
    assert_eq!(secret.fingerprint(), store.get("s").unwrap().fingerprint());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[rstest]
fn fingerprints_parse_from_their_display_form(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let fp = Secret::new("s", v.value(), &key()).fingerprint();
    assert_eq!(Fingerprint::parse(&fp.to_string()).unwrap(), fp);
    assert_eq!(fp.to_string().parse::<Fingerprint>().unwrap(), fp);
    assert_eq!(
        Fingerprint::parse(&fp.to_string().to_uppercase()).unwrap(),
        fp
    );
}

#[rstest]
#[case("")]
#[case("abc")]
#[case("3f9a00b")]
#[case("3f9a00b2a")]
#[case("zzzzzzzz")]
fn malformed_fingerprints_are_user_errors(#[case] input: &str) {
    let err = Fingerprint::parse(input).unwrap_err();
    assert!(err.is(crate::error::Kind::User), "{input}");
}

/// minijinja's error `Debug` dumps the referenced variables, which renders
/// secrets (revealing them in a real render, failing the formatter in a plain
/// one). `Templater::render_with_view` maps every render error through
/// `redact::template_error`, which this test guards.
#[rstest]
fn template_error_debug_never_shows_values(
    #[values(Awkward::Simple, Awkward::Newlines, Awkward::Unicode)] v: Awkward,
) {
    let value = v.value();
    let ctx = ctx_with(store_with(&value));
    let templater = Templater::new();
    let failing = "x = {{ secrets.s }} {{ vars.undefined_var }}\n";
    let errors = [
        templater.render_plain(failing, &ctx).unwrap_err(),
        templater.render_redacted(failing, &ctx).unwrap_err(),
        templater.render_sensitive(failing, &ctx).unwrap_err(),
    ];
    for err in errors {
        let shown = format!("{err:?} {err:#?} {err:#}");
        assert_no_leak(&shown, &value, "template error Debug");
    }
}

#[rstest]
fn api_sink_compares_and_fingerprints_without_showing_values(
    #[values(
        Awkward::Empty,
        Awkward::Simple,
        Awkward::Newlines,
        Awkward::TemplateSyntax,
        Awkward::Unicode,
        Awkward::VeryLong,
        Awkward::Quotes,
        Awkward::UrlChars
    )]
    v: Awkward,
) {
    let value = v.value();
    let ctx = ctx_with(store_with(&value));
    let desired = Templater::new()
        .render_sensitive("{{ secrets.s }}", &ctx)
        .unwrap();
    let live = ApiSink.receive(value.clone());
    assert!(ApiSink.matches(&desired, &live));
    assert!(!ApiSink.matches(&desired, &ApiSink.receive(format!("{value}x"))));

    let fp = ApiSink.fingerprint(&key(), &desired);
    assert_eq!(fp, ApiSink.fingerprint_live(&key(), &live));
    let body = ApiSink.json_body(serde_json::json!({"Path": "p"}), "Items", [("k", &desired)]);
    for (what, out) in [
        ("fingerprint", format!("{fp} {fp:?}")),
        ("live", format!("{live:?} {live}")),
        ("body", format!("{body:?} {body}")),
    ] {
        assert_no_leak(&out, &value, what);
    }
    let wire: serde_json::Value = serde_json::from_slice(ApiSink.open(&body)).unwrap();
    assert_eq!(wire["Items"]["k"], serde_json::Value::String(value));
}
