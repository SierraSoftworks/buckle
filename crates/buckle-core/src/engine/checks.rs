//! Running a module's health checks: once (for `buckle health` and the
//! rollout scopes) or with settle semantics after an apply (design §07).

use super::planner::{Planner, collect_facts};
use super::{ApplyEvent, Inputs};
use crate::health::{HealthReport, HealthResult, HealthScope, HealthStatus};
use crate::model::HealthDecl;
use crate::selectors::HostContext;
use crate::task::{Mode, Source};
use std::sync::Arc;
use std::time::Duration;
use tracing::Instrument;

/// Runs one check, with its timeout. Never panics or errors: problems become a
/// `Fail` or `Unknown` status with a reason.
async fn run_check(
    planner: &Planner<'_>,
    module: &str,
    ctx: &Arc<HostContext>,
    source: &Arc<dyn Source>,
    decl: &HealthDecl,
) -> HealthResult {
    let name = decl.display_name().to_owned();
    let kind = decl.check.kind.clone();
    let started = tokio::time::Instant::now();
    let at = planner.system.now();
    let status = async {
        let check = match crate::health::lookup(&kind) {
            Ok(c) => c,
            Err(e) => return HealthStatus::unknown(e.description()),
        };
        let platform = planner.system.platform();
        if !check.platforms().contains(platform) {
            return HealthStatus::unknown(format!(
                "the {kind} check kind does not support {platform}"
            ));
        }
        let task_ctx = ctx.in_task(&kind, &name, &[]);
        let json = match super::context::render_value(&decl.check.spec, &planner.templater, &task_ctx) {
            Ok(j) => j,
            Err(e) => return HealthStatus::fail(format!("the check's spec could not be rendered: {}", e.message())),
        };
        let spec = match check.parse(json) {
            Ok(s) => s,
            Err(e) => return HealthStatus::fail(e.message()),
        };
        let cx = planner.cx(module, source.clone(), task_ctx, Mode::Apply);
        match tokio::time::timeout(decl.timeout, check.check(&cx, &spec)).await {
            Ok(status) => status,
            Err(_) => HealthStatus::fail(format!(
                "timed out after {}",
                humantime::format_duration(decl.timeout)
            )),
        }
    }
    .instrument(tracing::info_span!("health.check", buckle.module = %module, buckle.check = %name, buckle.check.kind = %kind))
    .await;
    HealthResult {
        module: module.to_owned(),
        check: name,
        kind,
        required: decl.required,
        status,
        at,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

/// Runs every check of a module once.
pub(super) async fn run_once(
    planner: &Planner<'_>,
    module: &str,
    decls: &[HealthDecl],
    ctx: &Arc<HostContext>,
    source: &Arc<dyn Source>,
) -> Vec<HealthResult> {
    let mut results = Vec::with_capacity(decls.len());
    for decl in decls {
        results.push(run_check(planner, module, ctx, source, decl).await);
    }
    results
}

/// Runs the checks, and while a required (non-waived) check fails and less than
/// `settle` has elapsed since the first failure, re-runs them every `interval`
/// of the failing checks. Returns the final results and whether the module is
/// healthy.
pub(super) async fn run_settled(
    planner: &Planner<'_>,
    module: &str,
    decls: &[HealthDecl],
    ctx: &Arc<HostContext>,
    source: &Arc<dyn Source>,
    settle: Duration,
    waived: &[String],
    events: &tokio::sync::mpsc::Sender<ApplyEvent>,
) -> (Vec<HealthResult>, bool) {
    if decls.is_empty() {
        return (Vec::new(), true);
    }
    let mut first_failure: Option<tokio::time::Instant> = None;
    loop {
        let results = run_once(planner, module, decls, ctx, source).await;
        for r in &results {
            let reason = match &r.status {
                HealthStatus::Pass => None,
                HealthStatus::Fail { reason } | HealthStatus::Unknown { reason } => {
                    Some(reason.clone())
                }
            };
            let _ = events
                .send(ApplyEvent::HealthChecked {
                    module: module.to_owned(),
                    check: r.check.clone(),
                    passed: r.status.is_pass(),
                    reason,
                })
                .await;
        }
        let failing: Vec<&HealthResult> = results
            .iter()
            .filter(|r| r.counts_as_failure(waived))
            .collect();
        if failing.is_empty() {
            return (results, true);
        }
        let now = tokio::time::Instant::now();
        let first = *first_failure.get_or_insert(now);
        let elapsed = now.saturating_duration_since(first);
        if elapsed >= settle {
            tracing::warn!(
                buckle.module = module,
                failing = ?failing.iter().map(|r| r.check.as_str()).collect::<Vec<_>>(),
                "health checks still failing after the settle window"
            );
            return (results, false);
        }
        let interval = failing
            .iter()
            .filter_map(|r| decls.iter().find(|d| d.display_name() == r.check))
            .map(|d| d.interval)
            .min()
            .unwrap_or(Duration::from_secs(30))
            .max(Duration::from_millis(1));
        let remaining = settle - elapsed;
        tokio::time::sleep(interval.min(remaining)).await;
    }
}

/// `Engine::health`: the checks of the named modules (or every module selected
/// on this host), once, with the given scope.
pub(super) async fn run_scope(
    inputs: &Inputs,
    scope: HealthScope,
    modules: Option<&[String]>,
) -> crate::Result<HealthReport> {
    let planner = Planner::from_inputs(inputs, &inputs.revision)?;
    let facts = collect_facts(inputs, &planner).await;
    let contexts = planner.contexts(&facts);
    let targets: Vec<&super::LoadedModule> = match modules {
        Some(names) => names
            .iter()
            .map(|n| {
                inputs.revision.module(n).ok_or_else(|| {
                    crate::error::user(
                        format!("There is no module named '{n}' in this configuration."),
                        &["Check the module name; `buckle plan` lists the modules selected for this host."],
                    )
                })
            })
            .collect::<crate::Result<Vec<_>>>()?,
        None => planner.select(&contexts)?.order,
    };
    let mut results = Vec::new();
    for module in targets {
        if module.config.health.is_empty() {
            continue;
        }
        let ctx = planner.module_context(&contexts, &module.config)?;
        results.extend(
            run_once(
                &planner,
                &module.config.name,
                &module.config.health,
                &ctx,
                &module.source,
            )
            .await,
        );
    }
    Ok(HealthReport { scope, results })
}
