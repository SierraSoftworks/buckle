//! One distinct plan, as a reviewer reads it (design §11): the hosts that
//! share it, what changes module by module with unified diffs, destructive
//! steps and removals called out, deferred modules with their reason.

use buckle_api::{DiffKind, ModuleNote, PlanGroup, PlanView, StepOutcome, StepView};
use yew::prelude::*;

use super::diff::{DiffView, secret_text};
use super::icons;
use crate::logic::format::{count, short};
use crate::logic::plan::review;

#[derive(Properties, PartialEq)]
pub struct PlanGroupProps {
    pub group: PlanGroup,
    /// The hosts of the phase under review; others sharing the plan are
    /// listed apart.
    #[prop_or_default]
    pub focus: Vec<String>,
    #[prop_or_default]
    pub open: bool,
}

#[function_component(PlanGroupView)]
pub fn plan_group(props: &PlanGroupProps) -> Html {
    let g = &props.group;
    let r = g.plan.as_ref().map(review);
    let (here, elsewhere): (Vec<&String>, Vec<&String>) = if props.focus.is_empty() {
        (g.hosts.iter().collect(), Vec::new())
    } else {
        g.hosts.iter().partition(|h| props.focus.contains(h))
    };
    let destructive = r
        .as_ref()
        .map(|r| r.destructive())
        .unwrap_or(g.removals as usize);
    let summary = html! {
        <summary class="plan__summary">
            <span class="plan__digest mono" title={g.digest.clone()}>{ short(&g.digest) }</span>
            <span class="plan__hosts">{ here.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", ") }</span>
            <span class="plan__counts">
                { count(g.changes as usize, "change", "changes") }
                if g.removals > 0 { { " · " }{ count(g.removals as usize, "removal", "removals") } }
            </span>
            if g.destructive {
                <span class="badge badge--danger">{ icons::alert() }{ format!("{destructive} destructive") }</span>
            }
        </summary>
    };
    html! {
        <details class={classes!("plan", g.destructive.then_some("plan--destructive"))} open={props.open}
            data-digest={g.digest.clone()}>
            { summary }
            <div class="plan__body">
                if !elsewhere.is_empty() {
                    <p class="muted">{ format!("Also the plan of {} in other stages.", elsewhere.iter().map(|h| h.as_str()).collect::<Vec<_>>().join(", ")) }</p>
                }
                { match &g.plan {
                    Some(plan) => body(plan),
                    None => html! { <p class="muted">{ "The plan document is not available; the counts above come from the server's summary." }</p> },
                } }
            </div>
        </details>
    }
}

/// A section of modules the plan only has a note about.
fn notes(
    title: &str,
    aside: &str,
    glyph: &str,
    class: &str,
    list: &[ModuleNote],
    footer: Html,
) -> Html {
    if list.is_empty() {
        return html! {};
    }
    html! {
        <section class="plan-module">
            <header class="plan-module__head">
                <h4 class="plan-module__name">{ title.to_string() }</h4>
                if !aside.is_empty() { <span class="muted">{ aside.to_string() }</span> }
            </header>
            <ul class="steps">
                { for list.iter().map(|n| html! {
                    <li class={classes!("step", class.to_string())}>
                        <span class="step__glyph" aria-hidden="true">{ glyph.to_string() }</span>
                        <span class="mono">{ n.module.clone() }</span>
                        <span class="step__summary">{ secret_text(&n.reason) }</span>
                        if !n.waiting_on.is_empty() {
                            <span class="muted small">{ format!("waits for {}", n.waiting_on.join(", ")) }</span>
                        }
                        if n.plan_error { <span class="badge badge--danger">{ "plan error" }</span> }
                    </li>
                }) }
            </ul>
            { footer }
        </section>
    }
}

fn body(plan: &PlanView) -> Html {
    let warnings = (!plan.warnings.is_empty()).then(|| html! {
        <ul class="plan__warnings">
            { for plan.warnings.iter().map(|w| html! { <li>{ icons::alert() }{ secret_text(w) }</li> }) }
        </ul>
    });
    let modules = plan.modules.iter().map(|m| {
        let changed: Vec<&StepView> = m.steps.iter().filter(|s| s.outcome != StepOutcome::NoOp).collect();
        let unchanged = m.steps.len() - changed.len();
        html! {
            <section class="plan-module">
                <header class="plan-module__head">
                    <h4 class="plan-module__name mono">{ m.module.clone() }</h4>
                    if !m.needs.is_empty() {
                        <span class="muted">{ format!("needs: {}", m.needs.join(", ")) }</span>
                    }
                    if unchanged > 0 {
                        <span class="muted">{ format!("{unchanged} unchanged") }</span>
                    }
                </header>
                <ul class="steps">
                    { for changed.into_iter().map(step) }
                    { for m.handlers.iter().map(|h| html! {
                        <li class="step step--handler">
                            <span class="step__glyph" aria-hidden="true">{ "↻" }</span>
                            <span class="mono">{ format!("handler {}:{}", m.module, h.handler) }</span>
                            <span class="step__summary">{ h.summary.clone() }</span>
                        </li>
                    }) }
                </ul>
            </section>
        }
    });
    let removals = (!plan.removals.is_empty()).then(|| html! {
        <section class="plan-module plan-module--removals">
            <header class="plan-module__head">
                <h4 class="plan-module__name">{ "Removals" }</h4>
                <span class="muted">{ "resources the revision no longer declares" }</span>
            </header>
            <ul class="steps">
                { for plan.removals.iter().map(|r| html! {
                    <li class={classes!("step", if r.cleanup { "step--destructive" } else { "step--forget" })}>
                        <span class="step__glyph" aria-hidden="true">{ "−" }</span>
                        <span class="mono step__resource">{ r.resource() }</span>
                        <span class="step__summary">{ secret_text(&r.summary) }</span>
                        if r.cleanup { <span class="badge badge--danger">{ "removal" }</span> }
                        else { <span class="badge">{ "forget only" }</span> }
                        if r.caution && r.cleanup {
                            <span class="badge badge--warn" title="This kind's removal can take unrelated things down (dependent packages, files owned by a user).">{ icons::alert() }{ "caution" }</span>
                        }
                    </li>
                }) }
            </ul>
        </section>
    });
    let rehomed = (!plan.rehomed.is_empty()).then(|| html! {
        <section class="plan-module">
            <header class="plan-module__head">
                <h4 class="plan-module__name">{ "Taken over" }</h4>
                <span class="muted">{ "records a declared resource now manages, kept instead of removed" }</span>
            </header>
            <ul class="steps">
                { for plan.rehomed.iter().map(|r| html! {
                    <li class="step step--forget">
                        <span class="step__glyph" aria-hidden="true">{ "→" }</span>
                        <span class="mono step__resource">{ format!("{} → {}", r.from, r.to) }</span>
                        <span class="step__summary">{ secret_text(&r.summary) }</span>
                        <span class="badge">{ r.how.clone() }</span>
                    </li>
                }) }
            </ul>
        </section>
    });
    html! { <>
        { warnings.unwrap_or_default() }
        { for modules }
        { removals.unwrap_or_default() }
        { rehomed.unwrap_or_default() }
        { notes("Deferred", "", "…", "step--deferred", &plan.deferred,
            html! { <p class="muted small">{ "If a deferred module's plan turns out destructive, the agent stops before it and asks for approval again." }</p> }) }
        { notes("Indeterminate", "not applied; their resources are never removed", "?", "step--blocked", &plan.indeterminate, html! {}) }
        { notes("Held", "not applied at this revision; left alone", "‖", "step--deferred", &plan.held, html! {}) }
        if !plan.skipped.is_empty() {
            <p class="muted small">{ format!("Not selected: {}", plan.skipped.join("; ")) }</p>
        }
    </> }
}

fn step(s: &StepView) -> Html {
    let (glyph, summary, detail) = match s.outcome {
        StepOutcome::Change => {
            let glyph = match s.diff {
                Some(DiffKind::Create) => "+",
                Some(DiffKind::Delete) => "−",
                _ => "~",
            };
            let detail = match (&s.unified, &s.preview) {
                (Some(u), _) => html! { <DiffView unified={u.clone()} /> },
                (None, Some(p)) => html! { <pre class="step__preview">{ secret_text(p) }</pre> },
                _ => html! {},
            };
            (glyph, s.summary.clone(), detail)
        }
        StepOutcome::Blocked => ("!", format!("blocked: {}", s.summary), html! {}),
        StepOutcome::NoOp => ("✓", "no change".into(), html! {}),
    };
    html! {
        <li class={classes!("step", s.destructive.then_some("step--destructive"), (s.outcome == StepOutcome::Blocked).then_some("step--blocked"))}>
            <span class="step__glyph" aria-hidden="true">{ glyph }</span>
            <span class="mono step__resource">{ s.resource() }</span>
            <span class="step__summary">{ secret_text(&summary) }</span>
            if s.destructive { <span class="badge badge--danger">{ icons::alert() }{ "destructive" }</span> }
            if !s.locks.is_empty() { <span class="badge" title="resource locks">{ format!("lock: {}", s.locks.join(", ")) }</span> }
            if !s.notifies.is_empty() { <span class="muted small">{ format!("→ notify {}", s.notifies.join(", ")) }</span> }
            if s.always_runs { <span class="badge badge--warn">{ "always runs" }</span> }
            { detail }
        </li>
    }
}
