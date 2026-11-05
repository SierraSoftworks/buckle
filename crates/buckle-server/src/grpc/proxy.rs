//! Agents behind a TCP proxy (design §08): a connection from one of
//! `agents.trusted_proxies` may start with a PROXY protocol v2 header naming
//! the agent's own address, which whois then identifies instead of the
//! proxy's. Every other connection is served exactly as it arrived; a header
//! from an untrusted peer is never read and fails the TLS/HTTP/2 handshake.

use super::auth::Whois;
use crate::config::TrustedProxy;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tonic::transport::server::{Connected, TlsConnectInfo};

/// How long a trusted proxy may take to send its header.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// The PROXY protocol v2 signature.
const SIGNATURE: [u8; 12] = *b"\r\n\r\n\0\r\nQUIT\n";

/// The proxy a connection came through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyHop {
    /// The proxy's own address (the TCP peer).
    pub addr: SocketAddr,
    /// The proxy's tailnet node, when whois knows it.
    pub node: Option<String>,
}

impl std::fmt::Display for ProxyHop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.node {
            Some(node) => write!(f, "{node} ({})", self.addr),
            None => write!(f, "{}", self.addr),
        }
    }
}

/// Who is on the other end of an agent connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Peer {
    /// The agent's address: what a trusted proxy forwarded, the TCP peer otherwise.
    pub addr: Option<SocketAddr>,
    /// The trusted proxy that forwarded `addr`.
    pub proxy: Option<ProxyHop>,
}

impl Peer {
    /// The peer of `request`, as the listener accepted it.
    pub fn of<T>(request: &tonic::Request<T>) -> Self {
        let ext = request.extensions();
        ext.get::<Peer>()
            .or_else(|| ext.get::<TlsConnectInfo<Peer>>().map(|t| t.get_ref()))
            .cloned()
            .unwrap_or_else(|| Self {
                addr: request.remote_addr(),
                proxy: None,
            })
    }
}

/// An accepted agent connection.
#[derive(Debug)]
pub struct AgentStream {
    inner: TcpStream,
    peer: Peer,
}

impl Connected for AgentStream {
    type ConnectInfo = Peer;

    fn connect_info(&self) -> Peer {
        self.peer.clone()
    }
}

impl AsyncRead for AgentStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for AgentStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Decides which peers are trusted proxies.
#[derive(Clone)]
pub struct Proxies {
    trusted: Arc<[TrustedProxy]>,
    whois: Option<Arc<dyn Whois>>,
}

impl Proxies {
    /// Trusts `trusted`, asking `whois` for the tags of tailnet peers.
    pub fn new(trusted: &[TrustedProxy], whois: Option<Arc<dyn Whois>>) -> Self {
        Self {
            trusted: trusted.into(),
            whois,
        }
    }

    /// The proxy `peer` is, or `None` when it is not trusted to forward.
    async fn hop(&self, peer: SocketAddr) -> Option<ProxyHop> {
        if self.trusted.is_empty() {
            return None;
        }
        let ip = peer.ip().to_canonical();
        let by_addr = self
            .trusted
            .iter()
            .any(|t| matches!(t, TrustedProxy::Net(n) if n.contains(&ip)));
        let wants_tags = self
            .trusted
            .iter()
            .any(|t| matches!(t, TrustedProxy::Tag(_)));
        let identity = match &self.whois {
            Some(w) if (by_addr || wants_tags) && w.covers(&peer) => match w.whois(&peer).await {
                Ok(identity) => identity,
                Err(e) => {
                    tracing::warn!(peer = %peer, error = %e, "could not ask tailscaled whether a peer is a trusted proxy");
                    None
                }
            },
            _ => None,
        };
        let by_tag = identity.as_ref().is_some_and(|id| {
            self.trusted
                .iter()
                .any(|t| matches!(t, TrustedProxy::Tag(tag) if id.tags.contains(tag)))
        });
        (by_addr || by_tag).then(|| ProxyHop {
            addr: peer,
            node: identity.map(|id| id.node),
        })
    }

    /// Reads a trusted proxy's header off `stream` and says who the agent is.
    pub async fn accept(&self, stream: TcpStream) -> io::Result<AgentStream> {
        let peer = stream.peer_addr()?;
        let direct = Peer {
            addr: Some(peer),
            proxy: None,
        };
        let Some(hop) = self.hop(peer).await else {
            return Ok(AgentStream {
                inner: stream,
                peer: direct,
            });
        };
        let mut stream = stream;
        let forwarded = tokio::time::timeout(HEADER_TIMEOUT, read_header(&mut stream))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no PROXY header in time"))??;
        let peer = match forwarded {
            Some(addr) => Peer {
                addr: Some(addr),
                proxy: Some(hop),
            },
            // The proxy's own connection (a health check) or no header at all.
            None => direct,
        };
        Ok(AgentStream {
            inner: stream,
            peer,
        })
    }

    /// Accepts on `listener` until `cancel` trips, reading each connection's
    /// header off the accept loop so a slow proxy holds up nobody else.
    pub fn incoming(
        self,
        listener: TcpListener,
        cancel: CancellationToken,
    ) -> tokio_stream::wrappers::ReceiverStream<io::Result<AgentStream>> {
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = cancel.cancelled() => return,
                    a = listener.accept() => a,
                };
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        // Out of descriptors and the like: back off, don't spin.
                        tracing::warn!(error = %e, "could not accept an agent connection");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let (proxies, tx) = (self.clone(), tx.clone());
                tokio::spawn(async move {
                    match proxies.accept(stream).await {
                        Ok(s) => {
                            let _ = tx.send(Ok(s)).await;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "dropped a proxied agent connection");
                        }
                    }
                });
            }
        });
        tokio_stream::wrappers::ReceiverStream::new(rx)
    }
}

/// Consumes a PROXY protocol v2 header, if the stream starts with one, and
/// returns the source address it carries. Only the header's own bytes are
/// read, so whatever follows (TLS, HTTP/2) is left for the server.
async fn read_header(stream: &mut TcpStream) -> io::Result<Option<SocketAddr>> {
    let mut first = [0u8; 1];
    if stream.peek(&mut first).await? == 0 || first[0] != SIGNATURE[0] {
        return Ok(None);
    }
    let mut head = [0u8; 16];
    stream.read_exact(&mut head).await?;
    let len = u16::from_be_bytes([head[14], head[15]]) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    decode(&head, &body)
}

/// Decodes a v2 header (`head`, then its `body`): the source address of a
/// proxied TCP connection, `None` for a `LOCAL` one.
pub fn decode(head: &[u8; 16], body: &[u8]) -> io::Result<Option<SocketAddr>> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
    if head[..12] != SIGNATURE {
        return Err(bad("not a PROXY protocol v2 header"));
    }
    if head[12] >> 4 != 2 {
        return Err(bad("unsupported PROXY protocol version"));
    }
    match head[12] & 0x0F {
        0 => return Ok(None),
        1 => {}
        _ => return Err(bad("unsupported PROXY protocol command")),
    }
    let port = |b: &[u8]| u16::from_be_bytes([b[0], b[1]]);
    match head[13] {
        // TCP over IPv4: src, dst, src port, dst port.
        0x11 if body.len() >= 12 => {
            let ip: [u8; 4] = body[..4].try_into().unwrap();
            Ok(Some(SocketAddr::from((ip, port(&body[8..10])))))
        }
        // TCP over IPv6.
        0x21 if body.len() >= 36 => {
            let ip = std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&body[..16]).unwrap());
            Ok(Some(SocketAddr::new(
                ip.to_canonical(),
                port(&body[32..34]),
            )))
        }
        // UNSPEC: the proxy does not know the source.
        0x00 => Ok(None),
        _ => Err(bad("unsupported or truncated PROXY protocol address")),
    }
}

/// Encodes a v2 `PROXY` header from `src` to `dst` (tests and tools).
pub fn encode(src: SocketAddr, dst: SocketAddr) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    out.push(0x21);
    let mut body = Vec::new();
    match (src, dst) {
        (SocketAddr::V4(s), SocketAddr::V4(d)) => {
            out.push(0x11);
            body.extend(s.ip().octets());
            body.extend(d.ip().octets());
        }
        (s, d) => {
            out.push(0x21);
            let v6 = |a: SocketAddr| match a {
                SocketAddr::V4(v4) => v4.ip().to_ipv6_mapped(),
                SocketAddr::V6(v6) => *v6.ip(),
            };
            body.extend(v6(s).octets());
            body.extend(v6(d).octets());
        }
    }
    body.extend(src.port().to_be_bytes());
    body.extend(dst.port().to_be_bytes());
    out.extend((body.len() as u16).to_be_bytes());
    out.extend(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn split(bytes: &[u8]) -> ([u8; 16], &[u8]) {
        (bytes[..16].try_into().unwrap(), &bytes[16..])
    }

    #[rstest]
    #[case("100.64.0.7:41234", "100.64.0.1:7700")]
    #[case("[fd7a:115c:a1e0::7]:41234", "[fd7a:115c:a1e0::1]:7700")]
    fn headers_round_trip(#[case] src: &str, #[case] dst: &str) {
        let src: SocketAddr = src.parse().unwrap();
        let bytes = encode(src, dst.parse().unwrap());
        let (head, body) = split(&bytes);
        assert_eq!(decode(&head, body).unwrap(), Some(src));
    }

    #[test]
    fn local_and_unspec_carry_no_address() {
        let mut head = [0u8; 16];
        head[..12].copy_from_slice(&SIGNATURE);
        head[12] = 0x20;
        assert_eq!(decode(&head, &[]).unwrap(), None, "LOCAL");
        head[12] = 0x21;
        assert_eq!(decode(&head, &[]).unwrap(), None, "PROXY UNSPEC");
    }

    #[rstest]
    #[case::signature(0, 0x00)]
    #[case::version(12, 0x11)]
    #[case::command(12, 0x2F)]
    #[case::family(13, 0x31)]
    fn malformed_headers_are_refused(#[case] at: usize, #[case] value: u8) {
        let bytes = encode(
            "100.64.0.7:1".parse().unwrap(),
            "100.64.0.1:2".parse().unwrap(),
        );
        let (mut head, body) = split(&bytes);
        head[at] = value;
        assert!(decode(&head, body).is_err());
    }

    #[test]
    fn truncated_addresses_are_refused() {
        let bytes = encode(
            "100.64.0.7:1".parse().unwrap(),
            "100.64.0.1:2".parse().unwrap(),
        );
        let (head, body) = split(&bytes);
        assert!(decode(&head, &body[..6]).is_err());
    }
}
