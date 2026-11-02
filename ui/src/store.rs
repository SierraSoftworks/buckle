//! The shared live store: the events received from `GET /events`, numbered,
//! and the state of the connection. Pages fetch their data and then apply
//! the events received since (see [`crate::hooks::use_fetch`] and
//! [`crate::logic::live`]); a reconnection or a `lagged` signal bumps the
//! epoch, which makes every page fetch afresh.

use std::rc::Rc;

use buckle_api::LiveEvent;
use wasm_bindgen_futures::spawn_local;
use yew::prelude::*;

use crate::api::sse::{self, Signal};
use crate::logic::live::{EventLog, Frame};

/// The connection, as the app bar shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Open,
    Retrying(u32),
    /// Demo mode: nothing to connect to.
    Demo,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Live {
    pub log: EventLog,
    pub link: Link,
    /// Bumped whenever events may have been missed.
    pub epoch: u64,
}

pub enum Action {
    Event(Box<LiveEvent>),
    Link(Link),
    Resync,
}

impl Reducible for Live {
    type Action = Action;

    fn reduce(self: Rc<Self>, action: Action) -> Rc<Self> {
        let mut next = (*self).clone();
        match action {
            Action::Event(event) => next.log.push(*event),
            Action::Link(link) => next.link = link,
            Action::Resync => next.epoch += 1,
        }
        Rc::new(next)
    }
}

pub type LiveHandle = UseReducerHandle<Live>;

#[derive(Properties, PartialEq)]
pub struct LiveProviderProps {
    #[prop_or_default]
    pub children: Html,
}

#[function_component(LiveProvider)]
pub fn live_provider(props: &LiveProviderProps) -> Html {
    let demo = crate::fixtures::is_demo();
    let live = use_reducer(|| Live {
        log: EventLog::default(),
        link: if demo { Link::Demo } else { Link::Connecting },
        epoch: 0,
    });

    {
        let dispatcher = live.dispatcher();
        use_effect_with((), move |_| {
            if demo {
                #[cfg(debug_assertions)]
                demo_feed(dispatcher);
            } else {
                spawn_local(sse::run(move |signal| match signal {
                    Signal::Open => {
                        dispatcher.dispatch(Action::Link(Link::Open));
                        dispatcher.dispatch(Action::Resync);
                    }
                    Signal::Frame(Frame::Event(e)) => dispatcher.dispatch(Action::Event(e)),
                    // `sse::run` handles the end of a session itself.
                    Signal::Frame(Frame::Lagged | Frame::SessionEnded) => {
                        dispatcher.dispatch(Action::Resync)
                    }
                    Signal::Retrying(secs) => {
                        dispatcher.dispatch(Action::Link(Link::Retrying(secs)))
                    }
                }));
            }
            || ()
        });
    }

    html! {
        <ContextProvider<LiveHandle> context={live}>
            { props.children.clone() }
        </ContextProvider<LiveHandle>>
    }
}

/// Delivers what the demo store produces (and, with `?demo&live`, a rollout
/// that moves by itself) through the same reducer the stream feeds.
#[cfg(debug_assertions)]
fn demo_feed(dispatcher: UseReducerDispatcher<Live>) {
    use gloo_timers::callback::Interval;
    let ticking = crate::fixtures::flag("live").is_some();
    // `?demo&session=ended`: the server ends the stream as a session lapses.
    let ends = crate::fixtures::flag("session").as_deref() == Some("ended");
    let mut beat = 0u32;
    Interval::new(400, move || {
        beat += 1;
        if ends && beat == 3 {
            spawn_local(async {
                crate::auth::session_ended().await;
            });
        }
        let mut events = crate::fixtures::store::drain();
        if ticking && beat.is_multiple_of(10) {
            events.extend(crate::fixtures::live::tick());
        }
        for e in events {
            dispatcher.dispatch(Action::Event(Box::new(e)));
        }
    })
    .forget();
}

#[hook]
pub fn use_live() -> LiveHandle {
    use_context::<LiveHandle>().expect("the live store wraps the signed-in app")
}
