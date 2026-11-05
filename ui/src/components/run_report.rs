//! A host's run, as the plan it follows (design §04, §12): a summary strip
//! (state, counts, timing, revision, plan, trace), then every module in plan
//! order with its steps and handlers, and the removals. Each module's
//! heading carries what it changes, its health through the run and how long
//! it took. Problems open by
//! default, so a failed run reads from the top down to what went wrong; the
//! rest is a click away, or all of it with "expand all". The same view shows
//! a run in progress, so a live update only changes states.

use buckle_api::{ModuleReport, RemovalReport, RunReport, StepReport, StepState};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::changes::{Changes, Took};
use super::diff::secret_text;
use super::error_block::ErrorBlock;
use super::icons;
use super::link::RouteLink;
use super::ui::{Pill, TraceLink};
use crate::app::Route;
use crate::hooks::use_now;
use crate::logic::fleet_plan::Tally;
use crate::logic::format::{
    commit, count, millis, relative, rollout_label, short, since, timestamp,
};
use crate::logic::health::{Bucket, CheckState, current_state, timeline};
use crate::logic::run::{
    current_step, elapsed_ms, error_tone, expanded_by_default, module_has_problem, module_label,
    module_slug, module_tally, module_tone, resource_module, run_finished, run_label, run_tally,
    run_tone, step_glyph, step_key, step_label, step_progress, step_slug, step_tone,
};
use crate::logic::tone::{Tone, check_status_tone};

#[derive(Properties, PartialEq)]
pub struct RunReportProps {
    pub report: RunReport,
    /// Shown inside its own rollout's page: no link back to it.
    #[prop_or_default]
    pub in_rollout: bool,
    /// A cluster's run: its modules are workloads.
    #[prop_or_default]
    pub workloads: bool,
}

#[function_component(RunReportView)]
pub fn run_report(props: &RunReportProps) -> Html {
    let r = &props.report;
    let live = !run_finished(r.state);
    let now = use_now(if live { 1 } else { 30 });
    // What the reader opened or closed; everything else follows the
    // defaults, so a module that starts running opens by itself.
    // A list rather than a map: a run has a handful of modules.
    let overrides = use_state_eq(Vec::<(String, bool)>::new);
    let problems_only = use_state_eq(|| false);
    {
        let (overrides, problems_only) = (overrides.clone(), problems_only.clone());
        use_effect_with(r.id.clone(), move |_| {
            overrides.set(Vec::new());
            problems_only.set(false);
            || ()
        });
    }

    let is_open = |m: &ModuleReport| {
        overrides
            .iter()
            .find(|(name, _)| *name == m.name)
            .map(|(_, open)| *open)
            .unwrap_or_else(|| expanded_by_default(m))
    };
    let set_all = |open: bool| {
        let overrides = overrides.clone();
        let names: Vec<String> = r.modules.iter().map(|m| m.name.clone()).collect();
        Callback::from(move |_: MouseEvent| {
            overrides.set(names.iter().map(|n| (n.clone(), open)).collect())
        })
    };
    let toggle_problems = {
        let problems_only = problems_only.clone();
        Callback::from(move |_: MouseEvent| problems_only.set(!*problems_only))
    };

    let modules: Vec<&ModuleReport> = r
        .modules
        .iter()
        .filter(|m| !*problems_only || module_has_problem(m))
        .collect();
    let removals: Vec<&RemovalReport> = r
        .removals
        .iter()
        .filter(|x| !*problems_only || removal_problem(x))
        .collect();
    let problems = r.modules.iter().filter(|m| module_has_problem(m)).count()
        + r.removals.iter().filter(|x| removal_problem(x)).count();

    let body = if r.modules.is_empty() && r.removals.is_empty() {
        html! { <p class="muted">{ if props.workloads {
            "Nothing was selected for this cluster: the run had no workloads to apply."
        } else {
            "Nothing was selected for this host: the run had no modules to apply."
        } }</p> }
    } else if modules.is_empty() && removals.is_empty() {
        html! { <p class="muted" data-testid="run-no-problems">{ "Nothing failed or needs attention in this run." }</p> }
    } else {
        html! { <>
            if !modules.is_empty() {
                <ol class="run-modules" aria-label={if props.workloads { "Workloads in plan order" } else { "Modules in plan order" }}>
                    { for modules.iter().map(|m| {
                        let open = is_open(m);
                        let toggle = {
                            let (overrides, name) = (overrides.clone(), m.name.clone());
                            Callback::from(move |_: MouseEvent| {
                                let mut next: Vec<(String, bool)> = (*overrides)
                                    .iter()
                                    .filter(|(n, _)| *n != name)
                                    .cloned()
                                    .collect();
                                next.push((name.clone(), !open));
                                overrides.set(next);
                            })
                        };
                        module_row(r, m, open, toggle, now, live)
                    }) }
                </ol>
            }
            if !removals.is_empty() {
                <section class="run-removals" aria-label="Removals">
                    <h4 class="run__section-title">
                        { "Removals" }<span class="muted">{ " · resources the revision no longer declares" }</span>
                        <Changes tally={Tally { remove: r.removals.len(), ..Tally::default() }} />
                    </h4>
                    <ol class="run-steps run-steps--removals">{ for removals.iter().map(|x| removal_row(x, now, live)) }</ol>
                </section>
            }
        </> }
    };

    html! {
        <div class="run" data-run={r.id.clone()} data-state={run_label(r.state).replace(' ', "-")}>
            { summary(r, now, props.in_rollout, if props.workloads { "workloads" } else { "modules" }) }
            <ErrorBlock error={r.error.clone()} context={AttrValue::from("The run")} />
            if !r.modules.is_empty() {
                <div class="run__toolbar">
                    <div class="chips" role="group" aria-label="Filter modules">
                        <button type="button" class={classes!("chip", problems_only.then_some("chip--active"))}
                            aria-pressed={problems_only.to_string()} onclick={toggle_problems}>
                            { "Problems only" }<span class="chip__count">{ problems }</span>
                        </button>
                    </div>
                    <div class="run__expand">
                        <button type="button" class="btn btn--subtle btn--small" onclick={set_all(true)}>{ "Expand all" }</button>
                        <button type="button" class="btn btn--subtle btn--small" onclick={set_all(false)}>{ "Collapse all" }</button>
                    </div>
                </div>
            }
            { body }
        </div>
    }
}

fn removal_problem(x: &RemovalReport) -> bool {
    matches!(x.state, StepState::Failed | StepState::Blocked) || x.error.is_some()
}

/// The strip at the top: state, step counts, timing, what was applied.
fn summary(r: &RunReport, now: DateTime<Utc>, in_rollout: bool, noun: &str) -> Html {
    let s = &r.counts.steps;
    let live = !run_finished(r.state);
    let mut counts = vec![
        ("changed", s.changed, Tone::Ok),
        ("unchanged", s.unchanged, Tone::Neutral),
    ];
    // While it runs these can still move; once it has finished a zero is
    // noise and only what happened is listed.
    for (label, n, tone) in [
        ("failed", s.failed, Tone::Danger),
        ("blocked", s.blocked, Tone::Warn),
        ("pending", s.pending, Tone::Neutral),
    ] {
        if live || n > 0 {
            counts.push((label, n, tone));
        }
    }
    for (label, n, tone) in [
        ("running", s.running, Tone::Active),
        ("rolled back", s.rolled_back, Tone::Mute),
        ("not run", s.not_run, Tone::Neutral),
        ("skipped", s.skipped, Tone::Neutral),
    ] {
        if n > 0 {
            counts.push((label, n, tone));
        }
    }
    let took = elapsed_ms(None, r.started_at, r.finished_at, now, live).map(millis);
    let tally = run_tally(r);
    let (done, total) = step_progress(r);
    let modules_done = r
        .modules
        .iter()
        .filter(|m| {
            !matches!(
                m.state,
                buckle_api::ModuleState::Pending
                    | buckle_api::ModuleState::Running
                    | buckle_api::ModuleState::Deferred
            )
        })
        .count();
    html! { <>
        <div class="run__summary">
            <Pill tone={run_tone(r.state)} label={run_label(r.state)} />
            <ul class="run__counts" aria-label="Steps by state">
                { for counts.into_iter().map(|(label, n, tone)| html! {
                    <li class={classes!("run-count", tone.class(), (n == 0).then_some("run-count--zero"))}
                        data-count={label.replace(' ', "-")}>
                        <span class="run-count__n">{ n }</span>{ " " }{ label }
                    </li>
                }) }
            </ul>
            <span class="run__meta">
                { match r.started_at {
                    Some(at) => html! { <span title={timestamp(at)}>{ format!("started {}", relative(at, now)) }</span> },
                    None => html! { <span>{ "not started" }</span> },
                } }
                <Changes {tally} />
                if let Some(took) = took { <Took label={took} running={live} /> }
            </span>
            <span class="run__meta">
                <span class="mono" title={r.revision.clone()}>{ format!("revision {}", commit(&r.revision)) }</span>
                <span class="mono" title={r.plan_digest.clone()}>{ format!("plan {}", short(&r.plan_digest)) }</span>
                if let (Some(id), false) = (&r.rollout_id, in_rollout) {
                    <RouteLink to={Route::rollout(id)}>{ format!("rollout {}", rollout_label(id)) }</RouteLink>
                }
                if r.trace_id.is_some() { <TraceLink id={r.trace_id.clone()} url={r.trace_url.clone()} /> }
            </span>
        </div>
        if live && total > 0 {
            <div class="run__progress">
                <div class="progress" role="progressbar" aria-label="Steps finished"
                    aria-valuemin="0" aria-valuemax={total.to_string()} aria-valuenow={done.to_string()}>
                    <span class="progress__bar" style={format!("width: {}%", done * 100 / total)} />
                </div>
                <span class="muted small">{ format!("{done} of {} · {modules_done} of {} {noun} done", count(total as usize, "step", "steps"), r.modules.len()) }</span>
            </div>
        }
    </> }
}

fn module_row(
    r: &RunReport,
    m: &ModuleReport,
    open: bool,
    toggle: Callback<MouseEvent>,
    now: DateTime<Utc>,
    live: bool,
) -> Html {
    let slug = module_slug(m.state);
    let body_id = format!(
        "run-{}-{}",
        r.id.replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
        m.name.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
    );
    let running = m.state == buckle_api::ModuleState::Running;
    let took = elapsed_ms(
        m.duration_ms,
        m.started_at,
        m.finished_at,
        now,
        running && live,
    )
    .map(millis);
    let current = current_step(m);
    let aside = match (current, &m.note) {
        (Some(step), _) => html! {
            <span class="run-module__current">{ "running " }<span class="mono">{ step.id.clone() }</span></span>
        },
        (None, Some(note)) => html! { <span class="run-module__note">{ note.clone() }</span> },
        _ => html! {},
    };
    let empty = match m.state {
        buckle_api::ModuleState::Deferred => "Planned once the facts it waits for arrive.",
        buckle_api::ModuleState::Indeterminate => {
            "Never applied: its selection could not be decided on this host."
        }
        buckle_api::ModuleState::NotStarted => "Not started: the run halted before it.",
        buckle_api::ModuleState::Skipped => "Not selected once its facts arrived.",
        _ => "No steps.",
    };
    // The whole heading toggles the module; the button inside it is what
    // the keyboard and assistive technology reach.
    html! {
        <li class={classes!("run-module", format!("run-module--{slug}"), open.then_some("is-open"))}
            data-module={m.name.clone()} data-state={slug}>
            <div class="run-module__head" onclick={toggle}>
                <button type="button" class="run-module__row" aria-expanded={open.to_string()}
                    aria-controls={body_id.clone()}>
                    <span class="run-module__chevron" aria-hidden="true">{ icons::chevron() }</span>
                    <Pill tone={module_tone(m.state)} label={module_label(m.state)} />
                    <span class="run-module__title">
                        <span class="run-module__name mono">{ m.name.clone() }</span>
                        { aside }
                    </span>
                </button>
                <span class="run-module__aside">
                    <Changes tally={module_tally(m)} />
                    { health_bar(m, r.started_at) }
                    <span class="run-module__duration">
                        if let Some(took) = took { <Took label={took} running={running && live} /> }
                    </span>
                </span>
            </div>
            if open {
                <div class="run-module__body" id={body_id}>
                    if let (Some(note), Some(_)) = (&m.note, current) { <p class="run-module__note">{ note.clone() }</p> }
                    <ErrorBlock error={m.error.clone()} tone={error_tone(module_tone(m.state))}
                        context={AttrValue::from(format!("{} {}", m.name, module_label(m.state)))} />
                    if m.steps.is_empty() {
                        <p class="muted small">{ empty }</p>
                    } else {
                        <ol class="run-steps" aria-label={format!("Steps of {}", m.name)}>
                            { for m.steps.iter().map(|s| step_row(s, now, live, false)) }
                        </ol>
                    }
                    if !m.handlers.is_empty() {
                        <h5 class="run-module__sub">{ "Handlers" }</h5>
                        <ol class="run-steps run-steps--handlers" aria-label={format!("Handlers of {}", m.name)}>
                            { for m.handlers.iter().map(|s| step_row(s, now, live, false)) }
                        </ol>
                    }
                </div>
            }
        </li>
    }
}

fn step_row(s: &StepReport, now: DateTime<Utc>, live: bool, caution: bool) -> Html {
    let slug = step_slug(s.state);
    let running = s.state == StepState::Running;
    let took = elapsed_ms(
        s.duration_ms,
        s.started_at,
        s.finished_at,
        now,
        running && live,
    )
    .map(millis);
    html! {
        <li class={classes!("run-step", format!("run-step--{slug}"))} data-step={s.id.clone()} data-state={slug}>
            <span class={classes!("run-step__glyph", step_tone(s.state).class())} aria-hidden="true">{ step_glyph(s.state) }</span>
            <div class="run-step__main">
                <div class="run-step__head">
                    if let Some(name) = &s.name { <span class="run-step__name">{ name.clone() }</span> }
                    <span class="badge run-step__kind">{ s.kind.clone() }</span>
                    <span class="run-step__key mono">{ step_key(&s.kind, &s.id).to_string() }</span>
                    if s.destructive { <span class="badge badge--danger">{ icons::alert() }{ "destructive" }</span> }
                    if caution {
                        <span class="badge badge--warn" title="This kind's removal can take unrelated things down (dependent packages, files owned by a user).">{ icons::alert() }{ "caution" }</span>
                    }
                    <span class="run-step__state">{ step_label(s.state) }</span>
                </div>
                if !s.summary.is_empty() { <p class="run-step__summary">{ secret_text(&s.summary) }</p> }
                if let Some(result) = s.result.as_deref().filter(|t| !t.is_empty()) {
                    <p class="run-step__result">{ secret_text(result) }</p>
                }
                <ErrorBlock error={s.error.clone()} tone={error_tone(step_tone(s.state))} />
            </div>
            <span class="run-step__duration">{ took.unwrap_or_default() }</span>
        </li>
    }
}

/// A removal, drawn as the step it is: its module where the name goes, its
/// kind and key (`data-step` keeps the whole resource), and a caution badge
/// where its kind's removal can take more with it.
fn removal_row(x: &RemovalReport, now: DateTime<Utc>, live: bool) -> Html {
    let step = StepReport {
        id: x.resource.clone(),
        kind: x.kind.clone(),
        name: resource_module(&x.resource).map(str::to_string),
        summary: x.summary.clone(),
        state: x.state,
        result: None,
        diff: None,
        destructive: false,
        handler: false,
        started_at: None,
        finished_at: None,
        duration_ms: None,
        error: x.error.clone(),
    };
    step_row(&step, now, live, x.caution)
}

/// How many slices a module's health bar has.
const HEALTH_BUCKETS: usize = 20;

/// A module's health through the run (bake, pre-phase and drift readings),
/// in its heading: equal time slices, each as bad as its worst check, with
/// the checks' readings on hover or focus. An empty slot keeps the bars of
/// a run's modules in line when one has no readings.
fn health_bar(m: &ModuleReport, started: Option<DateTime<Utc>>) -> Html {
    let buckets = timeline(&m.health, started, HEALTH_BUCKETS);
    let Some(t0) = buckets.first().map(|b| b.from) else {
        return html! { <span class="hbar hbar--inline hbar--none" aria-hidden="true" /> };
    };
    let current = current_state(&m.health);
    // A click on a slice is for its popover, not the module's toggle.
    let keep = Callback::from(|e: MouseEvent| e.stop_propagation());
    html! {
        <div class="hbar hbar--inline" role="list" onclick={keep}
            data-state={current.map(CheckState::slug)}
            aria-label={format!("Health of {} over the run{}", m.name, current.map(|c| format!(": {} now", c.label())).unwrap_or_default())}
            title={current.map(|c| format!("health: {} now", c.label()))}>
            { for buckets.iter().map(|b| health_bucket(b, t0)) }
        </div>
    }
}

fn health_bucket(b: &Bucket<'_>, t0: DateTime<Utc>) -> Html {
    let Some(state) = b.state else {
        return html! { <span class="hbar__bucket hbar__bucket--empty" role="listitem" aria-label="no readings yet" /> };
    };
    let span = format!("{} to {}", since(b.from, t0), since(b.until, t0));
    let pop = html! { <>
        <span class="hbar__when small muted" title={format!("{} to {}", timestamp(b.from), timestamp(b.until))}>{ span.clone() }</span>
        <ul class="hbar__checks">
            { for b.checks.iter().map(|c| html! {
                <li class="hbar__check" data-check={c.reading.check.clone()} data-state={c.state.slug()}>
                    <Pill tone={check_status_tone(&c.reading.status)} label={c.reading.status.clone()} />
                    <span class="mono">{ c.reading.check.clone() }</span>
                    <span class="badge">{ c.reading.scope.clone() }</span>
                    if !c.reading.required { <span class="muted small">{ "informational" }</span> }
                    if let Some(reason) = &c.reading.reason { <span class="hbar__reason">{ reason.clone() }</span> }
                    if c.carried {
                        <span class="muted small" title={timestamp(c.reading.at)}>{ format!("last read {}", since(c.reading.at, t0)) }</span>
                    }
                </li>
            }) }
        </ul>
    </> };
    html! {
        <HealthSlice class={classes!("hbar__bucket", state.tone().class())} state={state.slug()}
            label={format!("{}: {span}", state.label())} {pop} />
    }
}

#[derive(Properties, PartialEq)]
struct SliceProps {
    class: Classes,
    state: AttrValue,
    label: AttrValue,
    pop: Html,
}

/// A slice of a health bar. Its popover joins the page on first hover or
/// focus (CSS shows and hides it from then on): a report has a popover per
/// slice per module, and adding them all up front is most of what the page
/// inserts.
#[function_component(HealthSlice)]
fn health_slice(props: &SliceProps) -> Html {
    let shown = use_state_eq(|| false);
    let (enter, focus) = {
        let (a, b) = (shown.clone(), shown.clone());
        (
            Callback::from(move |_: MouseEvent| a.set(true)),
            Callback::from(move |_: FocusEvent| b.set(true)),
        )
    };
    html! {
        <span class={props.class.clone()} role="listitem" tabindex="0" data-state={props.state.clone()}
            aria-label={props.label.clone()} onmouseenter={enter} onfocus={focus}>
            if *shown { <span class="hbar__pop" role="tooltip">{ props.pop.clone() }</span> }
        </span>
    }
}
