//! Add an agent: a three-step wizard on the hosts page that turns a platform,
//! a connection method, a name and labels into the one command that
//! bootstraps a new host (install Buckle, join the tailnet, enrol).
//!
//! With Tailscale provisioning through an OAuth app, the server first sends
//! the admin to Tailscale to consent to the node's auth key; Tailscale
//! returns to `/hosts?join=<id>`, which reopens the wizard on the finished
//! command. Through an OAuth client the server mints the key itself.

use buckle_api::{HostSummary, JoinMethod, JoinOptions, JoinPlatform, JoinState, JoinTicket};
use yew::prelude::*;

use crate::components::ui::{Note, NoteKind};
use yew_router::prelude::*;

use crate::api::{self, ApiError};
use crate::app::Route;
use crate::components::modal::Modal;
use crate::components::{Button, Countdown, ErrorState, Kind, Loading, RouteLink, icons};
use crate::hooks::use_fetch;
use crate::logic::join::{
    default_method, enrolled, needs_consent, notes, provisions_tailnet, request,
};
use crate::logic::labels::Row;

/// The ticket id Tailscale returns with when consent could not be finished.
pub const EXPIRED: &str = "expired";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Platform,
    Details,
    Command,
}

#[derive(Properties, PartialEq)]
pub struct AddAgentProps {
    pub on_close: Callback<()>,
    /// The fleet as it is now, to notice the new host arriving.
    pub hosts: Vec<HostSummary>,
    /// A ticket to reopen on (the return from Tailscale).
    #[prop_or_default]
    pub resume: Option<String>,
}

fn platform_blurb(p: JoinPlatform) -> &'static str {
    match p {
        JoinPlatform::Linux => "systemd service · amd64 or arm64",
        JoinPlatform::Macos => "launchd daemon · Intel or Apple silicon",
        JoinPlatform::Windows => "Windows service · x86_64",
    }
}

#[function_component(AddAgent)]
pub fn add_agent(props: &AddAgentProps) -> Html {
    let navigator = use_navigator();
    let options = use_fetch((), |_| api::join_options());
    let step = use_state(|| {
        if props.resume.is_some() {
            Step::Command
        } else {
            Step::Platform
        }
    });
    let platform = use_state(|| None::<JoinPlatform>);
    let method = use_state(|| None::<JoinMethod>);
    let host = use_state(String::new);
    let labels = use_state(|| vec![Row::default()]);
    let tags = use_state(|| None::<Vec<String>>);
    let ticket = use_state(|| None::<JoinTicket>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    // Back from Tailscale: load the ticket it finished.
    {
        let (ticket, error, platform, step) = (
            ticket.clone(),
            error.clone(),
            platform.clone(),
            step.clone(),
        );
        use_effect_with(props.resume.clone(), move |resume| {
            if resume.is_some() {
                step.set(Step::Command);
            }
            match resume.as_deref() {
                Some(EXPIRED) => error.set(Some(
                    "The Tailscale sign-in expired or was already used. Start again to make a new command.".into(),
                )),
                Some(id) => {
                    let id = id.to_owned();
                    wasm_bindgen_futures::spawn_local(async move {
                        match api::join_ticket(&id).await {
                            Ok(t) => {
                                platform.set(Some(t.platform));
                                ticket.set(Some(t));
                            }
                            Err(ApiError::NotFound(_)) => error.set(Some(
                                "That join command has expired. Start again to make a new one.".into(),
                            )),
                            Err(e) => error.set(Some(e.to_string())),
                        }
                    });
                }
                None => {}
            }
        });
    }

    let opts: Option<&JoinOptions> = options.value();
    let method_now = opts.map(|o| (*method).unwrap_or_else(|| default_method(o)));
    let provisioning = opts
        .zip(method_now)
        .is_some_and(|(o, m)| provisions_tailnet(o, m));
    let consent = opts
        .zip(method_now)
        .is_some_and(|(o, m)| needs_consent(o, m));
    let tags_now: Vec<String> = match (&*tags, opts.and_then(|o| o.tailscale.as_ref())) {
        (Some(t), _) => t.clone(),
        (None, Some(ts)) => ts.tags.first().cloned().into_iter().collect(),
        (None, None) => vec![],
    };
    let draft = (*platform).zip(method_now).map(|(p, m)| {
        request(
            p,
            m,
            &host,
            &labels,
            if provisioning { &tags_now } else { &[] },
        )
    });

    let go = |to: Step| {
        let (step, error) = (step.clone(), error.clone());
        Callback::from(move |_: MouseEvent| {
            error.set(None);
            step.set(to);
        })
    };

    let restart = {
        let (step, ticket, error, navigator) = (
            step.clone(),
            ticket.clone(),
            error.clone(),
            navigator.clone(),
        );
        Callback::from(move |_: MouseEvent| {
            ticket.set(None);
            error.set(None);
            step.set(Step::Details);
            // Drop `?join=` so a reload does not reopen the old ticket.
            if let Some(n) = &navigator {
                n.replace(&Route::Hosts);
            }
        })
    };

    let create = {
        let (ticket, busy, error, step, navigator) = (
            ticket.clone(),
            busy.clone(),
            error.clone(),
            step.clone(),
            navigator.clone(),
        );
        let draft = draft.clone();
        Callback::from(move |_: MouseEvent| {
            let Some(Ok(req)) = draft.clone() else {
                return;
            };
            let (ticket, busy, error, step, navigator) = (
                ticket.clone(),
                busy.clone(),
                error.clone(),
                step.clone(),
                navigator.clone(),
            );
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match api::create_join(&req).await {
                    Ok(t) => match &t.state {
                        JoinState::Authorize { authorize_url } => {
                            // Same-origin URLs stay in the app; Tailscale's
                            // consent screen is a page of its own.
                            if authorize_url.starts_with('/') {
                                if let (Some(n), Some(id)) =
                                    (&navigator, authorize_url.split("join=").nth(1))
                                {
                                    let _ = n
                                        .push_with_query(&Route::Hosts, &[("join", id.to_owned())]);
                                }
                            } else if let Some(w) = web_sys::window() {
                                let _ = w.location().set_href(authorize_url);
                            }
                        }
                        _ => {
                            ticket.set(Some(t));
                            step.set(Step::Command);
                            busy.set(false);
                        }
                    },
                    Err(e) => {
                        error.set(Some(e.to_string()));
                        busy.set(false);
                    }
                }
            });
        })
    };

    let close = {
        let on_close = props.on_close.clone();
        Callback::from(move |_: MouseEvent| on_close.emit(()))
    };

    let steps = [
        (Step::Platform, "Platform"),
        (Step::Details, "Details"),
        (Step::Command, "Command"),
    ];
    let stepper = html! {
        <ol class="wizard__steps" aria-label="Steps">
            { for steps.iter().enumerate().map(|(i, (s, label))| {
                let current = *s == *step;
                html! {
                    <li class={classes!("wizard__step", current.then_some("wizard__step--current"))}
                        aria-current={current.then_some("step")}>
                        <span class="wizard__num">{ i + 1 }</span>{ *label }
                    </li>
                }
            }) }
        </ol>
    };

    let (body, footer) = match (*step, opts, &options.error) {
        (Step::Platform | Step::Details, None, Some(e)) => (
            html! { <ErrorState error={e.clone()} retry={options.reload.clone()} /> },
            html! { <Button onclick={close.clone()}>{ "Close" }</Button> },
        ),
        (Step::Platform | Step::Details, None, None) => (
            html! { <Loading label="Loading what this server accepts…" /> },
            html! { <Button onclick={close.clone()}>{ "Cancel" }</Button> },
        ),
        (Step::Platform, Some(_), _) => (
            html! {
                <div class="wizard__choices" role="radiogroup" aria-label="Platform">
                    { for JoinPlatform::ALL.iter().map(|p| {
                        let selected = *platform == Some(*p);
                        let onclick = { let platform = platform.clone(); let p = *p; Callback::from(move |_| platform.set(Some(p))) };
                        html! {
                            <button type="button" role="radio" aria-checked={selected.to_string()}
                                class={classes!("choice", selected.then_some("choice--selected"))} {onclick}>
                                <span class="choice__title">{ p.label() }</span>
                                <span class="choice__sub">{ platform_blurb(*p) }</span>
                            </button>
                        }
                    }) }
                </div>
            },
            html! { <>
                <Button kind={Kind::Subtle} onclick={close.clone()}>{ "Cancel" }</Button>
                <Button kind={Kind::Primary} disabled={platform.is_none()} onclick={go(Step::Details)}>{ "Next" }</Button>
            </> },
        ),
        (Step::Details, Some(o), _) => {
            let method_choice = (o.methods.len() > 1).then(|| html! {
                <div class="field">
                    <span class="field__label">{ "Connect" }</span>
                    <div class="wizard__choices wizard__choices--two" role="radiogroup" aria-label="Connect">
                        { for o.methods.iter().map(|m| {
                            let selected = method_now == Some(*m);
                            let onclick = { let method = method.clone(); let m = *m; Callback::from(move |_| method.set(Some(m))) };
                            let (title, sub) = match (m, o.tailscale.as_ref().map(|t| t.consent)) {
                                (JoinMethod::Tailscale, Some(true)) => ("Over Tailscale", "Joins the tailnet with a single-use key you approve in Tailscale."),
                                (JoinMethod::Tailscale, Some(false)) => ("Over Tailscale", "Joins the tailnet with a single-use, tagged key."),
                                (JoinMethod::Tailscale, None) => ("Over Tailscale", "For a host already on the tailnet."),
                                (JoinMethod::Token, _) => ("With a join token", "For a host that reaches the server directly."),
                            };
                            html! {
                                <button type="button" role="radio" aria-checked={selected.to_string()}
                                    class={classes!("choice", selected.then_some("choice--selected"))} {onclick}>
                                    <span class="choice__title">{ title }</span>
                                    <span class="choice__sub">{ sub }</span>
                                </button>
                            }
                        }) }
                    </div>
                </div>
            });
            let tag_choice = o.tailscale.as_ref().filter(|_| provisioning).map(|ts| html! {
                <div class="field">
                    <span class="field__label">{ "Tailscale tags" }</span>
                    <div class="chips" role="group" aria-label="Tailscale tags">
                        { for ts.tags.iter().map(|t| {
                            let on = tags_now.contains(t);
                            let onclick = {
                                let (tags, now, t) = (tags.clone(), tags_now.clone(), t.clone());
                                Callback::from(move |_| {
                                    let mut next = now.clone();
                                    if on { next.retain(|x| x != &t) } else { next.push(t.clone()) }
                                    tags.set(Some(next));
                                })
                            };
                            html! {
                                <button type="button" class={classes!("chip", on.then_some("chip--active"))}
                                    aria-pressed={on.to_string()} {onclick}>if on { { icons::check() } }{ t.clone() }</button>
                            }
                        }) }
                    </div>
                    <span class="field__hint">{ if ts.consent {
                        "The node advertises these, so the server admits it. You must own them in the tailnet policy."
                    } else {
                        "The node's key carries these, so the server admits it."
                    } }</span>
                </div>
            });
            let edit = |i: usize, key: bool| {
                let labels = labels.clone();
                Callback::from(move |e: InputEvent| {
                    let value = e
                        .target_unchecked_into::<web_sys::HtmlInputElement>()
                        .value();
                    let mut rows = (*labels).clone();
                    if let Some(r) = rows.get_mut(i) {
                        if key { r.key = value } else { r.value = value }
                    }
                    labels.set(rows);
                })
            };
            let remove = |i: usize| {
                let labels = labels.clone();
                Callback::from(move |_: MouseEvent| {
                    let mut rows = (*labels).clone();
                    rows.remove(i);
                    labels.set(rows);
                })
            };
            let add = {
                let labels = labels.clone();
                Callback::from(move |_: MouseEvent| {
                    let mut rows = (*labels).clone();
                    rows.push(Row::default());
                    labels.set(rows);
                })
            };
            let on_host = {
                let host = host.clone();
                Callback::from(move |e: InputEvent| {
                    host.set(
                        e.target_unchecked_into::<web_sys::HtmlInputElement>()
                            .value(),
                    )
                })
            };
            let problem = match &draft {
                Some(Err(e)) => Some(e.clone()),
                _ => (*error).clone(),
            };
            let primary = if consent {
                "Continue in Tailscale"
            } else {
                "Create command"
            };
            (
                html! { <>
                    { method_choice }
                    <label class="field">
                        <span class="field__label">{ "Host name" }</span>
                        <input class="input mono" type="text" placeholder="its hostname" spellcheck="false"
                            value={(*host).clone()} oninput={on_host} />
                        <span class="field__hint">{ if method_now == Some(JoinMethod::Tailscale) {
                            "Its tailnet node name. Leave blank to use the host's hostname."
                        } else {
                            "Leave blank to use the host's hostname. A name binds the command to that host."
                        } }</span>
                    </label>
                    { tag_choice }
                    <div class="field">
                        <span class="field__label">{ "Labels" }</span>
                        <div class="kv-rows" role="group" aria-label="Labels">
                            { for labels.iter().enumerate().map(|(i, row)| html! {
                                <div class="kv-row">
                                    <input class="input mono" type="text" placeholder="key" aria-label={format!("Label {} key", i + 1)}
                                        value={row.key.clone()} oninput={edit(i, true)} spellcheck="false" />
                                    <span aria-hidden="true">{ "=" }</span>
                                    <input class="input mono" type="text" placeholder="value" aria-label={format!("Label {} value", i + 1)}
                                        value={row.value.clone()} oninput={edit(i, false)} spellcheck="false" />
                                    <button type="button" class="icon-btn" aria-label={format!("Remove label {}", i + 1)}
                                        onclick={remove(i)}>{ icons::close() }</button>
                                </div>
                            }) }
                            <Button small=true kind={Kind::Subtle} onclick={add}>{ icons::plus() }{ "Add label" }</Button>
                        </div>
                        <span class="field__hint">{ "The agent's own labels: they decide its groups and rollout phase." }</span>
                    </div>
                    if let Some(p) = problem { <Note kind={NoteKind::Danger}>{ p }</Note> }
                </> },
                html! { <>
                    <Button kind={Kind::Subtle} onclick={go(Step::Platform)}>{ "Back" }</Button>
                    <Button kind={Kind::Primary} busy={*busy} disabled={!matches!(draft, Some(Ok(_)))} onclick={create.clone()}>
                        { primary }
                        if consent { { icons::external() } }
                    </Button>
                </> },
            )
        }
        (Step::Command, _, _) => {
            let body = match (&*ticket, &*error) {
                (_, Some(e)) => {
                    html! { <Note kind={NoteKind::Danger}>{ e.clone() }</Note> }
                }
                (None, None) => html! { <Loading label="Loading the join command…" /> },
                (Some(t), None) => command_view(t, opts, &props.hosts),
            };
            (
                body,
                html! { <>
                    <Button kind={Kind::Subtle} onclick={restart}>{ "Start again" }</Button>
                    <Button kind={Kind::Primary} onclick={close.clone()}>{ "Done" }</Button>
                </> },
            )
        }
    };

    html! {
        <Modal title="Add an agent" wide=true on_close={props.on_close.clone()} {footer}>
            <div class="wizard" data-testid="add-agent">
                { stepper }
                { body }
            </div>
        </Modal>
    }
}

fn command_view(t: &JoinTicket, options: Option<&JoinOptions>, hosts: &[HostSummary]) -> Html {
    match &t.state {
        JoinState::Failed { reason } => html! {
            <Note kind={NoteKind::Danger}>{ reason.clone() }</Note>
        },
        JoinState::Authorize { .. } => html! { <Loading label="Waiting for Tailscale…" /> },
        JoinState::Ready { command } => {
            let copy = {
                let command = command.clone();
                Callback::from(move |_: MouseEvent| crate::util::copy(&command))
            };
            let arrived = enrolled(hosts, t);
            let who = t.host_id.clone().unwrap_or_else(|| "the new host".into());
            let shell = match t.platform {
                JoinPlatform::Windows => "PowerShell (as administrator)",
                _ => "Shell",
            };
            html! { <>
                <p>{ format!("Run this on {who} ({}).", t.platform.label()) }</p>
                <div class="join-command">
                    <div class="join-command__bar">
                        <span>{ shell }</span>
                        <Button kind={Kind::Subtle} small=true label="Copy the command" onclick={copy}>
                            { icons::copy() }{ " Copy" }
                        </Button>
                    </div>
                    <pre class="mono" data-testid="join-command">{ command.clone() }</pre>
                </div>
                <ul class="wizard__notes">
                    { for options.map(|o| notes(t.platform, t.method, o)).unwrap_or_default().into_iter().map(|n| html! { <li>{ n }</li> }) }
                    <li>{ "Its keys stop working " }<Countdown until={t.expires_at} prefix="in" done="now: start again for a new command" />{ "." }</li>
                </ul>
                <p class="wizard__arrival" role="status" data-testid="join-arrival">
                    { match arrived {
                        Some(h) => html! { <>
                            <span class="text-ok">{ icons::check() }</span>{ " " }
                            <RouteLink to={Route::Host { id: h.id.clone() }} classes="mono">{ h.id.clone() }</RouteLink>
                            { " enrolled." }
                        </> },
                        None => html! { <span class="muted">{ format!("Waiting for {who} to connect…") }</span> },
                    } }
                </p>
            </> }
        }
    }
}
