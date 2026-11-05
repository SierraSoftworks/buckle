//! Add a cluster: creates a cluster target that `buckle.yaml` does not
//! declare (`POST /targets`) and, for one whose executor runs inside it,
//! shows the job, policy and commands that put it there
//! (`GET /targets/{id}/executor`).

use buckle_api::{ClusterKind, ClusterSpec, ClusterTrust, CreateTarget, ExecutorSpec, TargetKind};
use yew::prelude::*;

use crate::components::ui::{Note, NoteKind};

use crate::api;
use crate::components::modal::Modal;
use crate::components::{Button, Kind, Loading, icons};

#[derive(Properties, PartialEq)]
pub struct AddClusterProps {
    pub on_close: Callback<()>,
}

/// Where the executor runs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Executor {
    /// A host of the fleet (its `executors` selector), with a Buckle token.
    Hosts,
    /// A job inside the cluster, with its workload identity.
    Inside,
}

/// The request the form describes, or why it cannot be sent.
fn request(
    id: &str,
    kind: ClusterKind,
    api: &str,
    executor: Executor,
    selector: &str,
    jwks: &str,
) -> Result<CreateTarget, String> {
    let id = id.trim();
    if id.is_empty() || id.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err("Name the cluster: letters, digits and dashes, without spaces.".into());
    }
    let api = api.trim();
    let (executors, identity, trust) = match executor {
        Executor::Hosts => {
            if selector.trim().is_empty() {
                return Err("Say which hosts may speak for the cluster.".into());
            }
            (Some(selector.trim().to_owned()), "buckle_jwt", None)
        }
        Executor::Inside => {
            if jwks.trim().is_empty() {
                return Err(
                    "Give the URL the server reads the cluster's signing keys from.".into(),
                );
            }
            let trust = ClusterTrust {
                jwks_url: Some(jwks.trim().to_owned()),
                ..Default::default()
            };
            (None, "workload", Some(Box::new(trust)))
        }
    };
    Ok(CreateTarget {
        id: id.to_owned(),
        kind: TargetKind::Cluster,
        cluster: Some(ClusterSpec {
            kind,
            description: String::new(),
            api: (!api.is_empty()).then(|| api.to_owned()),
            nodes: None,
            executors,
            identity: identity.to_owned(),
            cluster: None,
            scope: Default::default(),
            trust,
        }),
        labels: Default::default(),
        notes: String::new(),
    })
}

fn text(state: &UseStateHandle<String>) -> Callback<InputEvent> {
    let state = state.clone();
    Callback::from(move |e: InputEvent| {
        state.set(
            e.target_unchecked_into::<web_sys::HtmlInputElement>()
                .value(),
        )
    })
}

fn file_block(name: &str, contents: &str) -> Html {
    let copy = {
        let contents = contents.to_owned();
        Callback::from(move |_: MouseEvent| crate::util::copy(&contents))
    };
    html! {
        <div class="join-command">
            <div class="join-command__bar">
                <span class="mono">{ name.to_owned() }</span>
                <Button kind={Kind::Subtle} small=true label={format!("Copy {name}")} onclick={copy}>
                    { icons::copy() }{ " Copy" }
                </Button>
            </div>
            <pre class="mono">{ contents.to_owned() }</pre>
        </div>
    }
}

/// Where the scheduler serves the keys it signs workload identities with.
fn jwks_url(kind: ClusterKind, api: &str) -> String {
    let api = api.trim().trim_end_matches('/');
    match kind {
        ClusterKind::Nomad => format!("{api}/.well-known/jwks.json"),
        ClusterKind::Kubernetes => format!("{api}/openid/v1/jwks"),
    }
}

fn executor_view(kind: ClusterKind, spec: &ExecutorSpec) -> Html {
    let intro = match kind {
        ClusterKind::Nomad => format!(
            "Run these with a Nomad management token. The executor enrols as {} once the job starts.",
            spec.host_id
        ),
        ClusterKind::Kubernetes => format!(
            "Run these with cluster-admin credentials. The executor enrols as {} once its pod starts.",
            spec.host_id
        ),
    };
    html! { <>
        <p>{ intro }</p>
        { file_block(&spec.policy_file, &spec.policy) }
        { file_block(&spec.job_file, &spec.job) }
        { file_block("commands", &spec.commands.join("\n")) }
        <ul class="wizard__notes">
            { for spec.notes.iter().map(|n| html! { <li>{ n.clone() }</li> }) }
        </ul>
    </> }
}

#[function_component(AddCluster)]
pub fn add_cluster(props: &AddClusterProps) -> Html {
    let id = use_state(String::new);
    let kind = use_state(|| ClusterKind::Nomad);
    let api_url = use_state(String::new);
    let executor = use_state(|| Executor::Hosts);
    let selector = use_state(String::new);
    let jwks = use_state(String::new);
    let busy = use_state(|| false);
    let error = use_state(|| None::<String>);
    let created = use_state(|| None::<String>);
    let spec = use_state(|| None::<Result<ExecutorSpec, String>>);
    let close = {
        let on_close = props.on_close.clone();
        Callback::from(move |_: MouseEvent| on_close.emit(()))
    };

    let draft = request(&id, *kind, &api_url, *executor, &selector, &jwks);
    let create = {
        let (draft, busy, error, created, spec, executor) = (
            draft.clone(),
            busy.clone(),
            error.clone(),
            created.clone(),
            spec.clone(),
            *executor,
        );
        Callback::from(move |_: MouseEvent| {
            let Ok(body) = draft.clone() else { return };
            let (busy, error, created, spec) =
                (busy.clone(), error.clone(), created.clone(), spec.clone());
            busy.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                match api::create_target(&body).await {
                    Ok(summary) => {
                        error.set(None);
                        created.set(Some(summary.id.clone()));
                        if executor == Executor::Inside {
                            spec.set(Some(
                                api::target_executor(&summary.id)
                                    .await
                                    .map_err(|e| e.to_string()),
                            ));
                        }
                    }
                    Err(e) => error.set(Some(e.to_string())),
                }
                busy.set(false);
            });
        })
    };

    let choose = |e: Executor| {
        let (executor, jwks, api_url, kind) = (
            executor.clone(),
            jwks.clone(),
            api_url.clone(),
            kind.clone(),
        );
        Callback::from(move |_: MouseEvent| {
            if e == Executor::Inside && jwks.is_empty() && !api_url.trim().is_empty() {
                jwks.set(jwks_url(*kind, &api_url));
            }
            executor.set(e);
        })
    };
    let choose_kind = |k: ClusterKind| {
        let (kind, jwks, api_url) = (kind.clone(), jwks.clone(), api_url.clone());
        Callback::from(move |_: MouseEvent| {
            // A key URL derived for the other scheduler follows the switch.
            if !api_url.trim().is_empty() && *jwks == jwks_url(*kind, &api_url) {
                jwks.set(jwks_url(k, &api_url));
            }
            kind.set(k);
        })
    };
    let (name_hint, api_hint, executors_hint) = match *kind {
        ClusterKind::Nomad => (
            "home-nomad",
            "http://10.0.0.1:4646",
            r#"host.groups contains "nomad-servers""#,
        ),
        ClusterKind::Kubernetes => (
            "lab-k3s",
            "https://10.0.0.1:6443",
            r#"host.labels.role == "k3s-server""#,
        ),
    };

    let (body, footer) = match &*created {
        None => {
            let choice = |e: Executor, title: &'static str, sub: &'static str| {
                let selected = *executor == e;
                html! {
                    <button type="button" role="radio" aria-checked={selected.to_string()}
                        class={classes!("choice", selected.then_some("choice--selected"))} onclick={choose(e)}>
                        <span class="choice__title">{ title }</span>
                        <span class="choice__sub">{ sub }</span>
                    </button>
                }
            };
            let kind_choice = |k: ClusterKind, title: &'static str| {
                let selected = *kind == k;
                html! {
                    <button type="button" role="radio" aria-checked={selected.to_string()}
                        class={classes!("choice", selected.then_some("choice--selected"))} onclick={choose_kind(k)}>
                        <span class="choice__title">{ title }</span>
                    </button>
                }
            };
            let problem = (*error)
                .clone()
                .or_else(|| (!id.is_empty()).then(|| draft.clone().err()).flatten());
            (
                html! { <>
                    <label class="field">
                        <span class="field__label">{ "Name" }</span>
                        <input class="input mono" type="text" placeholder={name_hint} spellcheck="false"
                            value={(*id).clone()} oninput={text(&id)} />
                        <span class="field__hint">{ "A cluster buckle.yaml does not declare. Declared clusters appear here on their own." }</span>
                    </label>
                    <div class="field">
                        <span class="field__label">{ "Scheduler" }</span>
                        <div class="wizard__choices wizard__choices--two" role="radiogroup" aria-label="Scheduler">
                            { kind_choice(ClusterKind::Nomad, "Nomad") }
                            { kind_choice(ClusterKind::Kubernetes, "Kubernetes") }
                        </div>
                    </div>
                    <label class="field">
                        <span class="field__label">{ "API" }</span>
                        <input class="input mono" type="text" placeholder={api_hint} spellcheck="false"
                            value={(*api_url).clone()} oninput={text(&api_url)} />
                        <span class="field__hint">{ "As its executor reaches it. Leave blank for the executor's local default." }</span>
                    </label>
                    <div class="field">
                        <span class="field__label">{ "Executor" }</span>
                        <div class="wizard__choices wizard__choices--two" role="radiogroup" aria-label="Executor">
                            { choice(Executor::Hosts, "Hosts of the fleet", "Hosts near the cluster apply its workloads with a token Buckle issues.") }
                            { choice(Executor::Inside, "Inside the cluster", "A job or pod in the cluster applies them with its workload identity.") }
                        </div>
                    </div>
                    if *executor == Executor::Hosts {
                        <label class="field">
                            <span class="field__label">{ "Executors" }</span>
                            <input class="input mono" type="text" placeholder={executors_hint} spellcheck="false"
                                value={(*selector).clone()} oninput={text(&selector)} />
                            <span class="field__hint">{ "A selector over host.* choosing the hosts that may speak for it." }</span>
                        </label>
                    } else {
                        <label class="field">
                            <span class="field__label">{ "Signing keys" }</span>
                            <input class="input mono" type="text" placeholder={jwks_url(*kind, api_hint)} spellcheck="false"
                                value={(*jwks).clone()} oninput={text(&jwks)} />
                            <span class="field__hint">{ "Where the server reads the keys the cluster signs workload identities with." }</span>
                        </label>
                    }
                    if let Some(p) = problem { <Note kind={NoteKind::Danger}>{ p }</Note> }
                </> },
                html! { <>
                    <Button kind={Kind::Subtle} onclick={close.clone()}>{ "Cancel" }</Button>
                    <Button kind={Kind::Primary} busy={*busy} disabled={draft.is_err()} onclick={create}>{ "Add cluster" }</Button>
                </> },
            )
        }
        Some(name) => {
            let body = match (*executor, &*spec) {
                (Executor::Hosts, _) => html! {
                    <p>{ format!("{name} was added. Workloads selecting it are applied through its executors from the next rollout.") }</p>
                },
                (Executor::Inside, None) => {
                    html! { <Loading label="Rendering the executor job…" /> }
                }
                (Executor::Inside, Some(Err(e))) => {
                    html! { <Note kind={NoteKind::Danger}>{ e.clone() }</Note> }
                }
                (Executor::Inside, Some(Ok(s))) => executor_view(*kind, s),
            };
            (
                body,
                html! { <Button kind={Kind::Primary} onclick={close.clone()}>{ "Done" }</Button> },
            )
        }
    };

    html! {
        <Modal title="Add a cluster" wide=true on_close={props.on_close.clone()} {footer}>
            <div class="wizard" data-testid="add-cluster">{ body }</div>
        </Modal>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cluster_with_an_executor_inside_trusts_its_keys() {
        let r = request(
            "home-nomad",
            ClusterKind::Nomad,
            "http://n:4646",
            Executor::Inside,
            "",
            "http://n:4646/.well-known/jwks.json",
        )
        .unwrap();
        let spec = r.cluster.unwrap();
        assert_eq!(spec.identity, "workload");
        assert_eq!(
            spec.trust.and_then(|t| t.jwks_url).as_deref(),
            Some("http://n:4646/.well-known/jwks.json")
        );
    }

    #[test]
    fn a_kubernetes_cluster_reads_its_service_account_keys() {
        let jwks = jwks_url(ClusterKind::Kubernetes, "https://k:6443/");
        assert_eq!(jwks, "https://k:6443/openid/v1/jwks");
        let r = request(
            "lab",
            ClusterKind::Kubernetes,
            "https://k:6443",
            Executor::Inside,
            "",
            &jwks,
        )
        .unwrap();
        assert_eq!(r.cluster.unwrap().kind, ClusterKind::Kubernetes);
    }

    #[test]
    fn incomplete_forms_say_what_is_missing() {
        assert!(request("", ClusterKind::Nomad, "", Executor::Hosts, "true", "").is_err());
        assert!(request("a b", ClusterKind::Nomad, "", Executor::Hosts, "true", "").is_err());
        assert!(request("c", ClusterKind::Nomad, "", Executor::Hosts, " ", "").is_err());
        assert!(request("c", ClusterKind::Nomad, "", Executor::Inside, "", "").is_err());
        assert!(request("c", ClusterKind::Nomad, "", Executor::Hosts, "true", "").is_ok());
    }
}
