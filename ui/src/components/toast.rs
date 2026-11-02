//! Toasts: the outcome of an action, said where it will be noticed and then
//! gone. Failures stay until dismissed, because a refusal is something the
//! operator needs to read, not catch.

use std::rc::Rc;

use gloo_timers::callback::Timeout;
use yew::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Ok,
    Error,
}

#[derive(Clone, PartialEq)]
struct Toast {
    id: u32,
    kind: ToastKind,
    text: String,
}

#[derive(Clone, PartialEq, Default)]
struct Toasts {
    next: u32,
    list: Vec<Toast>,
}

enum Action {
    Push(ToastKind, String),
    Dismiss(u32),
}

impl Reducible for Toasts {
    type Action = Action;
    fn reduce(self: Rc<Self>, action: Action) -> Rc<Self> {
        let mut next = (*self).clone();
        match action {
            Action::Push(kind, text) => {
                next.next += 1;
                next.list.push(Toast {
                    id: next.next,
                    kind,
                    text,
                });
                if next.list.len() > 4 {
                    next.list.remove(0);
                }
            }
            Action::Dismiss(id) => next.list.retain(|t| t.id != id),
        }
        Rc::new(next)
    }
}

/// The handle pages use to report what happened.
#[derive(Clone, PartialEq)]
pub struct Toaster {
    dispatch: UseReducerDispatcher<Toasts>,
}

impl Toaster {
    pub fn ok(&self, text: impl Into<String>) {
        self.dispatch
            .dispatch(Action::Push(ToastKind::Ok, text.into()));
    }

    pub fn error(&self, text: impl Into<String>) {
        self.dispatch
            .dispatch(Action::Push(ToastKind::Error, text.into()));
    }
}

#[derive(Properties, PartialEq)]
pub struct ToastHostProps {
    #[prop_or_default]
    pub children: Html,
}

#[function_component(ToastHost)]
pub fn toast_host(props: &ToastHostProps) -> Html {
    let toasts = use_reducer(Toasts::default);
    let toaster = Toaster {
        dispatch: toasts.dispatcher(),
    };

    // Successes dismiss themselves after a few seconds.
    {
        let dispatch = toasts.dispatcher();
        let newest_ok = toasts
            .list
            .iter()
            .rev()
            .find(|t| t.kind == ToastKind::Ok)
            .map(|t| t.id);
        use_effect_with(newest_ok, move |id| {
            let timeout =
                id.map(|id| Timeout::new(5_000, move || dispatch.dispatch(Action::Dismiss(id))));
            move || drop(timeout)
        });
    }

    let item = |t: &Toast| {
        let dispatch = toasts.dispatcher();
        let id = t.id;
        let dismiss = Callback::from(move |_: MouseEvent| dispatch.dispatch(Action::Dismiss(id)));
        html! {
            <div key={t.id} class={classes!("toast", if t.kind == ToastKind::Ok { "toast--ok" } else { "toast--error" })}
                role={if t.kind == ToastKind::Error { "alert" } else { "status" }}>
                <span class="toast__text">{ t.text.clone() }</span>
                <button type="button" class="icon-btn" aria-label="Dismiss" onclick={dismiss}>
                    { super::icons::close() }
                </button>
            </div>
        }
    };

    html! {
        <ContextProvider<Toaster> context={toaster}>
            { props.children.clone() }
            <div class="toasts" aria-live="polite">
                { for toasts.list.iter().map(item) }
            </div>
        </ContextProvider<Toaster>>
    }
}

#[hook]
pub fn use_toast() -> Toaster {
    use_context::<Toaster>().expect("the toast host wraps the app")
}
