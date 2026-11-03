//! Revisions (design §12): the configuration repository's history as Buckle
//! saw it, git-log style: each commit's validation status (with the errors
//! of a rejected one), which rollout used it, and the two ways to act on one:
//! start a rollout of it, or roll the fleet back to it.

use buckle_api::{RevisionStatus, RevisionSummary, RolloutSummary};
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api;
use crate::app::{Route, use_auth};
use crate::components::{
    Button, Confirm, Empty, ErrorState, Kind, Loading, PageHead, Panel, Pill, Refresh, RouteLink,
    use_toast,
};
use crate::hooks::{use_fetch, use_live_value, use_now};
use crate::logic::format::{commit, first_line, relative, rollout_label, rollout_note, timestamp};
use crate::logic::live::{apply_revisions, apply_rollouts};
use crate::logic::tone::{Tone, rollout_finished, rollout_state_label, rollout_state_tone};

#[derive(Clone, PartialEq)]
enum Ask {
    Start(RevisionSummary),
    RollBack(RevisionSummary),
}

#[function_component(Revisions)]
pub fn revisions() -> Html {
    let now = use_now(30);
    let auth = use_auth();
    let toast = use_toast();
    let navigator = use_navigator();
    let page = use_fetch((), |_| async { api::revisions(0).await.map(|p| p.items) });
    let list = use_live_value(&page, apply_revisions);
    let rollouts = use_fetch((), |_| async { api::rollouts(0).await.map(|p| p.items) });
    let rollouts = use_live_value(&rollouts, apply_rollouts).unwrap_or_default();
    let ask = use_state(|| None::<Ask>);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);

    let active: Option<&RolloutSummary> = rollouts.iter().find(|r| !rollout_finished(r.state));

    let run = {
        let (ask, busy, error, toast, navigator) = (
            ask.clone(),
            busy.clone(),
            error.clone(),
            toast.clone(),
            navigator.clone(),
        );
        Callback::from(move |note: String| {
            let Some(what) = (*ask).clone() else { return };
            let rev = match &what {
                Ask::Start(r) | Ask::RollBack(r) => r.clone(),
            };
            let note = match &what {
                Ask::RollBack(_) if note.is_empty() => {
                    format!("Roll the fleet back to {}", commit(&rev.commit))
                }
                _ => note,
            };
            let (ask, busy, error, toast, navigator) = (
                ask.clone(),
                busy.clone(),
                error.clone(),
                toast.clone(),
                navigator.clone(),
            );
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match api::start_rollout(&rev.id, &note).await {
                    Ok(r) => {
                        toast.ok(format!(
                            "Rollout {} of {} started.",
                            rollout_label(&r.id),
                            commit(&rev.commit)
                        ));
                        ask.set(None);
                        if let Some(n) = navigator {
                            n.push(&Route::rollout(&r.id));
                        }
                    }
                    Err(e) => {
                        if e.is_forbidden() {
                            toast.error(format!("Not permitted: {e}"));
                        }
                        error.set(Some(e.to_string()));
                    }
                }
                busy.set(false);
            });
        })
    };

    let stable_at = list
        .as_ref()
        .and_then(|l| l.iter().find(|r| r.is_stable).map(|r| r.created_at));
    let body = match (&list, &page.error) {
        (Some(l), _) if l.is_empty() => html! {
            <Empty title="No revisions yet" message="Revisions appear as the server's source watcher sees commits (or a webhook tells it)." />
        },
        (Some(l), _) => html! {
            <ol class="gitlog">
                { for l.iter().map(|r| {
                    let rollout = r.rollout_id.as_ref().and_then(|id| rollouts.iter().find(|x| &x.id == id));
                    let older = stable_at.is_some_and(|s| r.created_at < s);
                    let open = |a: Ask| { let (ask, error) = (ask.clone(), error.clone()); Callback::from(move |_: MouseEvent| { error.set(None); ask.set(Some(a.clone())); }) };
                    let rejected = r.status == RevisionStatus::Rejected;
                    let body_text = r.message.lines().skip(1).collect::<Vec<_>>().join("\n");
                    html! {
                        <li class={classes!("gitlog__item", rejected.then_some("gitlog__item--rejected"), r.is_stable.then_some("gitlog__item--stable"))} data-revision={r.id.clone()}>
                            <span class="gitlog__dot" aria-hidden="true" />
                            <div class="gitlog__main">
                                <div class="gitlog__line">
                                    <span class="mono gitlog__commit" title={r.commit.clone()}>{ commit(&r.commit) }</span>
                                    <span class="gitlog__subject">{ first_line(&r.message).to_string() }</span>
                                    if r.is_stable { <span class="badge badge--ok">{ "stable" }</span> }
                                    <Pill tone={if rejected { Tone::Danger } else { Tone::Ok }} label={if rejected { "rejected" } else { "valid" }} />
                                    { match rollout {
                                        Some(o) => html! { <RouteLink to={Route::rollout(&o.id)}>
                                            <Pill tone={rollout_state_tone(o.state)} label={format!("rollout {} {}", rollout_label(&o.id), rollout_state_label(o.state))} title={rollout_note(o)} />
                                        </RouteLink> },
                                        None => html! {},
                                    } }
                                </div>
                                <div class="gitlog__meta muted">
                                    <span>{ r.author.clone() }</span>
                                    <span title={timestamp(r.created_at)}>{ relative(r.created_at, now) }</span>
                                    <span>{ format!("{} modules", r.module_count) }</span>
                                </div>
                                if !body_text.trim().is_empty() { <p class="gitlog__body">{ body_text.trim().to_string() }</p> }
                                if rejected {
                                    <ul class="errors" aria-label="Validation errors">
                                        { for r.validation_errors.iter().map(|e| html! { <li class="mono">{ e.clone() }</li> }) }
                                        if r.validation_errors.is_empty() { <li>{ "Rejected without a reason recorded." }</li> }
                                    </ul>
                                }
                            </div>
                            if auth.is_admin() && !rejected && !r.is_stable {
                                <div class="gitlog__actions">
                                    if older {
                                        <Button small=true kind={Kind::Subtle} onclick={open(Ask::RollBack(r.clone()))}>{ "Roll back fleet to this…" }</Button>
                                    } else if rollout.is_none() || rollout.is_some_and(|o| rollout_finished(o.state)) {
                                        <Button small=true onclick={open(Ask::Start(r.clone()))}>{ "Start rollout…" }</Button>
                                    }
                                </div>
                            }
                        </li>
                    }
                }) }
            </ol>
        },
        (None, Some(e)) => html! { <ErrorState error={e.clone()} retry={page.reload.clone()} /> },
        (None, None) => html! { <Loading label="Loading revisions…" /> },
    };

    let dialog = ask.as_ref().map(|a| {
        let cancel = { let ask = ask.clone(); Callback::from(move |_| ask.set(None)) };
        let queued = active.map(|r| format!("Rollout {} is still running; depending on rollout.supersede the new one waits for it or replaces it.", rollout_label(&r.id)));
        match a {
            Ask::Start(r) => html! {
                <Confirm title={format!("Start a rollout of {}", commit(&r.commit))} confirm_label="Start rollout" note=true
                    busy={*busy} error={(*error).clone()} on_confirm={run.clone()} on_cancel={cancel}>
                    <p>{ format!("“{}” moves through the phases as buckle.yaml describes: plans first, then every gate, wave and bake.", first_line(&r.message)) }</p>
                    if let Some(q) = queued { <p class="note">{ q }</p> }
                </Confirm>
            },
            Ask::RollBack(r) => html! {
                <Confirm title={format!("Roll the fleet back to {}", commit(&r.commit))} confirm_label={format!("Roll back to {}", commit(&r.commit))}
                    danger=true note=true type_to_confirm={AttrValue::from(commit(&r.commit).to_string())}
                    busy={*busy} error={(*error).clone()} on_confirm={run.clone()} on_cancel={cancel}>
                    <p>{ format!("Starts a rollout of the older revision “{}”. Rolling back is rolling forward to old content: resources added since are removed, and every phase gate, approval and bake still applies.", first_line(&r.message)) }</p>
                    <p class="muted">{ "An irreversible task (a package upgrade with a schema migration) only goes back as far as its kind can; the plans will show exactly what will happen." }</p>
                    if let Some(q) = queued { <p class="note">{ q }</p> }
                </Confirm>
            },
        }
    });

    html! {
        <div class="page">
            <PageHead title="Revisions" subtitle={html! { "Commits of the configuration repository, newest first." }}
                actions={html! { <Refresh loading={page.loading} reload={page.reload.clone()} /> }} />
            <Panel>{ body }</Panel>
            { dialog.unwrap_or_default() }
        </div>
    }
}
