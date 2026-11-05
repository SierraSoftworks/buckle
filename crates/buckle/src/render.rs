//! Text rendering of plans, apply events, outcomes and health reports for the
//! terminal (design §11). Everything rendered here is already redacted by
//! construction; this module only lays it out.

use buckle_core::engine::{
    ApplyEvent, ApplyOutcome, ModuleOutcome, ModuleStatus, StepOutcome, StepState,
};
use buckle_core::error::ErrorReport;
use buckle_core::health::{HealthReport, HealthStatus};
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, Plan, Planned, Step};
use std::fmt::Write as _;

const RESOURCE_WIDTH: usize = 52;

/// Renders a plan as the tree `buckle plan` prints.
pub fn render_plan(plan: &Plan, revision: &str, facts: Option<&serde_json::Value>) -> String {
    let mut out = String::new();
    let _ = write!(out, "revision  {revision}      host  {}", plan.host);
    if let Some(summary) = facts.and_then(facts_summary) {
        let _ = write!(out, "      facts  {summary}");
    }
    out.push('\n');

    let selected: Vec<&str> = plan
        .modules
        .iter()
        .map(|m| m.module.as_str())
        .chain(plan.deferred.iter().map(|d| d.module.as_str()))
        .collect();
    let total = selected.len() + plan.skipped.len();
    let _ = writeln!(
        out,
        "selected  {}    ({} of {total} modules)",
        if selected.is_empty() {
            "(none)".to_owned()
        } else {
            selected.join(" ")
        },
        selected.len()
    );
    out.push('\n');

    for module in &plan.modules {
        let needs = if module.needs.is_empty() {
            String::new()
        } else {
            format!("    needs: {}", module.needs.join(", "))
        };
        let _ = writeln!(out, "{}{needs}", module.module);
        for step in &module.steps {
            render_step(&mut out, step);
        }
        for handler in &module.handlers {
            let name = if handler.module == module.module {
                handler.handler.clone()
            } else {
                format!("{}:{}", handler.module, handler.handler)
            };
            let _ = writeln!(
                out,
                "  ↻ {:<width$} {}",
                format!("handler {name}"),
                handler.summary,
                width = RESOURCE_WIDTH
            );
        }
        if module.steps.is_empty() && module.handlers.is_empty() {
            let _ = writeln!(out, "  (no tasks)");
        }
    }
    for deferred in &plan.deferred {
        let _ = writeln!(
            out,
            "{:<width$} deferred: {}",
            deferred.module,
            deferred.reason,
            width = RESOURCE_WIDTH + 4
        );
        // Planned ahead of its providers; applied only if it keeps this intent.
        if let Some(preview) = &deferred.preview {
            for step in &preview.steps {
                render_step(&mut out, step);
            }
        }
    }

    for i in &plan.indeterminate {
        if i.plan_error {
            let _ = writeln!(out, "{}  could not be planned", i.module);
            let error = i
                .error_detail
                .clone()
                .unwrap_or_else(|| ErrorReport::user(i.reason.clone()));
            render_error(&mut out, "    ", &error);
        } else {
            let _ = writeln!(out, "{}  indeterminate: {}", i.module, i.reason);
        }
    }

    if !plan.removals.is_empty() {
        out.push('\n');
        out.push_str("removals\n");
        for removal in &plan.removals {
            let mut details = vec![removal.summary.clone()];
            details.push(format!(
                "policy {}",
                match removal.policy {
                    RemovalPolicy::Cleanup => "cleanup",
                    RemovalPolicy::Leave => "leave",
                }
            ));
            if let Some(rev) = &removal.introduced_in {
                details.push(format!("introduced in {}", rev.short()));
            }
            if removal.caution {
                details.push("CAUTION".to_owned());
            }
            let _ = writeln!(
                out,
                "  − {:<width$} {}",
                removal.resource.to_string(),
                details.join(" · "),
                width = RESOURCE_WIDTH
            );
        }
    }

    // Indeterminate modules are shown above, structured.
    let other_warnings: Vec<&String> = plan
        .warnings
        .iter()
        .filter(|w| !w.starts_with("removal:"))
        .filter(|w| {
            !plan
                .indeterminate
                .iter()
                .any(|i| w.starts_with(&format!("module '{}' ", i.module)))
        })
        .collect();
    if !other_warnings.is_empty() {
        out.push('\n');
        out.push_str("warnings\n");
        for w in other_warnings {
            let _ = writeln!(out, "  ! {w}");
        }
    }

    let changed_modules = plan.modules.iter().filter(|m| m.changes() > 0).count();
    out.push('\n');
    let _ = writeln!(
        out,
        "summary  {changed_modules} modules changed · {} changes · {} removals · {} deferred · plan digest {}",
        plan.changes(),
        plan.removals.len(),
        plan.deferred.len(),
        plan.digest.short()
    );
    out
}

fn render_step(out: &mut String, step: &Step) {
    let resource = format!("{}[{}]", step.task.kind, step.resource.key);
    let (marker, summary) = match &step.planned {
        Planned::NoOp => ("✓", "no change".to_owned()),
        Planned::Change { diff } => (
            if diff.is_destructive() { "−" } else { "~" },
            diff.summary().to_owned(),
        ),
        Planned::Blocked { reason } => ("!", format!("blocked: {reason}")),
    };
    let mut annotations = Vec::new();
    if !step.locks.is_empty() {
        annotations.push(format!("lock: {}", step.locks.join(", ")));
    }
    if step.planned.is_change() && !step.notifies.is_empty() {
        annotations.push(format!("→ notify {}", step.notifies.join(", ")));
    }
    if step.always_runs {
        annotations.push("always runs".to_owned());
    }
    let annotations = if annotations.is_empty() {
        String::new()
    } else {
        format!("    {}", annotations.join("    "))
    };
    let _ = writeln!(
        out,
        "  {marker} {resource:<width$} {summary}{annotations}",
        width = RESOURCE_WIDTH
    );
    if let Planned::Change { diff } = &step.planned {
        match diff {
            Diff::Update {
                unified: Some(text),
                ..
            } => {
                for line in text
                    .get()
                    .lines()
                    .filter(|l| !l.starts_with("---") && !l.starts_with("+++"))
                {
                    let _ = writeln!(out, "      {line}");
                }
            }
            Diff::Create {
                preview: Some(text),
                ..
            } => {
                for line in text.get().lines() {
                    let _ = writeln!(out, "      + {line}");
                }
            }
            _ => {}
        }
    }
}

fn facts_summary(facts: &serde_json::Value) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(os) = facts.get("os") {
        let family = os.get("family").and_then(|v| v.as_str());
        let codename = os.get("codename").and_then(|v| v.as_str());
        match (family, codename) {
            (Some(f), Some(c)) => parts.push(format!("os={f}/{c}")),
            (Some(f), None) => parts.push(format!("os={f}")),
            _ => {}
        }
    }
    if let Some(arch) = facts
        .pointer("/hardware/arch")
        .or_else(|| facts.get("arch"))
        .and_then(|v| v.as_str())
    {
        parts.push(format!("arch={arch}"));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// The glyph of a step state in the run tree.
pub fn glyph(state: StepState) -> &'static str {
    match state {
        StepState::Changed => "✓",
        StepState::Unchanged => "·",
        StepState::Failed => "✗",
        StepState::Blocked => "!",
        StepState::RolledBack => "↺",
        StepState::NotRun => "−",
        StepState::Running => "»",
        StepState::Pending | StepState::Skipped => "○",
    }
}

fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// An error as an indented block: the message, then `caused by:` lines,
/// then `to fix:` bullets. Never the dumped `Display` of the whole error.
pub fn render_error(out: &mut String, indent: &str, error: &ErrorReport) {
    let _ = writeln!(out, "{indent}{}", error.message);
    for cause in &error.causes {
        let _ = writeln!(out, "{indent}caused by: {cause}");
    }
    if !error.advice.is_empty() {
        let _ = writeln!(out, "{indent}to fix:");
        for advice in &error.advice {
            let _ = writeln!(out, "{indent}  • {advice}");
        }
    }
}

/// What a step line says: what it did once it ran, else what was planned,
/// with the state spelled out where the glyph alone is ambiguous.
fn step_text(step: &StepOutcome) -> String {
    let did = step.result.clone().unwrap_or_else(|| step.summary.clone());
    match step.state {
        StepState::NotRun => format!("{}  (not run)", step.summary),
        StepState::RolledBack => format!("{did}  (rolled back)"),
        StepState::Skipped => format!("{did}  (skipped)"),
        StepState::Pending => format!("{}  (pending)", step.summary),
        StepState::Running => format!("{}  (running)", step.summary),
        StepState::Failed => step.summary.clone(),
        _ => did,
    }
}

fn render_step_outcome(out: &mut String, step: &StepOutcome, handler: bool) {
    let label = if handler {
        format!("handler {}", step.name.as_deref().unwrap_or(&step.id))
    } else {
        step.id.clone()
    };
    let took = match (step.state, step.duration_ms) {
        (StepState::Changed | StepState::Unchanged | StepState::Failed, Some(ms)) => {
            format!("  {}", duration(ms))
        }
        _ => String::new(),
    };
    let _ = writeln!(
        out,
        "  {} {label:<width$} {}{took}",
        glyph(step.state),
        step_text(step),
        width = RESOURCE_WIDTH
    );
    if let Some(error) = &step.error_detail {
        render_error(out, "      ", error);
    }
}

fn module_header(module: &ModuleOutcome) -> String {
    let mut parts = vec![status_word(module.status).to_owned()];
    if module.changes > 0 {
        parts.push(format!(
            "{} change{}",
            module.changes,
            if module.changes == 1 { "" } else { "s" }
        ));
    }
    if module.duration_ms > 0 {
        parts.push(duration(module.duration_ms));
    }
    format!("{}  {}", module.module, parts.join(" · "))
}

/// The outcome of an apply as a tree: each module with its state, each step
/// on one line with a glyph, its summary and duration, an error block under
/// whatever failed, the removals, and a closing summary of the counts.
pub fn render_outcome(outcome: &ApplyOutcome) -> String {
    let mut out = String::new();
    for module in &outcome.module_results {
        let _ = writeln!(out, "{}", module_header(module));
        for step in &module.steps {
            render_step_outcome(&mut out, step, false);
        }
        for handler in &module.handlers {
            render_step_outcome(&mut out, handler, true);
        }
        // A module error no step accounts for (health, blocked steps, a plan
        // error, a rollback), or the module's when it has no steps listed.
        let explained = module
            .steps
            .iter()
            .chain(&module.handlers)
            .any(|s| s.error_detail.is_some());
        match (&module.error_detail, &module.error) {
            (Some(error), _) if !explained => render_error(&mut out, "    ", error),
            (None, Some(line)) if !explained => {
                let _ = writeln!(out, "    {line}");
            }
            _ => {}
        }
    }
    if !outcome.removals_run.is_empty() {
        out.push('\n');
        out.push_str("removals\n");
        for removal in &outcome.removals_run {
            let note = match removal.state {
                StepState::NotRun => "  (not run)",
                StepState::Skipped => "  (skipped)",
                _ => "",
            };
            let _ = writeln!(
                out,
                "  {} {:<width$} {}{note}",
                glyph(removal.state),
                removal.resource,
                removal.summary,
                width = RESOURCE_WIDTH
            );
            if let Some(error) = &removal.error_detail {
                render_error(&mut out, "      ", error);
            }
        }
    }
    match (&outcome.error_detail, &outcome.error) {
        (Some(error), _) => {
            out.push('\n');
            out.push_str("error\n");
            render_error(&mut out, "  ", error);
        }
        // An older outcome: its error line, when no module explains it.
        (None, Some(line))
            if !outcome
                .module_results
                .iter()
                .any(|m| m.error.is_some() || m.error_detail.is_some()) =>
        {
            let _ = writeln!(out, "\nerror: {line}");
        }
        _ => {}
    }
    out.push('\n');
    out.push_str(&summary_line(outcome));
    out
}

fn count<T>(
    items: impl IntoIterator<Item = T>,
    key: impl Fn(&T) -> &'static str,
) -> Vec<(&'static str, usize)> {
    let mut counts: Vec<(&'static str, usize)> = Vec::new();
    for item in items {
        let k = key(&item);
        match counts.iter_mut().find(|(name, _)| *name == k) {
            Some((_, n)) => *n += 1,
            None => counts.push((k, 1)),
        }
    }
    counts
}

fn step_word(state: &StepState) -> &'static str {
    match state {
        StepState::Pending => "pending",
        StepState::Running => "running",
        StepState::Changed => "changed",
        StepState::Unchanged => "unchanged",
        StepState::Failed => "failed",
        StepState::Blocked => "blocked",
        StepState::Skipped => "skipped",
        StepState::NotRun => "not run",
        StepState::RolledBack => "rolled back",
    }
}

fn join_counts(counts: &[(&str, usize)]) -> String {
    counts
        .iter()
        .map(|(name, n)| format!("{n} {name}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The closing line of a run: how it ended and the module and step counts.
pub fn summary_line(outcome: &ApplyOutcome) -> String {
    let result = if outcome.succeeded {
        "succeeded"
    } else if outcome.failed_module.is_none()
        && outcome
            .module_results
            .iter()
            .any(|m| m.status == ModuleStatus::Blocked)
    {
        "NEEDS ATTENTION"
    } else {
        "FAILED"
    };
    let modules = count(&outcome.module_results, |m| status_word(m.status));
    let steps = count(
        outcome
            .module_results
            .iter()
            .flat_map(|m| m.steps.iter().chain(&m.handlers))
            .map(|s| s.state),
        step_word,
    );
    let mut parts = vec![result.to_owned()];
    if !modules.is_empty() {
        parts.push(format!("modules: {}", join_counts(&modules)));
    }
    if !steps.is_empty() {
        parts.push(format!("steps: {}", join_counts(&steps)));
    }
    parts.push(format!("{} removals", outcome.removals));
    parts.push(format!("{:.1}s", outcome.duration_ms as f64 / 1000.0));
    parts.push(format!("plan digest {}", outcome.plan_digest.short()));
    format!("result   {}\n", parts.join(" · "))
}

/// One line for an apply event, as `buckle apply` prints them while the run
/// goes: one line per step (when it ends), module start and end lines,
/// removals, health and facts. `None` for events that do not print a line
/// of their own (a step starting, a handler's bookkeeping).
pub fn render_event(event: &ApplyEvent) -> Option<String> {
    let step_line = |module: &str, id: &str, state: StepState, text: &str| {
        format!("  {} {module}/{id}  {text}", glyph(state))
    };
    Some(match event {
        ApplyEvent::ModuleStarted { module } => module.clone(),
        ApplyEvent::StepStarted { .. } | ApplyEvent::HandlerRun { .. } => return None,
        ApplyEvent::StepFinished {
            module,
            id,
            task_id,
            changed,
            summary,
            ..
        } => step_line(
            module,
            if id.is_empty() { task_id } else { id },
            if *changed {
                StepState::Changed
            } else {
                StepState::Unchanged
            },
            summary,
        ),
        ApplyEvent::StepFailed {
            module,
            id,
            task_id,
            error,
            ..
        } => step_line(
            module,
            if id.is_empty() { task_id } else { id },
            StepState::Failed,
            error,
        ),
        ApplyEvent::StepBlocked {
            module, id, reason, ..
        } => step_line(
            module,
            id,
            StepState::Blocked,
            &format!("blocked: {reason}"),
        ),
        ApplyEvent::StepSkipped {
            module, id, reason, ..
        } => step_line(
            module,
            id,
            StepState::Skipped,
            &format!("skipped: {reason}"),
        ),
        ApplyEvent::ModuleFinished { module, status } => {
            format!("{module}  {}", status_word(*status))
        }
        ApplyEvent::RemovalRun { resource, summary } => match summary.strip_prefix("failed: ") {
            Some(error) => format!("  {} {resource}  {error}", glyph(StepState::Failed)),
            None => format!("  {} {resource}  {summary}", glyph(StepState::Changed)),
        },
        ApplyEvent::HealthChecked {
            module,
            check,
            passed,
            reason,
        } => format!(
            "  {} health {module}/{check}{}",
            if *passed { "✓" } else { "✗" },
            reason
                .as_ref()
                .map(|r| format!("  {r}"))
                .unwrap_or_default()
        ),
        ApplyEvent::FactsRefreshed { providers } => {
            format!("  facts refreshed: {}", providers.join(", "))
        }
        ApplyEvent::StepProgress {
            module,
            id,
            message,
            ..
        } => format!("  … {module}/{id}: {message}"),
    })
}

/// Prints apply events as they arrive. On an interactive terminal a step
/// that starts shows as `»` and its line is rewritten in place when it ends
/// (when nothing else printed in between), so the screen keeps one line per
/// step; otherwise only the end of each step prints.
#[derive(Debug, Default)]
pub struct LiveRenderer {
    interactive: bool,
    /// The step whose `»` line is open (no newline yet).
    open: Option<(String, String)>,
}

impl LiveRenderer {
    pub fn new(interactive: bool) -> Self {
        Self {
            interactive,
            open: None,
        }
    }

    /// The text to print for `event` (possibly empty).
    pub fn event(&mut self, event: &ApplyEvent) -> String {
        let mut out = String::new();
        if self.interactive
            && let ApplyEvent::StepStarted {
                module,
                id,
                task_id,
                ..
            } = event
        {
            self.close(&mut out);
            let id = if id.is_empty() { task_id } else { id };
            let _ = write!(out, "  {} {module}/{id}", glyph(StepState::Running));
            self.open = Some((module.clone(), id.clone()));
            return out;
        }
        let Some(line) = render_event(event) else {
            return out;
        };
        let same_step = match (&self.open, event.step()) {
            (Some((m, i)), Some((module, id))) => m == module && i == id,
            _ => false,
        };
        if same_step {
            out.push_str("\r\x1b[2K");
            self.open = None;
        } else {
            self.close(&mut out);
        }
        out.push_str(&line);
        out.push('\n');
        out
    }

    /// Ends an open `»` line, if any.
    pub fn close(&mut self, out: &mut String) {
        if self.open.take().is_some() {
            out.push('\n');
        }
    }
}

fn status_word(status: ModuleStatus) -> &'static str {
    match status {
        ModuleStatus::Applied => "applied",
        ModuleStatus::Unchanged => "unchanged",
        ModuleStatus::Failed => "FAILED",
        ModuleStatus::Unhealthy => "UNHEALTHY",
        ModuleStatus::RolledBack => "rolled back",
        ModuleStatus::NotStarted => "not started",
        ModuleStatus::Deferred => "deferred",
        ModuleStatus::Indeterminate => "indeterminate",
        ModuleStatus::Skipped => "skipped",
        ModuleStatus::Blocked => "BLOCKED",
    }
}

/// A health report as a list of checks.
pub fn render_health(report: &HealthReport, waived: &[String]) -> String {
    let mut out = String::new();
    if report.results.is_empty() {
        out.push_str("no health checks declared for the selected modules\n");
        return out;
    }
    for r in &report.results {
        let (marker, detail) = match &r.status {
            HealthStatus::Pass => ("✓", String::new()),
            HealthStatus::Fail { reason } => ("✗", format!("  {reason}")),
            HealthStatus::Unknown { reason } => ("?", format!("  unknown: {reason}")),
        };
        let flag = if !r.required {
            "  (informational)"
        } else if waived
            .iter()
            .any(|w| w == &format!("{}/{}", r.module, r.check))
        {
            "  (waived)"
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "  {marker} {:<width$} {}{detail}{flag}",
            format!("{}/{}", r.module, r.check),
            r.kind,
            width = RESOURCE_WIDTH
        );
    }
    let _ = writeln!(
        out,
        "health   {} · {} checks · {} failing",
        if report.healthy(waived) {
            "healthy"
        } else {
            "UNHEALTHY"
        },
        report.results.len(),
        report.failures(waived).count()
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_core::plan::{ModulePlan, Removal, ResourceKey, RevisionId, TaskRef};
    use buckle_core::secrets::Redacted;

    fn step(kind: &str, key: &str, planned: Planned) -> Step {
        Step {
            task: TaskRef {
                module: "m".into(),
                id: key.into(),
                kind: kind.into(),
                name: None,
            },
            resource: ResourceKey::new("m", kind, key),
            planned,
            locks: vec!["apt".into()],
            notifies: vec!["restart".into()],
            always_runs: false,
            removal: RemovalPolicy::Cleanup,
            facts: None,
        }
    }

    #[test]
    fn plan_text_shows_modules_steps_diffs_removals_and_summary() {
        let plan = Plan::new(
            RevisionId::new("working-tree"),
            "pi-1",
            vec![ModulePlan {
                module: "nomad".into(),
                needs: vec!["docker".into()],
                steps: vec![
                    step("apt.package", "nomad", Planned::NoOp),
                    step(
                        "file.template",
                        "/etc/nomad.d/nomad.hcl",
                        Planned::change(Diff::unified(
                            "1 line changed",
                            &Redacted::plain("a\nb\n".into()),
                            &Redacted::plain("a\nc\n".into()),
                        )),
                    ),
                    step(
                        "docker.image",
                        "x",
                        Planned::blocked("docker is not installed"),
                    ),
                ],
                handlers: vec![buckle_core::plan::HandlerRun {
                    module: "nomad".into(),
                    handler: "restart".into(),
                    kind: "systemd.restart".into(),
                    summary: "restart nomad".into(),
                }],
                provides_facts: vec![],
            }],
            vec![Removal {
                resource: ResourceKey::new("grey", "systemd.unit", "grey"),
                policy: RemovalPolicy::Cleanup,
                summary: "disable + stop".into(),
                caution: true,
                introduced_in: Some(RevisionId::new("abcdef0123")),
            }],
            vec![buckle_core::plan::Deferred {
                module: "profiler".into(),
                waiting_on: vec!["docker".into()],
                reason: "waits for facts from docker".into(),
                preview: None,
            }],
        );
        let text = render_plan(
            &plan,
            "working-tree",
            Some(
                &serde_json::json!({"os": {"family": "debian", "codename": "bookworm"}, "hardware": {"arch": "aarch64"}}),
            ),
        );
        assert!(
            text.contains("facts  os=debian/bookworm arch=aarch64"),
            "{text}"
        );
        assert!(
            text.contains("selected  nomad profiler    (2 of 2 modules)"),
            "{text}"
        );
        assert!(text.contains("nomad    needs: docker"), "{text}");
        assert!(text.contains("✓ apt.package[nomad]"), "{text}");
        assert!(
            text.contains("~ file.template[/etc/nomad.d/nomad.hcl]"),
            "{text}"
        );
        assert!(text.contains("      -b\n      +c\n"), "{text}");
        assert!(text.contains("! docker.image[x]"), "{text}");
        assert!(text.contains("→ notify restart"), "{text}");
        assert!(text.contains("↻ handler restart"), "{text}");
        assert!(
            text.contains("deferred: waits for facts from docker"),
            "{text}"
        );
        assert!(text.contains("− grey/systemd.unit[grey]"), "{text}");
        assert!(
            text.contains("introduced in abcdef01") && text.contains("CAUTION"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "summary  1 modules changed · 1 changes · 1 removals · 1 deferred · plan digest {}",
                plan.digest.short()
            )),
            "{text}"
        );
    }

    #[test]
    fn events_and_health_render_one_line_each() {
        let line = render_event(&ApplyEvent::StepFinished {
            module: "m".into(),
            task_id: "/x".into(),
            id: "file.copy[/x]".into(),
            kind: "file.copy".into(),
            changed: true,
            summary: "wrote /x".into(),
        });
        assert_eq!(line.as_deref(), Some("  ✓ m/file.copy[/x]  wrote /x"));
        let report = HealthReport {
            scope: buckle_core::health::HealthScope::Manual,
            results: vec![buckle_core::health::HealthResult {
                module: "m".into(),
                check: "api".into(),
                kind: "http".into(),
                required: true,
                status: HealthStatus::fail("503"),
                at: chrono::Utc::now(),
                duration_ms: 3,
            }],
        };
        let text = render_health(&report, &[]);
        assert!(text.contains("✗ m/api"), "{text}");
        assert!(text.contains("UNHEALTHY · 1 checks · 1 failing"), "{text}");
        assert!(
            render_health(&report, &["m/api".to_string()])
                .contains("healthy · 1 checks · 0 failing")
        );
    }

    fn step_outcome(id: &str, state: StepState) -> StepOutcome {
        StepOutcome {
            id: id.into(),
            kind: "file.copy".into(),
            name: None,
            summary: format!("create {id}"),
            state,
            result: matches!(state, StepState::Changed).then(|| format!("wrote {id}")),
            started_at: None,
            finished_at: None,
            duration_ms: matches!(
                state,
                StepState::Changed | StepState::Unchanged | StepState::Failed
            )
            .then_some(12),
            error_detail: (state == StepState::Failed).then(|| {
                ErrorReport::user("The file could not be written.")
                    .with_causes(["disk full"])
                    .with_advice(["Free some space on the disk.", "Then apply again."])
            }),
        }
    }

    fn outcome_with(modules: Vec<ModuleOutcome>, succeeded: bool) -> ApplyOutcome {
        ApplyOutcome {
            plan_digest: buckle_core::Digest::of(b"plan"),
            succeeded,
            changes: 1,
            removals: 0,
            duration_ms: 1500,
            failed_module: (!succeeded).then(|| "app".into()),
            module_results: modules,
            rolled_back_modules: vec![],
            not_started: vec![],
            health: None,
            error: None,
            error_detail: None,
            removals_run: vec![],
            started_at: None,
            finished_at: None,
            reboot: None,
        }
    }

    fn module_with(name: &str, status: ModuleStatus, steps: Vec<StepOutcome>) -> ModuleOutcome {
        let error = steps
            .iter()
            .find_map(|s| s.error_detail.clone())
            .map(|e| e.prefixed(&format!("{name}/file.copy failed")));
        ModuleOutcome {
            steps,
            changes: 1,
            duration_ms: 40,
            error: error.as_ref().map(ErrorReport::one_line),
            error_detail: error,
            ..ModuleOutcome::new(name, status)
        }
    }

    fn failed_run() -> ApplyOutcome {
        outcome_with(
            vec![
                module_with(
                    "base",
                    ModuleStatus::Applied,
                    vec![
                        step_outcome("file.copy[/a]", StepState::Changed),
                        step_outcome("file.copy[/b]", StepState::Unchanged),
                    ],
                ),
                module_with(
                    "app",
                    ModuleStatus::Failed,
                    vec![
                        step_outcome("file.copy[/c]", StepState::Changed),
                        step_outcome("file.copy[/d]", StepState::Failed),
                        step_outcome("file.copy[/e]", StepState::NotRun),
                    ],
                ),
                module_with(
                    "later",
                    ModuleStatus::NotStarted,
                    vec![step_outcome("file.copy[/f]", StepState::NotRun)],
                ),
            ],
            false,
        )
    }

    fn rolled_back_run() -> ApplyOutcome {
        outcome_with(
            vec![module_with(
                "app",
                ModuleStatus::RolledBack,
                vec![
                    step_outcome("file.copy[/c]", StepState::RolledBack),
                    step_outcome("file.copy[/d]", StepState::Failed),
                ],
            )],
            false,
        )
    }

    fn blocked_run() -> ApplyOutcome {
        let mut blocked = step_outcome("apt.package[nomad]", StepState::Blocked);
        blocked.result = Some("blocked: apt is not installed".into());
        let mut module = module_with("nomad", ModuleStatus::Blocked, vec![blocked]);
        module.error_detail = Some(
            ErrorReport::user("needs attention: nomad/apt.package[nomad] is blocked")
                .with_advice(["Install apt, then apply again."]),
        );
        ApplyOutcome {
            failed_module: None,
            ..outcome_with(vec![module], false)
        }
    }

    #[rstest::rstest]
    #[case::failed(failed_run(), &[
        "base  applied · 1 change · 40ms",
        "  ✓ file.copy[/a]",
        "  · file.copy[/b]",
        "app  FAILED",
        "  ✗ file.copy[/d]",
        "  − file.copy[/e]",
        "later  not started",
        "  − file.copy[/f]",
    ], "FAILED")]
    #[case::rolled_back(rolled_back_run(), &["app  rolled back", "  ↺ file.copy[/c]", "  ✗ file.copy[/d]"], "FAILED")]
    #[case::blocked(blocked_run(), &["nomad  BLOCKED", "  ! apt.package[nomad]", "blocked: apt is not installed", "    needs attention", "    to fix:", "      • Install apt, then apply again."], "NEEDS ATTENTION")]
    fn outcomes_render_as_a_module_and_step_tree(
        #[case] outcome: ApplyOutcome,
        #[case] lines: &[&str],
        #[case] result: &str,
    ) {
        let text = render_outcome(&outcome);
        for line in lines {
            assert!(
                text.lines()
                    .any(|l| l.starts_with(line) || l.contains(line)),
                "missing {line:?} in\n{text}"
            );
        }
        let last = text.lines().last().unwrap();
        assert!(last.starts_with(&format!("result   {result}")), "{last}");
        assert!(!text.contains("To try and fix this"), "{text}");
        assert!(!text.contains("This was caused by"), "{text}");
    }

    #[test]
    fn a_failed_step_has_an_indented_error_block_and_no_inline_advice() {
        let text = render_outcome(&failed_run());
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("  ✗ file.copy[/d]"))
            .expect("the failed step");
        assert!(
            lines[at].ends_with("create file.copy[/d]  12ms"),
            "{}",
            lines[at]
        );
        assert!(
            !lines[at].contains("Free some space"),
            "advice is not inline"
        );
        assert_eq!(
            &lines[at + 1..at + 6],
            &[
                "      The file could not be written.",
                "      caused by: disk full",
                "      to fix:",
                "        • Free some space on the disk.",
                "        • Then apply again.",
            ]
        );
        assert!(
            lines[at + 6].starts_with("  − file.copy[/e]"),
            "the next step follows the block"
        );
        assert_eq!(
            text.matches("Free some space").count(),
            1,
            "the module's error repeats nothing a step already shows"
        );
        let summary = text.lines().last().unwrap();
        assert!(
            summary.contains("modules: 1 applied, 1 FAILED, 1 not started"),
            "{summary}"
        );
        assert!(
            summary.contains("steps: 2 changed, 1 unchanged, 1 failed, 2 not run"),
            "{summary}"
        );
    }

    #[test]
    fn run_errors_and_removals_render_structured() {
        let mut outcome = failed_run();
        outcome.removals_run = vec![buckle_core::engine::RemovalOutcome {
            resource: "old/file.copy[/x]".into(),
            kind: "file.copy".into(),
            summary: "delete /x".into(),
            state: StepState::NotRun,
            caution: false,
            error_detail: None,
        }];
        outcome.error_detail = Some(
            ErrorReport::system("state could not be saved: disk full")
                .with_advice(["Check the state directory."]),
        );
        let text = render_outcome(&outcome);
        assert!(text.contains("removals\n  − old/file.copy[/x]"), "{text}");
        assert!(text.contains("delete /x  (not run)"), "{text}");
        assert!(
            text.contains("error\n  state could not be saved: disk full\n  to fix:\n    • Check the state directory.\n"),
            "{text}"
        );
    }

    #[rstest::rstest]
    #[case::interactive(true)]
    #[case::piped(false)]
    fn live_events_print_one_line_per_step(#[case] interactive: bool) {
        let mut live = LiveRenderer::new(interactive);
        let started = ApplyEvent::StepStarted {
            module: "m".into(),
            task_id: "/x".into(),
            id: "file.copy[/x]".into(),
            kind: "file.copy".into(),
        };
        let failed = ApplyEvent::StepFailed {
            module: "m".into(),
            task_id: "/x".into(),
            id: "file.copy[/x]".into(),
            kind: "file.copy".into(),
            error: "The file could not be written.".into(),
            error_detail: Some(
                ErrorReport::user("The file could not be written.").with_advice(["Free space."]),
            ),
        };
        let mut text = live.event(&ApplyEvent::ModuleStarted { module: "m".into() });
        let running = live.event(&started);
        text.push_str(&running);
        text.push_str(&live.event(&failed));
        if interactive {
            assert_eq!(running, "  » m/file.copy[/x]");
            assert!(text.contains("\r\x1b[2K  ✗ m/file.copy[/x]"), "{text:?}");
        } else {
            assert_eq!(running, "", "piped output only prints the end of a step");
        }
        let visible: Vec<&str> = text
            .split('\n')
            .filter(|l| !l.is_empty())
            .map(|l| l.rsplit("\x1b[2K").next().unwrap())
            .collect();
        assert_eq!(
            visible,
            vec!["m", "  ✗ m/file.copy[/x]  The file could not be written."]
        );
        assert!(!text.contains("Free space"), "no advice in the live line");
    }

    #[test]
    fn plan_errors_render_as_structured_blocks() {
        let plan = Plan::new(RevisionId::new("w"), "pi-1", vec![], vec![], vec![])
            .with_indeterminate(vec![
                buckle_core::plan::Indeterminate::plan_error(
                    "nomad",
                    "planning failed: dump",
                    Some(
                        ErrorReport::user("A template could not be rendered.")
                            .with_causes(["undefined value 'dc'"])
                            .with_advice(["Check the template."]),
                    ),
                ),
                buckle_core::plan::Indeterminate::new("docker", "facts missing"),
            ]);
        let mut plan = plan;
        plan.warnings = vec![
            "module 'nomad' could not be planned: dump; it will not be applied".into(),
            "something else".into(),
        ];
        let text = render_plan(&plan, "w", None);
        assert!(
            text.contains("nomad  could not be planned\n    A template could not be rendered.\n    caused by: undefined value 'dc'\n    to fix:\n      • Check the template.\n"),
            "{text}"
        );
        assert!(
            text.contains("docker  indeterminate: facts missing"),
            "{text}"
        );
        assert!(
            !text.contains("dump"),
            "the dumped reason is not printed: {text}"
        );
        assert!(text.contains("! something else"), "{text}");
    }
}
