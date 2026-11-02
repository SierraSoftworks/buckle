//! Agent authentication: Tailscale whois when the peer is on the tailnet,
//! per-host tokens (enrolled with the bootstrap token) otherwise.

use async_trait::async_trait;
use sha2::{Digest as _, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

/// Who a tailnet peer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhoisIdentity {
    /// The node's short name (the host identity).
    pub node: String,
    /// The owning user's login, when the node is user-owned.
    pub user: Option<String>,
    /// The node's ACL tags (`tag:…`); empty for a user-owned node.
    pub tags: Vec<String>,
}

/// Resolves tailnet peers to node identities.
#[async_trait]
pub trait Whois: Send + Sync + 'static {
    /// Whether this resolver can answer for `peer` at all (tailnet address
    /// and a reachable tailscaled).
    fn covers(&self, peer: &SocketAddr) -> bool;

    /// The node behind `peer`, or `None` when tailscaled does not know it.
    async fn whois(&self, peer: &SocketAddr) -> buckle_core::Result<Option<WhoisIdentity>>;
}

/// Whether `ip` is a Tailscale address (100.64.0.0/10 or fd7a:115c:a1e0::/48).
pub fn is_tailnet(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 100 && (o[1] & 0xC0) == 64
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_tailnet(&IpAddr::V4(v4));
            }
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// Asks the local tailscaled over its LocalAPI unix socket.
#[derive(Debug, Clone)]
pub struct LocalApiWhois {
    socket: PathBuf,
}

impl LocalApiWhois {
    /// Uses the tailscaled socket at `socket`.
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }
}

/// Extracts the node identity from a LocalAPI whois response body.
pub fn parse_whois(body: &[u8]) -> Option<WhoisIdentity> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let node = &v["Node"];
    let name = node["ComputedName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            node["Name"]
                .as_str()
                .and_then(|n| n.split('.').next())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        })?;
    let tags = node["Tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Some(WhoisIdentity {
        node: name,
        // Tagged nodes report a placeholder profile; only a user-owned node
        // has an owner worth matching.
        user: v["UserProfile"]["LoginName"]
            .as_str()
            .filter(|_| node["Tags"].as_array().is_none_or(|a| a.is_empty()))
            .map(str::to_owned),
        tags,
    })
}

#[async_trait]
impl Whois for LocalApiWhois {
    fn covers(&self, peer: &SocketAddr) -> bool {
        is_tailnet(&peer.ip()) && self.socket.exists()
    }

    #[cfg(unix)]
    async fn whois(&self, peer: &SocketAddr) -> buckle_core::Result<Option<WhoisIdentity>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let fail = |e: std::io::Error| {
            buckle_core::error::wrap_system(
                e,
                "The local tailscaled could not be asked who a peer is.",
                &["Check agents.tailscale_socket in server.yaml and that tailscaled is running."],
            )
        };
        let mut stream = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::UnixStream::connect(&self.socket),
        )
        .await
        .map_err(|_| fail(std::io::Error::other("timed out connecting to tailscaled")))?
        .map_err(fail)?;
        let addr = peer.to_string();
        let query: String = url::form_urlencoded::byte_serialize(addr.as_bytes()).collect();
        let request = format!(
            "GET /localapi/v0/whois?addr={query} HTTP/1.0\r\nHost: local-tailscaled.sock\r\nAccept: application/json\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.map_err(fail)?;
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_to_end(&mut response),
        )
        .await
        .map_err(|_| fail(std::io::Error::other("timed out reading from tailscaled")))?
        .map_err(fail)?;
        let Some(split) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
            return Ok(None);
        };
        let head = String::from_utf8_lossy(&response[..split]);
        let ok = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            == Some("200");
        if !ok {
            return Ok(None);
        }
        Ok(parse_whois(&response[split + 4..]))
    }

    #[cfg(not(unix))]
    async fn whois(&self, _peer: &SocketAddr) -> buckle_core::Result<Option<WhoisIdentity>> {
        Ok(None)
    }
}

/// SHA-256 hex of a token (what is stored).
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// A fresh per-host token: 32 random bytes, hex.
pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

/// Constant-time string equality.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Whether a host id is acceptable: 1–128 of `[A-Za-z0-9._-]`.
pub fn valid_host_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Compares dotted versions numerically; a pre-release (`2.0.0-dev`) sorts
/// before its release. Unparseable parts compare as zero.
pub fn version_at_least(version: &str, minimum: &str) -> bool {
    fn parse(v: &str) -> (Vec<u64>, bool) {
        let v = v.trim().trim_start_matches('v');
        let (core, pre) = match v.split_once(['-', '+']) {
            Some((c, rest)) => (c, !rest.is_empty() && v.contains('-')),
            None => (v, false),
        };
        let mut parts: Vec<u64> = core.split('.').map(|p| p.parse().unwrap_or(0)).collect();
        while parts.len() < 3 {
            parts.push(0);
        }
        (parts, pre)
    }
    let (a, a_pre) = parse(version);
    let (b, b_pre) = parse(minimum);
    match a.cmp(&b) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => !a_pre || b_pre,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("100.64.0.1", true)]
    #[case("100.127.255.254", true)]
    #[case("100.128.0.1", false)]
    #[case("10.0.0.1", false)]
    #[case("fd7a:115c:a1e0::1", true)]
    #[case("fd7a:115c:a1e1::1", false)]
    #[case("::ffff:100.100.1.1", true)]
    fn tailnet_addresses(#[case] ip: &str, #[case] expected: bool) {
        assert_eq!(is_tailnet(&ip.parse().unwrap()), expected);
    }

    #[rstest]
    #[case("2.0.0", "2.0.0", true)]
    #[case("2.0.1", "2.0.0", true)]
    #[case("1.9.9", "2.0.0", false)]
    #[case("2.0.0-dev", "2.0.0", false)]
    #[case("2.0.0", "2.0.0-dev", true)]
    #[case("2.1", "2.0.5", true)]
    #[case("v2.0.0", "2.0.0", true)]
    fn versions_compare_numerically(#[case] v: &str, #[case] min: &str, #[case] ok: bool) {
        assert_eq!(version_at_least(v, min), ok);
    }

    #[test]
    fn whois_bodies_name_the_node() {
        let body = br#"{"Node":{"Name":"pi-1.tail1234.ts.net.","ComputedName":"pi-1"},"UserProfile":{"LoginName":"ada@example.com"}}"#;
        assert_eq!(
            parse_whois(body),
            Some(WhoisIdentity {
                node: "pi-1".into(),
                user: Some("ada@example.com".into()),
                tags: vec![],
            })
        );
        let unnamed = br#"{"Node":{"Name":"oci-1.tail1234.ts.net."}}"#;
        assert_eq!(parse_whois(unnamed).unwrap().node, "oci-1");
        let tagged = br#"{"Node":{"Name":"oci-1.tail1234.ts.net.","Tags":["tag:buckle"]},"UserProfile":{"LoginName":"tagged-devices"}}"#;
        let id = parse_whois(tagged).unwrap();
        assert_eq!(id.tags, vec!["tag:buckle".to_string()]);
        assert_eq!(id.user, None, "a tagged node has no owner to match");
        assert_eq!(parse_whois(b"nope"), None);
    }

    #[rstest]
    #[case("pi-1", true)]
    #[case("a.b_c", true)]
    #[case("", false)]
    #[case("../etc", false)]
    #[case("has space", false)]
    fn host_ids(#[case] id: &str, #[case] ok: bool) {
        assert_eq!(valid_host_id(id), ok);
    }

    #[test]
    fn tokens_are_random_and_hashed() {
        let a = new_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, new_token());
        assert_eq!(hash_token(&a).len(), 64);
        assert!(constant_time_eq(&a, &a.clone()));
        assert!(!constant_time_eq(&a, "x"));
    }
}
