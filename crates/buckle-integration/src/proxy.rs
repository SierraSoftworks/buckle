//! A cuttable TCP proxy between the agents and the server's gRPC port.
//!
//! Agents are configured with the proxy's address, so a test can cut every
//! agent off the server at once (dropping their connections, as a network
//! partition or a dead server would), keep them cut while the server is
//! stopped, and point them at a fresh server instance on another port.
//! TLS passes through untouched, so certificate pinning is unaffected.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct State {
    upstream: Option<SocketAddr>,
    generation: CancellationToken,
}

/// The proxy. Dropping it stops it and drops every connection.
pub struct Proxy {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
}

impl Proxy {
    /// Listens on an ephemeral localhost port, forwarding nowhere yet.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("the proxy binds");
        let addr = listener.local_addr().expect("the proxy has an address");
        let state = Arc::new(Mutex::new(State::default()));
        let stop = CancellationToken::new();
        let (s2, stop2) = (state.clone(), stop.clone());
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = stop2.cancelled() => break,
                    a = listener.accept() => a,
                };
                let Ok((inbound, _)) = accepted else { continue };
                let (upstream, generation) = {
                    let s = s2.lock().unwrap_or_else(|e| e.into_inner());
                    (s.upstream, s.generation.clone())
                };
                let Some(upstream) = upstream else {
                    drop(inbound);
                    continue;
                };
                let stop3 = stop2.clone();
                tokio::spawn(async move {
                    let Ok(outbound) = TcpStream::connect(upstream).await else {
                        return;
                    };
                    let _ = inbound.set_nodelay(true);
                    let _ = outbound.set_nodelay(true);
                    let (mut a, mut b) = (inbound, outbound);
                    tokio::select! {
                        _ = generation.cancelled() => {}
                        _ = stop3.cancelled() => {}
                        _ = tokio::io::copy_bidirectional(&mut a, &mut b) => {}
                    }
                });
            }
        });
        Self { addr, state, stop }
    }

    /// Where agents connect.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Forwards new connections to `upstream`.
    pub fn point_at(&self, upstream: SocketAddr) {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .upstream = Some(upstream);
    }

    /// Drops every proxied connection and refuses new ones until
    /// [`Proxy::point_at`] is called again.
    pub fn cut(&self) {
        self.cutter().cut();
    }

    /// A handle that cuts this proxy from anywhere (e.g. a scripted command
    /// on a fake host, so a change itself can cut the agent off).
    pub fn cutter(&self) -> Cutter {
        Cutter(self.state.clone())
    }
}

/// Cuts a [`Proxy`], as [`Proxy::cut`] does.
#[derive(Clone)]
pub struct Cutter(Arc<Mutex<State>>);

impl Cutter {
    /// See [`Proxy::cut`].
    pub fn cut(&self) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        s.upstream = None;
        s.generation.cancel();
        s.generation = CancellationToken::new();
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
