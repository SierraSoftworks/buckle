//! A host's blueprint (design §12): what Buckle manages there, module by
//! module, drawn like the run report. Each module collapses to its worst
//! state, resource count and how many resources removal would clean up;
//! modules with a problem open by default.

use buckle_api::{ActionInfo, DriftItem, ResourceSummary, RunReport};
use chrono::{DateTime, Utc};
use yew::prelude::*;

use super::actions::ActionsButton;
use super::diff::secret_text;
use super::icons;
use super::ui::{Empty, Pill};
use crate::hooks::use_now;
use crate::logic::blueprint::{Module, Resource, blueprint};
use crate::logic::format::{commit, count, relative, timestamp};

#[derive(Properties, PartialEq)]
pub struct BlueprintProps {
    pub resources: Vec<ResourceSummary>,
    #[prop_or_default]
    pub drift: Vec<DriftItem>,
    #[prop_or_default]
    pub run: Option<RunReport>,
    /// The target's actions; each module head offers its own.
    #[prop_or_default]
    pub actions: Vec<ActionInfo>,
    /// Runs (or asks to confirm) an action; `None` hides them.
    #[prop_or_default]
    pub on_action: Option<Callback<ActionInfo>>,
}

#[function_component(BlueprintView)]
pub fn blueprint_view(props: &BlueprintProps) -> Html {
    let now = use_now(30);
    // What the reader opened or closed; the rest follows the defaults.
    let overrides = use_state_eq(Vec::<(String, bool)>::new);
    let modules = blueprint(&props.resources, &props.drift, props.run.as_ref());
    if modules.is_empty() {
        return html! { <Empty title="Nothing managed yet" message="Resources appear once a revision has been applied." /> };
    }
    let is_open = |m: &Module| {
        overrides
            .iter()
            .find(|(name, _)| *name == m.name)
            .map(|(_, open)| *open)
            .unwrap_or_else(|| m.state().is_problem())
    };
    let set_all = |open: bool| {
        let overrides = overrides.clone();
        let names: Vec<String> = modules.iter().map(|m| m.name.clone()).collect();
        Callback::from(move |_: MouseEvent| {
            overrides.set(names.iter().map(|n| (n.clone(), open)).collect())
        })
    };
    html! {
        <div class="blueprint">
            <div class="run__toolbar">
                <span class="muted small">{ format!(
                    "{} · {}",
                    count(modules.len(), "module", "modules"),
                    count(modules.iter().map(|m| m.resources.len()).sum(), "resource", "resources"),
                ) }</span>
                <div class="run__expand">
                    <button type="button" class="btn btn--subtle btn--small" onclick={set_all(true)}>{ "Expand all" }</button>
                    <button type="button" class="btn btn--subtle btn--small" onclick={set_all(false)}>{ "Collapse all" }</button>
                </div>
            </div>
            <ol class="run-modules" aria-label="Modules of the blueprint">
                { for modules.iter().map(|m| {
                    let open = is_open(m);
                    let toggle = {
                        let (overrides, name) = (overrides.clone(), m.name.clone());
                        Callback::from(move |_: MouseEvent| {
                            let mut next: Vec<(String, bool)> =
                                (*overrides).iter().filter(|(n, _)| *n != name).cloned().collect();
                            next.push((name.clone(), !open));
                            overrides.set(next);
                        })
                    };
                    let actions = props.on_action.as_ref().map(|on_run| html! {
                        <ActionsButton actions={crate::logic::actions::of_module(&props.actions, &m.name)}
                            on_run={on_run.clone()} testid={format!("actions-{}", m.name)} />
                    });
                    module_row(m, open, toggle, now, actions)
                }) }
            </ol>
        </div>
    }
}

fn module_row(
    m: &Module,
    open: bool,
    toggle: Callback<MouseEvent>,
    now: DateTime<Utc>,
    actions: Option<Html>,
) -> Html {
    let state = m.state();
    let body_id = format!(
        "blueprint-{}",
        m.name.replace(|c: char| !c.is_ascii_alphanumeric(), "-")
    );
    let cleanup = m.cleanup();
    html! {
        <li class={classes!("run-module", format!("run-module--{}", state.slug()), open.then_some("is-open"))}
            data-module={m.name.clone()} data-state={state.slug()}>
            <div class="run-module__head" onclick={toggle}>
                <button type="button" class="run-module__row" aria-expanded={open.to_string()}
                    aria-controls={body_id.clone()}>
                    <span class="run-module__chevron" aria-hidden="true">{ icons::chevron() }</span>
                    <Pill tone={state.tone()} label={state.label()} />
                    <span class="run-module__title">
                        <span class="run-module__name mono">{ m.name.clone() }</span>
                        <span class="run-module__note">{ count(m.resources.len(), "resource", "resources") }</span>
                    </span>
                </button>
                <span class="run-module__cleanup muted small" title="Resources deleted from the host if the revision drops them">
                    { format!("{cleanup} tagged for cleanup") }
                </span>
                { actions.unwrap_or_default() }
            </div>
            if open {
                <div class="run-module__body" id={body_id}>
                    <ol class="run-steps" aria-label={format!("Resources of {}", m.name)}>
                        { for m.resources.iter().map(|r| resource_row(r, now)) }
                    </ol>
                </div>
            }
        </li>
    }
}

fn resource_row(r: &Resource, now: DateTime<Utc>) -> Html {
    let slug = r.state.slug();
    html! {
        <li class={classes!("run-step", format!("run-step--{slug}"))}
            data-resource={format!("{}[{}]", r.kind, r.key)} data-state={slug}>
            <span class={classes!("run-step__glyph", r.state.tone().class())} aria-hidden="true">{ r.state.glyph() }</span>
            <div class="run-step__main">
                <div class="run-step__head">
                    <span class="badge run-step__kind">{ r.kind.clone() }</span>
                    <span class="run-step__key mono">{ r.key.clone() }</span>
                    if r.cleanup {
                        <span class="badge badge--warn" title="Deleted from the host if the revision drops it">{ "cleanup" }</span>
                    } else {
                        <span class="badge" title="Left on the host, only forgotten, if the revision drops it">{ "forget only" }</span>
                    }
                    <span class="run-step__state">{ r.state.label() }</span>
                </div>
                if let Some(note) = &r.note { <p class="run-step__summary">{ secret_text(note) }</p> }
            </div>
            <span class="run-step__duration">
                if let Some((rev, at)) = &r.applied {
                    <span class="mono" title={rev.clone()}>{ commit(rev) }</span>{ " · " }
                    <span title={timestamp(*at)}>{ relative(*at, now) }</span>
                }
            </span>
        </li>
    }
}
