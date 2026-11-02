//! Revision validation: everything that can be checked without a host (names,
//! graph, kinds, handler references, secret declarations, template syntax) and,
//! given a facts fixture, everything up to planning (vars, selection, spec
//! rendering, resource keys).

use super::{Revision, graph::Graph};
use crate::facts::Facts;
use crate::model::TaskDecl;
use crate::selectors::HostIdentity;
use crate::template::Templater;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

/// One problem found while validating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// The module the issue belongs to, when it is module-scoped.
    #[serde(default)]
    pub module: Option<String>,
    /// Where in the configuration: `buckle.yaml vars[1]`, `modules/nomad/module.yaml tasks[3]`, …
    pub location: String,
    pub message: String,
}

impl std::fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.location, self.message)
    }
}

/// Everything found while validating a revision. Errors make the revision
/// unusable; warnings are worth showing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationReport {
    #[serde(default)]
    pub errors: Vec<ValidationIssue>,
    #[serde(default)]
    pub warnings: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    pub fn error(
        &mut self,
        module: Option<&str>,
        location: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.errors.push(ValidationIssue {
            module: module.map(str::to_owned),
            location: location.into(),
            message: message.into(),
        });
    }

    pub fn warning(
        &mut self,
        module: Option<&str>,
        location: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.warnings.push(ValidationIssue {
            module: module.map(str::to_owned),
            location: location.into(),
            message: message.into(),
        });
    }

    pub fn merge(&mut self, other: ValidationReport) {
        self.errors.extend(other.errors);
        self.warnings.extend(other.warnings);
    }

    /// `Ok(self)` when there are no errors, else a user error listing them.
    pub fn into_result(self) -> crate::Result<ValidationReport> {
        if self.is_ok() {
            return Ok(self);
        }
        let list = self
            .errors
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n");
        Err(crate::error::user(
            format!(
                "The configuration has {} validation error{}:\n{list}",
                self.errors.len(),
                if self.errors.len() == 1 { "" } else { "s" }
            ),
            &["Fix the listed problems and run `buckle validate` again."],
        ))
    }
}

impl std::fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for e in &self.errors {
            writeln!(f, "error: {e}")?;
        }
        for w in &self.warnings {
            writeln!(f, "warning: {w}")?;
        }
        if self.errors.is_empty() && self.warnings.is_empty() {
            writeln!(f, "ok: no problems found")?;
        }
        Ok(())
    }
}

static SECRET_REF: LazyLock<regex::Regex> = LazyLock::new(|| {
    // `secrets` must start a word (not `mysecrets.x`, not `host.secrets.x`);
    // the regex crate has no look-behind, so the boundary is matched as a
    // non-capturing prefix.
    regex::Regex::new(
        r#"(?:^|[^A-Za-z0-9_.])secrets\s*(?:\.\s*([A-Za-z_][A-Za-z0-9_]*)|\[\s*(?:"([^"]+)"|'([^']+)')\s*\])"#,
    )
    .expect("static regex compiles")
});

/// The secret names referenced by a piece of text: `secrets.NAME` (with or
/// without `.present` / `.fingerprint`) and the subscript forms
/// `secrets["NAME"]` / `secrets['NAME']`. `secrets` must begin a word, so
/// `mysecrets.x` and `host.secrets.x` are not references.
pub fn secret_references(text: &str) -> BTreeSet<String> {
    SECRET_REF
        .captures_iter(text)
        .filter_map(|c| {
            c.get(1)
                .or_else(|| c.get(2))
                .or_else(|| c.get(3))
                .map(|m| m.as_str().to_owned())
        })
        .collect()
}

fn module_file(module: &str) -> String {
    format!("modules/{module}/module.yaml")
}

/// Collects every string scalar in a YAML value.
fn yaml_strings<'a>(value: &'a serde_yaml::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_yaml::Value::String(s) => out.push(s),
        serde_yaml::Value::Sequence(items) => items.iter().for_each(|v| yaml_strings(v, out)),
        serde_yaml::Value::Mapping(map) => map.iter().for_each(|(k, v)| {
            yaml_strings(k, out);
            yaml_strings(v, out);
        }),
        serde_yaml::Value::Tagged(t) => yaml_strings(&t.value, out),
        _ => {}
    }
}

/// Static validation of a revision (design §03, §04, §05, §06): unique module
/// names, `needs` resolve and form a DAG, task and health kinds exist, handler
/// references resolve, every referenced secret is declared, templates compile.
pub fn validate(revision: &Revision) -> crate::Result<ValidationReport> {
    let mut report = ValidationReport::default();
    let templater = Templater::new();
    let declared_secrets: BTreeSet<&str> =
        revision.fleet.secrets.keys().map(String::as_str).collect();

    let check_secrets = |report: &mut ValidationReport,
                         module: Option<&str>,
                         location: &str,
                         text: &str| {
        for name in secret_references(text) {
            if !declared_secrets.contains(name.as_str()) {
                report.error(
                    module,
                    location,
                    format!("references the secret '{name}', which buckle.yaml does not declare under `secrets`"),
                );
            }
        }
    };
    let check_template =
        |report: &mut ValidationReport, module: Option<&str>, location: &str, text: &str| {
            if Templater::needs_render(text)
                && let Err(e) = templater.check(text)
            {
                report.error(
                    module,
                    location,
                    format!("template does not compile: {}", e.message()),
                );
            }
        };

    // Fleet-level text.
    for (name, selector) in &revision.fleet.fleet.groups {
        check_secrets(
            &mut report,
            None,
            &format!("buckle.yaml fleet.groups.{name}"),
            selector.raw(),
        );
    }
    for (i, overlay) in revision.fleet.vars.iter().enumerate() {
        let location = format!("buckle.yaml vars[{i}]");
        if let Some(when) = &overlay.when {
            check_secrets(&mut report, None, &location, when.raw());
        }
        if overlay.file.is_none() && overlay.values.is_empty() {
            report.warning(
                None,
                &location,
                "overlay has neither `file` nor `values` and does nothing",
            );
        }
        for (k, v) in &overlay.values {
            let mut strings = Vec::new();
            yaml_strings(v, &mut strings);
            for s in strings {
                let loc = format!("{location}.values.{k}");
                check_secrets(&mut report, None, &loc, s);
                check_template(&mut report, None, &loc, s);
            }
        }
    }
    for (path, content) in &revision.var_files {
        let mut strings = Vec::new();
        yaml_strings(content, &mut strings);
        let location = path.display().to_string();
        for s in strings {
            check_secrets(&mut report, None, &location, s);
            check_template(&mut report, None, &location, s);
        }
    }

    for name in &revision.unloaded {
        report.warning(
            Some(name),
            format!("modules/{name}"),
            "the directory has no module.yaml: nothing is planned for it, and resources Buckle recorded for a module of this name are kept rather than removed",
        );
    }

    // Module names and graph.
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for m in &revision.modules {
        *seen.entry(m.config.name.as_str()).or_default() += 1;
    }
    for (name, count) in &seen {
        if *count > 1 {
            report.error(
                Some(name),
                module_file(name),
                format!(
                    "the module name '{name}' is used {count} times; module names must be unique"
                ),
            );
        }
    }
    let mut graph = Graph::new();
    for m in &revision.modules {
        graph.add(&m.config.name, &m.config.needs);
        for need in &m.config.needs {
            if !seen.contains_key(need.as_str()) {
                report.error(
                    Some(&m.config.name),
                    module_file(&m.config.name),
                    format!("needs the module '{need}', which does not exist"),
                );
            }
            if need == &m.config.name {
                report.error(
                    Some(&m.config.name),
                    module_file(&m.config.name),
                    "a module cannot need itself",
                );
            }
        }
    }
    if let Some(cycle) = graph.find_cycle() {
        report.error(
            None,
            "modules",
            format!("the module `needs` form a cycle: {}", cycle.join(" -> ")),
        );
    }

    // Per module: kinds, handlers, secrets, templates.
    let handler_names: HashMap<&str, BTreeSet<&str>> = revision
        .modules
        .iter()
        .map(|m| {
            (
                m.config.name.as_str(),
                m.config.handlers.keys().map(String::as_str).collect(),
            )
        })
        .collect();
    for m in &revision.modules {
        let name = m.config.name.as_str();
        let file = module_file(name);
        if let Some(when) = &m.config.when {
            check_secrets(&mut report, Some(name), &format!("{file} when"), when.raw());
        }
        for (k, v) in &m.config.vars {
            let mut strings = Vec::new();
            yaml_strings(v, &mut strings);
            for s in strings {
                let loc = format!("{file} vars.{k}");
                check_secrets(&mut report, Some(name), &loc, s);
                check_template(&mut report, Some(name), &loc, s);
            }
        }
        for provider in &m.config.facts.provides {
            if !crate::facts::registry().contains_key(provider.as_str()) {
                report.warning(
                    Some(name),
                    format!("{file} facts.provides"),
                    format!("the fact provider '{provider}' is not built into this Buckle"),
                );
            }
        }
        let check_decl = |report: &mut ValidationReport, decl: &TaskDecl, location: &str| {
            if let Some(problem) = after_unsupported(decl) {
                report.error(Some(name), location, problem);
            }
            if crate::task::registry().get(decl.kind.as_str()).is_none() {
                report.error(
                    Some(name),
                    location,
                    format!("'{}' is not a task kind this Buckle knows about", decl.kind),
                );
            }
            let mut strings = Vec::new();
            yaml_strings(&decl.spec, &mut strings);
            for s in strings {
                check_secrets(report, Some(name), location, s);
                check_template(report, Some(name), location, s);
            }
            if let Some(when) = &decl.when {
                check_secrets(report, Some(name), location, when.raw());
            }
            for notify in &decl.notify {
                let (target_module, handler) = match notify.split_once(':') {
                    Some((m, h)) => (m, h),
                    None => (name, notify.as_str()),
                };
                match handler_names.get(target_module) {
                    None => report.error(Some(name), location, format!("notifies '{notify}', but there is no module '{target_module}'")),
                    Some(handlers) if !handlers.contains(handler) => report.error(
                        Some(name),
                        location,
                        format!("notifies '{notify}', but module '{target_module}' declares no handler '{handler}'"),
                    ),
                    Some(_) => {}
                }
            }
        };
        for (i, decl) in m.config.tasks.iter().enumerate() {
            let location = format!("{file} tasks[{i}] ({})", decl.kind);
            check_decl(&mut report, decl, &location);
            if let Some(problem) = command_run_without_id(decl) {
                report.error(Some(name), &location, problem);
            }
        }
        for (hname, decl) in &m.config.handlers {
            check_decl(
                &mut report,
                decl,
                &format!("{file} handlers.{hname} ({})", decl.kind),
            );
        }
        for (i, h) in m.config.health.iter().enumerate() {
            let location = format!("{file} health[{i}] ({})", h.display_name());
            if crate::health::registry()
                .get(h.check.kind.as_str())
                .is_none()
            {
                report.error(
                    Some(name),
                    &location,
                    format!(
                        "'{}' is not a health check kind this Buckle knows about",
                        h.check.kind
                    ),
                );
            }
            let mut strings = Vec::new();
            yaml_strings(&h.check.spec, &mut strings);
            for s in strings {
                check_secrets(&mut report, Some(name), &location, s);
                check_template(&mut report, Some(name), &location, s);
            }
        }
        for (path, source) in &m.templates {
            let location = format!("modules/{name}/files/{}", path.display());
            check_secrets(&mut report, Some(name), &location, source);
            if let Err(e) = templater.check(source) {
                report.error(
                    Some(name),
                    &location,
                    format!("template does not compile: {}", e.message()),
                );
            }
        }
    }

    Ok(report)
}

/// What a dry planning pass decided for one host (`buckle validate --facts`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSummary {
    pub host: String,
    /// Fleet groups the host is a member of.
    pub groups: Vec<String>,
    /// Modules selected, in dependency order.
    pub selected: Vec<String>,
    pub skipped: Vec<String>,
    /// Modules whose `when` reads facts a selected module provides: decided
    /// on the host after that module applies.
    #[serde(default)]
    pub conditional: Vec<String>,
    /// Modules whose selection cannot be decided from these facts.
    #[serde(default)]
    pub indeterminate: Vec<crate::plan::Indeterminate>,
}

/// Static validation plus a dry planning pass for one host described by
/// `host` and `facts`: groups, vars, module selection, spec rendering, spec
/// parsing and resource keys (duplicates are errors). No task `plan` runs and
/// the host is never inspected, so this is safe on any machine.
pub async fn validate_with_facts(
    revision: &Revision,
    host: &HostIdentity,
    facts: &Facts,
) -> crate::Result<ValidationReport> {
    Ok(validate_host(revision, host, facts).await?.0)
}

/// [`validate_with_facts`], also returning what was decided for the host
/// (`None` when static validation already failed). Module-scoped problems are
/// reported per module; an indeterminate module is a warning.
pub async fn validate_host(
    revision: &Revision,
    host: &HostIdentity,
    facts: &Facts,
) -> crate::Result<(ValidationReport, Option<HostSummary>)> {
    let mut report = validate(revision)?;
    if !report.is_ok() {
        return Ok((report, None));
    }
    let location = format!("host {}", host.id);
    match super::planner::dry_run(revision, host, facts).await {
        Ok(dry) => {
            for (module, message, is_error) in &dry.problems {
                if *is_error {
                    report.error(
                        Some(module),
                        &location,
                        format!("module '{module}': {message}"),
                    );
                } else {
                    report.warning(
                        Some(module),
                        &location,
                        format!("module '{module}': {message}"),
                    );
                }
            }
            for i in &dry.summary.indeterminate {
                report.warning(
                    Some(&i.module),
                    &location,
                    format!("module '{}' is indeterminate: {}", i.module, i.reason),
                );
            }
            Ok((report, Some(dry.summary)))
        }
        Err(err) => {
            report.error(None, location, err.message());
            Ok((report, None))
        }
    }
}

/// `after:` is parsed but not implemented: tasks of a module run strictly in
/// order. Rather than silently ignoring it, it is rejected.
pub fn after_unsupported(decl: &TaskDecl) -> Option<String> {
    decl.after.as_ref().map(|_| {
        "sets `after:`, which this Buckle does not support: the tasks of a module run one at a time in the order they are written; remove `after:` and order the tasks instead".to_owned()
    })
}

/// `command.run` is keyed by its rendered command line, so editing the
/// command (or a var inside it) turns the old resource into an orphan and runs
/// its `remove:`. A task with `remove:` must therefore carry an explicit `id`.
pub fn command_run_without_id(decl: &TaskDecl) -> Option<String> {
    let has_remove = decl
        .spec
        .as_mapping()
        .is_some_and(|m| m.contains_key(serde_yaml::Value::String("remove".into())));
    (decl.kind == "command.run" && has_remove && decl.id.is_none()).then(|| {
        "sets `remove:` but no `id`: a command.run is identified by its rendered command, so editing the command (or a var inside it) would run the old `remove:`; give the task a stable `id`".to_owned()
    })
}

/// Looks up the kind for `decl`, as a validation-friendly error.
pub(super) fn kind_of(decl: &TaskDecl) -> crate::Result<&'static dyn crate::task::DynTask> {
    crate::task::lookup(&decl.kind)
}
