//! Reference (design §12): the task kinds, fact providers and health checks
//! registered in the running binary, rendered from the JSON schemas
//! `GET /catalog` serves, so the documentation cannot drift from the code.

use buckle_api::KindDescriptor;
use web_sys::HtmlInputElement;
use yew::prelude::*;

use crate::api;
use crate::components::{Empty, ErrorState, Loading, PageHead, Panel, icons};
use crate::hooks::use_fetch;
use crate::logic::schema::{self, Field};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Tasks,
    Facts,
    Health,
}

#[function_component(Reference)]
pub fn reference() -> Html {
    let catalog = use_fetch((), |_| api::catalog());
    let tab = use_state(|| Tab::Tasks);
    let search = use_state(String::new);

    let body = match (catalog.value(), &catalog.error) {
        (None, Some(e)) => {
            html! { <ErrorState error={e.clone()} retry={catalog.reload.clone()} /> }
        }
        (None, None) => html! { <Loading label="Reading the catalog…" /> },
        (Some(c), _) => {
            let kinds = match *tab {
                Tab::Tasks => &c.tasks,
                Tab::Facts => &c.facts,
                Tab::Health => &c.health,
            };
            let q = search.trim().to_lowercase();
            let shown: Vec<&KindDescriptor> = kinds
                .iter()
                .filter(|k| {
                    q.is_empty()
                        || k.kind.to_lowercase().contains(&q)
                        || k.description.to_lowercase().contains(&q)
                })
                .collect();
            let tab_button = |t: Tab, label: &str, n: usize| {
                let tab = tab.clone();
                let selected = *tab == t;
                html! {
                    <button type="button" role="tab" aria-selected={selected.to_string()}
                        class={classes!("tabs__tab", selected.then_some("tabs__tab--active"))}
                        onclick={Callback::from(move |_| tab.set(t))}>
                        { label.to_string() }<span class="chip__count">{ n }</span>
                    </button>
                }
            };
            let oninput = {
                let search = search.clone();
                Callback::from(move |e: InputEvent| {
                    search.set(e.target_unchecked_into::<HtmlInputElement>().value())
                })
            };
            html! { <>
                <div class="toolbar">
                    <div class="tabs" role="tablist" aria-label="Registries">
                        { tab_button(Tab::Tasks, "Task kinds", c.tasks.len()) }
                        { tab_button(Tab::Facts, "Fact providers", c.facts.len()) }
                        { tab_button(Tab::Health, "Health checks", c.health.len()) }
                    </div>
                    <input class="input toolbar__search" type="search" placeholder="Search kinds" aria-label="Search kinds"
                        value={(*search).clone()} {oninput} />
                </div>
                <p class="muted small">{ format!("From the server's registries, version {}.", c.server_version) }</p>
                if shown.is_empty() {
                    <Empty title="No kinds match" />
                }
                <nav class="toc" aria-label="Kinds">
                    { for shown.iter().map(|k| html! { <a class="toc__item mono" href={format!("#kind-{}", k.kind)}>{ k.kind.clone() }</a> }) }
                </nav>
                { for shown.into_iter().map(kind) }
            </> }
        }
    };

    html! {
        <div class="page">
            <PageHead title="Reference" subtitle={html! { "What this Buckle binary can do, from its own schemas." }} />
            { body }
        </div>
    }
}

fn kind(k: &KindDescriptor) -> Html {
    let fields = schema::fields(&k.schema);
    let table = if k.schema.is_null() {
        html! { <p class="muted">{ "Takes no configuration; its facts are listed under the host's facts tree." }</p> }
    } else if fields.is_empty() {
        html! { <p class="muted">{ format!("Takes {} (free-form).", schema::describe(&k.schema)) }</p> }
    } else {
        html! {
            <div class="table-wrap">
                <table class="table table--compact table--schema">
                    <thead><tr>
                        <th scope="col">{ "Field" }</th><th scope="col">{ "Type" }</th>
                        <th scope="col">{ "Default" }</th><th scope="col">{ "Description" }</th>
                    </tr></thead>
                    <tbody>{ for fields.iter().map(field) }</tbody>
                </table>
            </div>
        }
    };
    html! {
        <Panel id={format!("kind-{}", k.kind)} class="kind">
            <header class="kind__head">
                <h2 class="kind__name mono">{ k.kind.clone() }</h2>
                <span class="kind__platforms">{ for k.platforms.iter().map(|p| html! { <span class="badge">{ p.clone() }</span> }) }</span>
            </header>
            <p class="kind__desc">{ k.description.clone() }</p>
            if let Some(d) = schema::description(&k.schema).filter(|d| *d != k.description) {
                <p class="muted">{ d }</p>
            }
            <div class="kind__facts">
                if !k.locks.is_empty() { <span class="badge" title="serialised with other tasks holding the same lock">{ format!("lock: {}", k.locks.join(", ")) }</span> }
                if let Some(r) = &k.removal_default { <span class="badge">{ format!("removal: {r}") }</span> }
                if k.removal_caution {
                    <span class="badge badge--warn" title="Removal can take unrelated things down; plans flag it.">{ icons::alert() }{ "removal needs care" }</span>
                }
            </div>
            { table }
        </Panel>
    }
}

fn field(f: &Field) -> Html {
    html! {
        <tr class={classes!(f.secret.then_some("is-secret"))}>
            <td class="mono" style={format!("padding-left: calc(0.6rem + {}rem)", f.depth)}>
                { f.path.clone() }
                if f.required { <span class="req" title="required">{ " *" }</span> }
                if f.secret {
                    <span class="badge badge--secret" title="May reference secrets ({{ secrets.NAME }}); shown redacted as «secret:NAME@fp» everywhere.">
                        { icons::key() }{ "secret" }
                    </span>
                }
            </td>
            <td class="mono">
                { f.ty.clone() }
                if !f.options.is_empty() { <span class="cell-sub">{ f.options.join(" | ") }</span> }
            </td>
            <td class="mono">{ f.default.clone().unwrap_or_else(|| "—".into()) }</td>
            <td>{ f.description.clone().unwrap_or_default() }</td>
        </tr>
    }
}
