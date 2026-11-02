//! The live event stream, `GET /api/v1/events` (design §12).
//!
//! Server-Sent Events: one-directional, proxied without fuss, and carried by
//! the session cookie like any other request. The browser reconnects a
//! dropped stream by itself; when it gives up (the server answered with an
//! error, or is down) this loop reconnects with exponential backoff, and on
//! the way checks the session, so an expired one is refreshed or sent to
//! sign in rather than retried for ever. A stream the server ends with
//! `session_ended` (it re-checks the session while the stream is open) is
//! renewed at once if the refresh cookie allows, or sent to sign in.

use futures::StreamExt;
use futures::stream::select_all;
use gloo_net::eventsource::futures::EventSource;
use gloo_timers::future::TimeoutFuture;

use crate::logic::live::{Backoff, EVENT_NAMES, Frame, decode};

/// What the stream reports to the live store.
pub enum Signal {
    /// Connected (or reconnected): anything may have been missed, so pages
    /// refetch.
    Open,
    Frame(Frame),
    /// Lost; trying again in this many seconds.
    Retrying(u32),
}

/// Runs for the lifetime of the page, calling `emit` for everything that
/// happens on the stream.
pub async fn run(emit: impl Fn(Signal) + 'static) {
    let url = format!("{}/events", super::BASE);
    let mut backoff = Backoff::new(1_000, 30_000);
    loop {
        if let Ok(mut source) = EventSource::new(&url) {
            let mut streams = Vec::new();
            if let Ok(open) = source.subscribe("open") {
                streams.push(open);
            }
            for name in EVENT_NAMES {
                if let Ok(s) = source.subscribe(*name) {
                    streams.push(s);
                }
            }
            let mut all = select_all(streams);
            let mut ended = false;
            while let Some(item) = all.next().await {
                match item {
                    Ok((name, _)) if name == "open" => {
                        backoff.reset();
                        emit(Signal::Open);
                    }
                    Ok((name, message)) => {
                        let data = message.data().as_string().unwrap_or_default();
                        match decode(&name, &data) {
                            Ok(Frame::SessionEnded) => {
                                ended = true;
                                break;
                            }
                            Ok(frame) => emit(Signal::Frame(frame)),
                            Err(e) => log::warn!("{e}"),
                        }
                    }
                    Err(_) => break,
                }
            }
            drop(all);
            source.close();
            if ended {
                if crate::auth::session_ended().await {
                    // Renewed: reconnect straight away with the new cookie.
                    backoff.reset();
                    continue;
                }
                // On the way to sign in; nothing to reconnect to.
                return;
            }
        }

        let delay = backoff.next_delay(crate::util::random());
        emit(Signal::Retrying(delay.div_ceil(1_000)));
        // A closed stream is often an expired session: asking who we are
        // goes through the client's refresh-then-sign-in path.
        if backoff.attempts() > 1 {
            let _ = super::me().await;
        }
        TimeoutFuture::new(delay).await;
    }
}
