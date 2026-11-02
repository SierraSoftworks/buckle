//! Text rendering of plans, apply events, outcomes and health reports for the
//! terminal (design §11). Everything rendered here is already redacted by
//! construction; this module only lays it out.

use buckle_core::engine::{ApplyEvent, ApplyOutcome, ModuleStatus};
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

    let other_warnings: Vec<&String> = plan
        .warnings
        .iter()
        .filter(|w| !w.starts_with("removal:"))
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

/// One line per apply event, printed as events arrive.
pub fn render_event(event: &ApplyEvent) -> String {
    match event {
        ApplyEvent::ModuleStarted { module } => module.clone(),
        ApplyEvent::StepStarted {
            module,
            task_id,
            kind,
            ..
        } => {
            format!("  … {module}/{kind}[{task_id}]")
        }
        ApplyEvent::StepFinished {
            module,
            task_id,
            kind,
            changed,
            summary,
            ..
        } => format!(
            "  {} {module}/{kind}[{task_id}]  {summary}",
            if *changed { "~" } else { "✓" }
        ),
        ApplyEvent::StepFailed {
            module,
            task_id,
            kind,
            error,
            ..
        } => format!("  ✗ {module}/{kind}[{task_id}]  {error}"),
        ApplyEvent::StepBlocked {
            module, id, reason, ..
        } => format!("  ! {module}/{id}  blocked: {reason}"),
        ApplyEvent::StepSkipped {
            module, id, reason, ..
        } => format!("  · {module}/{id}  skipped: {reason}"),
        ApplyEvent::HandlerRun { module, handler } => format!("  ↻ handler {module}:{handler}"),
        ApplyEvent::ModuleFinished { module, status } => {
            format!("{module}  {}", status_word(*status))
        }
        ApplyEvent::RemovalRun { resource, summary } => format!("  − {resource}  {summary}"),
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

/// The closing summary of an apply.
pub fn render_outcome(outcome: &ApplyOutcome) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "result   {} · {} changes · {} removals · {:.1}s · plan digest {}",
        if outcome.succeeded {
            "succeeded"
        } else {
            "FAILED"
        },
        outcome.changes,
        outcome.removals,
        outcome.duration_ms as f64 / 1000.0,
        outcome.plan_digest.short()
    );
    for module in &outcome.module_results {
        let _ = write!(
            out,
            "  {:<width$} {}",
            module.module,
            status_word(module.status),
            width = RESOURCE_WIDTH
        );
        if let Some(error) = &module.error {
            let _ = write!(out, "  {error}");
        }
        out.push('\n');
    }
    if !outcome.rolled_back_modules.is_empty() {
        let _ = writeln!(
            out,
            "rolled back: {}",
            outcome.rolled_back_modules.join(", ")
        );
    }
    if !outcome.not_started.is_empty() {
        let _ = writeln!(out, "not started: {}", outcome.not_started.join(", "));
    }
    if let Some(error) = &outcome.error {
        let _ = writeln!(out, "error: {error}");
    }
    out
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
        assert_eq!(line, "  ~ m/file.copy[/x]  wrote /x");
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
}
