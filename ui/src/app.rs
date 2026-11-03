//! The application root: routes, the session gate, and the contexts every
//! page reads (who is signed in, the live store, toasts).

use buckle_api::Me;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;
use yew_router::prelude::*;

use crate::api::{self, ApiError};
use crate::components::shell::Shell;
use crate::components::{Loading, ToastHost};
use crate::pages;
use crate::store::LiveProvider;

#[derive(Clone, Routable, PartialEq, Debug)]
pub enum Route {
    #[at("/")]
    Overview,
    #[at("/rollouts")]
    Rollouts,
    #[at("/rollouts/:id")]
    Rollout { id: String },
    #[at("/hosts")]
    Hosts,
    #[at("/hosts/:id")]
    Host { id: String },
    #[at("/revisions")]
    Revisions,
    #[at("/audit")]
    Audit,
    #[not_found]
    #[at("/404")]
    NotFound,
}

impl Route {
    /// A rollout's page, addressed by its `word.word.word` name where it has
    /// one (the API resolves names) and by its id otherwise.
    pub fn rollout(id: &str) -> Route {
        Route::Rollout {
            id: buckle_api::rollout_name(id).unwrap_or_else(|| id.to_string()),
        }
    }
}

/// Who is using the UI, as `/me` said.
#[derive(Clone, PartialEq, Debug)]
pub enum Session {
    Loading,
    SignedIn(Me),
    /// The server has no sign-in configured. `admin` is whether its
    /// `admin_acl` (absent: everyone) admits this request's metadata.
    Open {
        admin: bool,
    },
    /// On the way to the sign-in page.
    Redirecting,
    Forbidden(String),
    Error(String),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Auth {
    pub session: Session,
}

impl Auth {
    /// Whether to offer administrative actions. Hiding them is a courtesy:
    /// the server enforces `admin_acl` regardless, and a refusal is shown.
    pub fn is_admin(&self) -> bool {
        match &self.session {
            Session::SignedIn(me) => me.is_admin,
            Session::Open { admin } => *admin,
            _ => false,
        }
    }
}

#[hook]
pub fn use_auth() -> Auth {
    use_context::<Auth>().unwrap_or(Auth {
        session: Session::Loading,
    })
}

#[function_component(App)]
pub fn app() -> Html {
    let session = use_state(|| Session::Loading);
    {
        let session = session.clone();
        use_effect_with((), move |_| {
            spawn_local(async move {
                session.set(match api::me().await {
                    Ok(Some(me)) if me.anonymous => Session::Open { admin: me.is_admin },
                    Ok(Some(me)) => Session::SignedIn(me),
                    // A server from before `Me::anonymous` answered 204.
                    Ok(None) => Session::Open { admin: true },
                    Err(ApiError::Unauthorized) => {
                        crate::auth::begin_login();
                        Session::Redirecting
                    }
                    Err(ApiError::Forbidden(m)) => Session::Forbidden(m),
                    Err(e) => Session::Error(e.to_string()),
                });
            });
            || ()
        });
    }

    let auth = Auth {
        session: (*session).clone(),
    };
    let body = match &*session {
        Session::Loading => {
            html! { <div class="splash"><Loading label="Connecting to Buckle…" /></div> }
        }
        Session::Redirecting => html! {
            <div class="splash" data-testid="signing-in">
                <Loading label="Your session has ended. Taking you to sign in…" />
            </div>
        },
        Session::Forbidden(m) => html! {
            <div class="splash">
                <div class="state state--error" role="alert">
                    <p class="state__title">{ "Not permitted" }</p>
                    <p class="state__message">{ if m.is_empty() { "Your account is not allowed to use Buckle." } else { m.as_str() } }</p>
                </div>
            </div>
        },
        Session::Error(m) => html! {
            <div class="splash">
                <div class="state state--error" role="alert">
                    <p class="state__title">{ "Buckle could not be reached" }</p>
                    <p class="state__message">{ m.clone() }</p>
                    <button type="button" class="btn" onclick={Callback::from(|_| { let _ = crate::util::window().location().reload(); })}>{ "Try again" }</button>
                </div>
            </div>
        },
        Session::SignedIn(_) | Session::Open { .. } => html! {
            <LiveProvider>
                <Shell>
                    <Switch<Route> render={switch} />
                </Shell>
            </LiveProvider>
        },
    };

    html! {
        <BrowserRouter>
            <ContextProvider<Auth> context={auth}>
                <ToastHost>{ body }</ToastHost>
            </ContextProvider<Auth>>
        </BrowserRouter>
    }
}

fn switch(route: Route) -> Html {
    match route {
        Route::Overview => html! { <pages::overview::Overview /> },
        Route::Rollouts => html! { <pages::rollouts::Rollouts /> },
        Route::Rollout { id } => html! { <pages::rollout::RolloutPage {id} /> },
        Route::Hosts => html! { <pages::hosts::Hosts /> },
        Route::Host { id } => html! { <pages::host::HostPage {id} /> },
        Route::Revisions => html! { <pages::revisions::Revisions /> },
        Route::Audit => html! { <pages::audit::Audit /> },
        Route::NotFound => html! { <pages::NotFound /> },
    }
}
