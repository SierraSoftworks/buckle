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
use std::collections::BTreeSet;
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
        // A marking render (see [`Emitted`]) brackets every expression's
        // output so the text an expression emitted can be told from the
        // template's literal text afterwards.
        env.set_formatter(|out, state, value| {
            let marking = MARKING.with(|m| m.get());
            if marking {
                out.write_str(MARK_OPEN)?;
            }
            minijinja::escape_formatter(out, state, value)?;
            if marking {
                out.write_str(MARK_CLOSE)?;
            }
            Ok(())
        });
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
        // Expression output is traced for display only; a render that
        // reveals secrets is never traced.
        let trace = ctx.emitted.as_ref().filter(|_| mode != RenderMode::Real);
        let touched = view.touched.clone();
        let missing = view.missing.clone();
        // The JSON context (host, vars, module, task) plus the secrets object.
        let mut map: std::collections::BTreeMap<String, Value> = match ctx.template_json() {
            serde_json::Value::Object(obj) => obj
                .into_iter()
                .map(|(k, v)| (k, Value::from(minijinja::value::Serde(v))))
                .collect(),
            _ => Default::default(),
        };
        map.insert("secrets".into(), Value::from_object(view));
        let context = Value::from(minijinja::value::Serde(map));
        let rendered = self.env.render_str(source, context.clone());
        if let (Ok(plain), Some(trace)) = (&rendered, trace) {
            // The marking render never produces the text that is used: it
            // only says where in `plain` the expressions' output lies.
            trace.observe(plain, ctx, || {
                let _marking = MarkScope::open();
                self.env.render_str(source, context).ok()
            });
        }
        rendered.map_err(|e| {
                // minijinja's error Debug dumps the referenced variables, which
                // would include secret values: scrub through the secrets module.
                let err = crate::secrets::redact::template_error(e, &ctx.secrets);
                let missing = missing.lock().map(|m| m.clone()).unwrap_or_default();
                if !missing.is_empty() {
                    let names = missing
                        .iter()
                        .map(|n| format!("'{n}'"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let held: Vec<&str> = ctx.secrets.names().collect();
                    crate::error::wrap_user(
                        err,
                        format!(
                            "The template reads the secret {names}, which this host does not hold{}.",
                            if held.is_empty() {
                                " (it holds no secrets)".to_owned()
                            } else {
                                format!(" (it holds {})", held.join(", "))
                            }
                        ),
                        &[
                            "Declare the secret under `secrets:` in buckle.yaml and check that the server (or, in local mode, this host) can resolve its value.",
                            "In server mode a host receives only the secrets the modules selected for it reference; if the server's view of this host's facts is stale, the agent fetches the missing ones before planning.",
                        ],
                    )
                } else if mode == RenderMode::Plain && touched.load(std::sync::atomic::Ordering::Relaxed) {
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

/// Opens a fact placeholder: `«fact:host.facts.os.codename»`.
pub const FACT_TOKEN_PREFIX: &str = "\u{ab}fact:";
const TOKEN_OPEN: char = '\u{ab}';
const TOKEN_CLOSE: char = '\u{bb}';

/// What templates rendered with a traced context
/// ([`HostContext::with_emitted`]) emitted through `{{ … }}` expressions,
/// as opposed to their literal text, and the host values found there.
///
/// A traced render is rendered a second time with every expression's
/// output bracketed by markers (text that is never used itself), which says
/// where in the rendered text the expressions' output lies. The host's
/// values (`host.id`, `host.facts.net.fqdn`, …) found inside those spans
/// become `«fact:PATH»` placeholders in a display copy of the text, so the
/// fleet view shows a placeholder exactly where a template put a host value
/// and nowhere else: literal text that happens to equal one stays text.
#[derive(Debug, Default)]
pub struct Emitted {
    trace: std::sync::Mutex<Trace>,
    /// The host's `(value, path)` leaves worth a placeholder, longest value
    /// first (`seer.lan` is taken whole before `seer`) and, among equal
    /// values, the shortest path (`host.id` over `host.facts.os.hostname`).
    leaves: std::sync::OnceLock<Vec<(String, String)>>,
}

#[derive(Debug, Default)]
struct Trace {
    /// Renders in which an expression emitted a host value: the text as
    /// rendered, and the same text with those values as placeholders.
    renders: Vec<(String, String)>,
    /// The paths placed, across renders.
    paths: BTreeSet<String>,
}

impl Emitted {
    /// Records what the expressions of a traced render emitted. `plain` is
    /// the render's output; `marked` renders it again with every
    /// expression's output bracketed, and is only asked for when `plain`
    /// holds a host value at all.
    fn observe(&self, plain: &str, ctx: &HostContext, marked: impl FnOnce() -> Option<String>) {
        let leaves = self.leaves.get_or_init(|| placeholder_leaves(ctx));
        if !leaves.iter().any(|(v, _)| plain.contains(v.as_str())) {
            return;
        }
        let Some(marked) = marked() else {
            return;
        };
        let (stripped, spans) = unmark(&marked);
        // A marked render that reads differently (a template that measures
        // or compares its own output) says nothing reliable about `plain`.
        if stripped != plain {
            return;
        }
        let mut paths = BTreeSet::new();
        let mut display = String::with_capacity(plain.len());
        let mut at = 0;
        for (start, end) in spans {
            display.push_str(&plain[at..start]);
            display.push_str(&placeholders(&plain[start..end], leaves, &mut paths));
            at = end;
        }
        display.push_str(&plain[at..]);
        if paths.is_empty() {
            return;
        }
        if let Ok(mut trace) = self.trace.lock() {
            trace.renders.push((plain.to_owned(), display));
            trace.paths.extend(paths);
        }
    }

    /// The paths (`host.facts.net.fqdn`) of the host values the traced
    /// renders' expressions emitted, in path order.
    pub fn paths(&self) -> Vec<String> {
        self.trace
            .lock()
            .map(|t| t.paths.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// `planned`'s diff text (the preview of a creation, the unified diff of
    /// an update) with the host values its templates emitted shown as
    /// placeholders: a line of the planned text is shown as the traced
    /// render that holds that same line, at that line number, displays it.
    /// `None` when no line of the text came from such an expression.
    pub fn display(&self, planned: &crate::plan::Planned) -> Option<String> {
        use crate::plan::{Diff, Planned};
        let trace = self.trace.lock().ok()?;
        if trace.renders.is_empty() {
            return None;
        }
        let renders: Vec<Lines<'_>> = trace
            .renders
            .iter()
            .map(|(plain, display)| Lines::of(plain, display))
            .collect();
        match planned {
            Planned::Change {
                diff:
                    Diff::Create {
                        preview: Some(preview),
                        ..
                    },
            } => shown(&[preview_lines(preview.get())], &renders),
            Planned::Change {
                diff:
                    Diff::Update {
                        unified: Some(unified),
                        ..
                    },
            } => shown(&unified_hunks(unified.get()), &renders),
            _ => None,
        }
    }
}

/// Emitted text shorter than this cannot hold a value worth a placeholder.
const MIN_EMITTED_LEN: usize = 3;

/// The host's leaves that may become placeholders (see [`Emitted::leaves`]).
/// A value with a line break is left out, so a display text keeps the line
/// structure of the text it stands for.
fn placeholder_leaves(ctx: &HostContext) -> Vec<(String, String)> {
    let mut leaves: Vec<(String, String)> = ctx
        .host_leaves()
        .into_iter()
        .filter(|(_, v)| v.len() >= MIN_EMITTED_LEN && !v.contains(['\n', '\r']))
        .map(|(path, value)| (value, path))
        .collect();
    leaves.sort_by(|a, b| {
        b.0.len()
            .cmp(&a.0.len())
            .then(a.1.len().cmp(&b.1.len()))
            .then(a.1.cmp(&b.1))
    });
    leaves
}

/// `text`, an expression's output, with every host value in it that stands
/// as a word of its own (the text's edges count as word boundaries, an
/// existing `«…»` token is kept whole) replaced by its placeholder.
fn placeholders(text: &str, leaves: &[(String, String)], paths: &mut BTreeSet<String>) -> String {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut out = String::with_capacity(text.len());
    let mut prev: Option<char> = None;
    let mut i = 0;
    'scan: while i < text.len() {
        let rest = &text[i..];
        if rest.starts_with(TOKEN_OPEN)
            && let Some(end) = rest.find(TOKEN_CLOSE)
        {
            let token = &rest[..end + TOKEN_CLOSE.len_utf8()];
            out.push_str(token);
            prev = Some(TOKEN_CLOSE);
            i += token.len();
            continue;
        }
        if !word(prev) {
            for (value, path) in leaves {
                if rest.starts_with(value.as_str()) && !word(rest[value.len()..].chars().next()) {
                    out.push_str(FACT_TOKEN_PREFIX);
                    out.push_str(path);
                    out.push(TOKEN_CLOSE);
                    paths.insert(path.clone());
                    prev = value.chars().last();
                    i += value.len();
                    continue 'scan;
                }
            }
        }
        let c = rest.chars().next().expect("i is within text");
        out.push(c);
        prev = Some(c);
        i += c.len_utf8();
    }
    out
}

/// Brackets for expression output in a marking render: noncharacters, which
/// no text holds on purpose.
const MARK_OPEN: &str = "\u{fdd0}";
const MARK_CLOSE: &str = "\u{fdd1}";

/// A marked render without its markers, with the spans (byte ranges of the
/// returned text) the expressions emitted. Output emitted within another
/// expression's output (a macro's body, a captured block) is one span.
fn unmark(marked: &str) -> (String, Vec<(usize, usize)>) {
    let mut text = String::with_capacity(marked.len());
    let mut spans = Vec::new();
    let (mut depth, mut start) = (0usize, 0usize);
    for c in marked.chars() {
        if MARK_OPEN.starts_with(c) {
            if depth == 0 {
                start = text.len();
            }
            depth += 1;
        } else if MARK_CLOSE.starts_with(c) {
            if depth > 0 {
                depth -= 1;
                if depth == 0 {
                    spans.push((start, text.len()));
                }
            }
        } else {
            text.push(c);
        }
    }
    if depth > 0 {
        spans.push((start, text.len()));
    }
    (text, spans)
}

thread_local! {
    /// Whether the render running on this thread marks expression output.
    static MARKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks expression output for one render, restoring the enclosing state
/// when dropped.
struct MarkScope(bool);

impl MarkScope {
    fn open() -> Self {
        Self(MARKING.with(|m| m.replace(true)))
    }
}

impl Drop for MarkScope {
    fn drop(&mut self) {
        MARKING.with(|m| m.set(self.0));
    }
}

/// A traced render split into lines (line endings kept), as rendered and
/// as displayed; both have the same lines, a placeholder never standing for
/// a value with a line break.
struct Lines<'a> {
    plain: Vec<&'a str>,
    display: Vec<&'a str>,
}

impl<'a> Lines<'a> {
    fn of(plain: &'a str, display: &'a str) -> Self {
        Self {
            plain: plain.split_inclusive('\n').collect(),
            display: display.split_inclusive('\n').collect(),
        }
    }

    /// How line `n` (zero-based) is displayed, without its line ending, when
    /// the render's line reads `line` (a diff's last line may lack the
    /// ending the render has, or the other way round).
    fn shown(&self, n: usize, line: &str) -> Option<&'a str> {
        let plain = self.plain.get(n)?.trim_end_matches('\n');
        (plain == line.trim_end_matches('\n'))
            .then(|| self.display.get(n).map(|d| d.trim_end_matches('\n')))
            .flatten()
    }
}

/// A line of a diff text: as it stands, and for a line of the planned text
/// (after `prefix` bytes of diff marker) which line of the render it holds.
struct Line<'a> {
    text: &'a str,
    planned: Option<usize>,
    prefix: usize,
}

/// A creation's preview: the planned text itself, perhaps cut short.
fn preview_lines(text: &str) -> Vec<Line<'_>> {
    text.split_inclusive('\n')
        .enumerate()
        .map(|(n, text)| Line {
            text,
            planned: Some(n),
            prefix: 0,
        })
        .collect()
}

/// A unified diff's lines by hunk (the text between hunks forming groups
/// of its own), each `+` and context line placed at its line number of the
/// planned text by the hunk header. A line that does not read as part of
/// its hunk ends the hunk.
fn unified_hunks(text: &str) -> Vec<Vec<Line<'_>>> {
    let mut groups: Vec<Vec<Line<'_>>> = vec![Vec::new()];
    let (mut old_left, mut new_left, mut next) = (0usize, 0usize, 0usize);
    for text in text.split_inclusive('\n') {
        if old_left > 0 || new_left > 0 {
            let line = match text.as_bytes().first() {
                Some(b'+') if new_left > 0 => {
                    new_left -= 1;
                    next += 1;
                    Some(Line {
                        text,
                        planned: Some(next - 1),
                        prefix: 1,
                    })
                }
                Some(b' ') if new_left > 0 && old_left > 0 => {
                    new_left -= 1;
                    old_left -= 1;
                    next += 1;
                    Some(Line {
                        text,
                        planned: Some(next - 1),
                        prefix: 1,
                    })
                }
                Some(b'-') if old_left > 0 => {
                    old_left -= 1;
                    Some(Line {
                        text,
                        planned: None,
                        prefix: 0,
                    })
                }
                Some(b'\\') => Some(Line {
                    text,
                    planned: None,
                    prefix: 0,
                }),
                _ => None,
            };
            if let Some(line) = line {
                groups.last_mut().expect("one group at least").push(line);
                continue;
            }
            (old_left, new_left) = (0, 0);
        }
        if let Some((old, new, start)) = hunk_header(text) {
            (old_left, new_left, next) = (old, new, start);
            groups.push(Vec::new());
        }
        groups.last_mut().expect("one group at least").push(Line {
            text,
            planned: None,
            prefix: 0,
        });
    }
    groups
}

/// `(old lines, new lines, first new line)` of a `@@ -A,B +C,D @@` header;
/// the first new line counted from zero.
fn hunk_header(line: &str) -> Option<(usize, usize, usize)> {
    let body = line.strip_prefix("@@ -")?;
    let (old, rest) = body.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let range = |s: &str| -> Option<(usize, usize)> {
        Some(match s.split_once(',') {
            Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
            None => (s.parse().ok()?, 1),
        })
    };
    let (_, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Some((old_count, new_count, new_start.saturating_sub(1)))
}

/// `groups` as the renders display them: each group takes the render with
/// the most lines in common with it (at their line numbers), and every
/// planned line that render holds is shown its way. `None` when no line
/// changes.
fn shown(groups: &[Vec<Line<'_>>], renders: &[Lines<'_>]) -> Option<String> {
    let mut out = String::new();
    let mut changed = false;
    for group in groups {
        let common = |r: &Lines<'_>| {
            group
                .iter()
                .filter(|l| {
                    l.planned
                        .is_some_and(|n| r.shown(n, &l.text[l.prefix..]).is_some())
                })
                .count()
        };
        // The first of the renders with the most lines in common.
        let best = renders.iter().rev().max_by_key(|r| common(r));
        for line in group {
            let display = match (line.planned, best) {
                (Some(n), Some(r)) => r.shown(n, &line.text[line.prefix..]),
                _ => None,
            };
            match display {
                Some(d) => {
                    out.push_str(&line.text[..line.prefix]);
                    out.push_str(d);
                    if line.text.ends_with('\n') {
                        out.push('\n');
                    }
                    changed |= d != line.text[line.prefix..].trim_end_matches('\n');
                }
                None => out.push_str(line.text),
            }
        }
    }
    changed.then_some(out)
}

/// The `secrets` template variable.
#[derive(Debug)]
struct SecretsView {
    store: Arc<SecretStore>,
    mode: RenderMode,
    touched: Arc<std::sync::atomic::AtomicBool>,
    /// Names the template asked for that the store does not hold, so the
    /// resulting "undefined value" can be explained.
    missing: Arc<std::sync::Mutex<Vec<String>>>,
}

impl SecretsView {
    fn new(store: Arc<SecretStore>, mode: RenderMode) -> Self {
        Self {
            store,
            mode,
            touched: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            missing: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }
}

impl Object for SecretsView {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let name = key.as_str()?;
        let Some(secret) = self.store.get(name).cloned() else {
            if let Ok(mut missing) = self.missing.lock()
                && !missing.iter().any(|m| m == name)
            {
                missing.push(name.to_owned());
            }
            return None;
        };
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
    fn a_missing_secret_is_named_in_the_error() {
        let key = InstallKey::from_bytes([7u8; 32]);
        let mut store = SecretStore::new();
        store.insert(Secret::new("held", "s3cr3t-held".into(), &key));
        let ctx = HostContext::new(HostIdentity::default()).with_secrets(Arc::new(store));
        let err = Templater::new()
            .render_sensitive("{{ secrets.ABSENT }}", &ctx)
            .unwrap_err();
        let message = err.message();
        assert!(message.contains("'ABSENT'"), "{message}");
        assert!(message.contains("does not hold"), "{message}");
        assert!(message.contains("held"), "{message}");
        assert!(
            !crate::error::pretty(&err).to_string().contains("s3cr3t"),
            "values stay out of the error"
        );
        // A plain-mode render of an absent secret says the same, not "plain values only".
        let err = Templater::new()
            .render_plain("{{ secrets.ABSENT }}", &ctx)
            .unwrap_err();
        assert!(err.message().contains("'ABSENT'"), "{}", err.message());
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

    /// A host whose facts hold values that also appear as literal text in
    /// a rustic-style config (`global`, `/backup`).
    fn traced() -> (HostContext, Arc<Emitted>) {
        let emitted = Arc::new(Emitted::default());
        let ctx = HostContext::new(HostIdentity {
            id: "seer".into(),
            ..Default::default()
        })
        .with_facts(serde_json::json!({
            "net": {"fqdn": "seer.lan", "interfaces": [{"scope": "global"}]},
            "disks": [{"mount": "/backup"}],
        }))
        .with_emitted(emitted.clone());
        (ctx, emitted)
    }

    #[rstest]
    #[case::literal_text_is_not_traced("[global]\nrepo = \"/backup\"\n", &[])]
    #[case::expressions_are(
        "[global]\nrepo = \"/backup\"\nhost = \"{{ host.facts.net.fqdn }}\"\n",
        // `seer.lan` is taken whole, not as the host's id and `.lan`.
        &["host.facts.net.fqdn"]
    )]
    #[case::through_variables(
        "{% set d = host.id ~ '-data' %}path = {{ d }}",
        &["host.id"]
    )]
    #[case::through_includes("{% include 'part.j2' %}", &["host.facts.disks[0].mount"])]
    fn traced_renders_record_the_host_values_expressions_emit(
        #[case] source: &str,
        #[case] expected: &[&str],
    ) {
        let mut t = Templater::new();
        t.add_template("part.j2", "mount {{ host.facts.disks[0].mount }}")
            .unwrap();
        let (ctx, _) = traced();
        t.render_plain(source, &ctx).unwrap();
        assert_eq!(ctx.emitted_paths().unwrap(), expected);
    }

    /// `[global]`, `/backup` and the literal `seer` equal facts of the
    /// host; only the expression's `seer` is a placeholder, in a creation's
    /// preview and on the planned side of an update.
    #[test]
    fn display_puts_placeholders_only_where_expressions_emitted_host_values() {
        use crate::plan::{Diff, Planned};
        let t = Templater::new();
        let (ctx, emitted) = traced();
        let text = t
            .render_redacted(
                "[global]\nrepo = \"/backup\"\nhost = \"{{ host.id }}\"\nname = seer\n",
                &ctx,
            )
            .unwrap();
        assert_eq!(emitted.paths(), ["host.id"]);
        let created = Planned::change(Diff::Create {
            summary: "c".into(),
            preview: Some(text.clone()),
        });
        assert_eq!(
            emitted.display(&created).as_deref(),
            Some(
                "[global]\nrepo = \"/backup\"\nhost = \"\u{ab}fact:host.id\u{bb}\"\nname = seer\n"
            )
        );
        let old = Redacted::plain(
            "[global]\nrepo = \"/backup\"\nhost = \"old\"\nname = seer\n".to_owned(),
        );
        let updated = Planned::change(Diff::unified("u", &old, &text));
        let shown = emitted.display(&updated).unwrap();
        assert_eq!(
            shown,
            "--- current\n+++ planned\n@@ -1,4 +1,4 @@\n [global]\n repo = \"/backup\"\n-host = \"old\"\n+host = \"\u{ab}fact:host.id\u{bb}\"\n name = seer\n"
        );
        // A diff of something else (or no text at all) has no display.
        let other = Redacted::plain("seer\n".to_owned());
        assert_eq!(
            emitted.display(&Planned::change(Diff::unified("o", &old, &other))),
            None
        );
        assert_eq!(emitted.display(&Planned::NoOp), None);
    }

    /// Each hunk is shown by the render that holds its lines, so a diff
    /// over several rendered files (file.tree) reads right throughout.
    #[test]
    fn each_hunk_is_shown_by_the_render_that_holds_its_lines() {
        use crate::plan::{Diff, Planned};
        let t = Templater::new();
        let (ctx, emitted) = traced();
        let a = t
            .render_redacted("host = \"{{ host.id }}\"\n", &ctx)
            .unwrap();
        let b = t
            .render_redacted("y\nid = \"{{ host.id }}\"\n", &ctx)
            .unwrap();
        let unified = Redacted::plain(format!(
            "--- a\n+++ a\n@@ -1 +1 @@\n-x\n+{a}--- b\n+++ b\n@@ -1,2 +1,2 @@\n y\n-x\n+{}",
            b.get().lines().nth(1).unwrap()
        ));
        let planned = Planned::change(Diff::Update {
            summary: "u".into(),
            unified: Some(unified),
        });
        assert_eq!(
            emitted.display(&planned).as_deref(),
            Some(
                "--- a\n+++ a\n@@ -1 +1 @@\n-x\n+host = \"\u{ab}fact:host.id\u{bb}\"\n--- b\n+++ b\n@@ -1,2 +1,2 @@\n y\n-x\n+id = \"\u{ab}fact:host.id\u{bb}\""
            )
        );
    }

    /// A preview cut short keeps the lines it has, shown as rendered.
    #[test]
    fn a_truncated_preview_is_shown_as_far_as_it_goes() {
        use crate::plan::{Diff, Planned};
        let t = Templater::new();
        let (ctx, emitted) = traced();
        let text = t
            .render_redacted("host = \"{{ host.id }}\"\nx\n", &ctx)
            .unwrap();
        let cut = format!(
            "{}... (truncated, 99 bytes in total)\n",
            text.get().lines().next().unwrap().to_owned() + "\n"
        );
        let planned = Planned::change(Diff::Create {
            summary: "c".into(),
            preview: Some(Redacted::plain(cut)),
        });
        assert_eq!(
            emitted.display(&planned).as_deref(),
            Some("host = \"\u{ab}fact:host.id\u{bb}\"\n... (truncated, 99 bytes in total)\n")
        );
    }

    /// The marking render never produces the text that is used, and a
    /// template that measures its own output (where the markers would
    /// count) is left without a display rather than shown wrongly.
    #[test]
    fn a_template_that_measures_its_output_renders_right_and_gets_no_display() {
        let t = Templater::new();
        let (ctx, emitted) = traced();
        let out = t
            .render_plain(
                "{% set x %}{{ host.id }}{% endset %}{{ x | length }}-{{ host.id }}",
                &ctx,
            )
            .unwrap();
        assert_eq!(out, "4-seer");
        assert!(emitted.paths().is_empty());
        let plain = t.render_plain("{{ host.id }}", &ctx).unwrap();
        assert!(!plain.contains(MARK_OPEN) && !plain.contains(MARK_CLOSE));
    }

    #[rstest]
    #[case("@@ -1,3 +1,4 @@\n", Some((3, 4, 0)))]
    #[case("@@ -0,0 +1 @@\n", Some((0, 1, 0)))]
    #[case("@@ -5 +7,2 @@ fn x\n", Some((1, 2, 6)))]
    #[case("+++ planned\n", None)]
    fn hunk_headers_give_the_lines_of_each_side(
        #[case] line: &str,
        #[case] expected: Option<(usize, usize, usize)>,
    ) {
        assert_eq!(hunk_header(line), expected);
    }

    #[test]
    fn renders_that_reveal_secrets_or_are_untraced_record_nothing() {
        let t = Templater::new();
        let (ctx, emitted) = traced();
        t.render_sensitive("{{ host.facts.net.fqdn }}", &ctx)
            .unwrap();
        assert!(emitted.paths().is_empty());

        let untraced = HostContext {
            emitted: None,
            ..ctx
        };
        t.render_plain("{{ host.facts.net.fqdn }}", &untraced)
            .unwrap();
        assert_eq!(untraced.emitted_paths(), None);
        assert!(emitted.paths().is_empty());
    }
}
