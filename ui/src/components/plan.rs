//! One distinct plan, as a reviewer reads it (design §11): the hosts that
//! share it, what changes module by module with unified diffs, destructive
//! steps and removals called out, deferred modules with their reason.

use buckle_api::PlanGroup;
use yew::prelude::*;

use super::diff::{DiffView, secret_text};
use super::icons;
use crate::logic::format::{count, short};
use crate::logic::plan::{Diff, PlanDoc, Planned, StepDoc, destructive_step, review};

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
    let doc = PlanDoc::read(&g.plan);
    let r = doc.as_ref().map(review);
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
                { match &doc {
                    Some(doc) => body(doc),
                    None => html! { <p class="muted">{ "The plan document is not available; the counts above come from the server's summary." }</p> },
                } }
            </div>
        </details>
    }
}

fn body(doc: &PlanDoc) -> Html {
    let warnings = (!doc.warnings.is_empty()).then(|| html! {
        <ul class="plan__warnings">
            { for doc.warnings.iter().map(|w| html! { <li>{ icons::alert() }{ secret_text(w) }</li> }) }
        </ul>
    });
    let modules = doc.modules.iter().map(|m| {
        let changed: Vec<&StepDoc> = m.steps.iter().filter(|s| !matches!(s.planned, Planned::NoOp)).collect();
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
                            <span class="mono">{ format!("handler {}:{}", h.module, h.handler) }</span>
                            <span class="step__summary">{ h.summary.clone() }</span>
                        </li>
                    }) }
                </ul>
            </section>
        }
    });
    let removals = (!doc.removals.is_empty()).then(|| html! {
        <section class="plan-module plan-module--removals">
            <header class="plan-module__head">
                <h4 class="plan-module__name">{ "Removals" }</h4>
                <span class="muted">{ "resources the revision no longer declares" }</span>
            </header>
            <ul class="steps">
                { for doc.removals.iter().map(|r| html! {
                    <li class={classes!("step", if r.runs() { "step--destructive" } else { "step--forget" })}>
                        <span class="step__glyph" aria-hidden="true">{ "−" }</span>
                        <span class="mono step__resource">{ r.resource.to_string() }</span>
                        <span class="step__summary">{ secret_text(&r.summary) }</span>
                        if r.runs() { <span class="badge badge--danger">{ "removal" }</span> }
                        else { <span class="badge">{ "forget only" }</span> }
                        if r.caution && r.runs() {
                            <span class="badge badge--warn" title="This kind's removal can take unrelated things down (dependent packages, files owned by a user).">{ icons::alert() }{ "caution" }</span>
                        }
                    </li>
                }) }
            </ul>
        </section>
    });
    let deferred = (!doc.deferred.is_empty()).then(|| html! {
        <section class="plan-module">
            <header class="plan-module__head"><h4 class="plan-module__name">{ "Deferred" }</h4></header>
            <ul class="steps">
                { for doc.deferred.iter().map(|d| html! {
                    <li class="step step--deferred">
                        <span class="step__glyph" aria-hidden="true">{ "…" }</span>
                        <span class="mono">{ d.module.clone() }</span>
                        <span class="step__summary">{ d.reason.clone() }</span>
                    </li>
                }) }
            </ul>
            <p class="muted small">{ "If a deferred module's plan turns out destructive, the agent stops before it and asks for approval again." }</p>
        </section>
    });
    html! { <>
        { warnings.unwrap_or_default() }
        { for modules }
        { removals.unwrap_or_default() }
        { deferred.unwrap_or_default() }
        if !doc.skipped.is_empty() {
            <p class="muted small">{ format!("Not selected: {}", doc.skipped.join("; ")) }</p>
        }
    </> }
}

fn step(s: &StepDoc) -> Html {
    let destructive = destructive_step(s);
    let (glyph, summary, detail) = match &s.planned {
        Planned::Change { diff } => {
            let glyph = match diff {
                Diff::Create { .. } => "+",
                Diff::Delete { .. } => "−",
                _ => "~",
            };
            let detail = match diff {
                Diff::Update {
                    unified: Some(u), ..
                } => html! { <DiffView unified={u.clone()} /> },
                Diff::Create {
                    preview: Some(p), ..
                } => html! { <pre class="step__preview">{ secret_text(p) }</pre> },
                _ => html! {},
            };
            (glyph, diff.summary().to_string(), detail)
        }
        Planned::Blocked { reason } => ("!", format!("blocked: {reason}"), html! {}),
        Planned::NoOp | Planned::Unknown => ("✓", "no change".into(), html! {}),
    };
    html! {
        <li class={classes!("step", destructive.then_some("step--destructive"), matches!(s.planned, Planned::Blocked { .. }).then_some("step--blocked"))}>
            <span class="step__glyph" aria-hidden="true">{ glyph }</span>
            <span class="mono step__resource">{ format!("{}[{}]", s.resource.kind, s.resource.key) }</span>
            <span class="step__summary">{ secret_text(&summary) }</span>
            if destructive { <span class="badge badge--danger">{ icons::alert() }{ "destructive" }</span> }
            if !s.locks.is_empty() { <span class="badge" title="resource locks">{ format!("lock: {}", s.locks.join(", ")) }</span> }
            if !s.notifies.is_empty() { <span class="muted small">{ format!("→ notify {}", s.notifies.join(", ")) }</span> }
            if s.always_runs { <span class="badge badge--warn">{ "always runs" }</span> }
            { detail }
        </li>
    }
}
