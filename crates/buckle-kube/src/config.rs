//! Where an executor finds a cluster's API and the credential it may use
//! there: a kubeconfig on the host (root's `k3s.yaml` on a k3s server), the
//! in-cluster ServiceAccount of a pod, or only an address and CA for a
//! Buckle-issued bearer token.

use crate::transport::Tls;
use base64::Engine;
use buckle_core::Result;
use buckle_core::error::{user, wrap_user};
use secrecy::{SecretBox, SecretString};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The kubeconfigs an executor looks for, in order, after `KUBECONFIG`.
pub const KUBECONFIG_PATHS: &[&str] = &["/etc/rancher/k3s/k3s.yaml", "~/.kube/config"];

/// Where a pod's ServiceAccount credentials are mounted.
pub const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// An API server and the credential the host holds for it.
#[derive(Debug, Default)]
pub struct Endpoint {
    /// `https://host:6443`.
    pub server: String,
    pub tls: Tls,
    pub credential: HostCredential,
    /// Where it came from, for messages (`/etc/rancher/k3s/k3s.yaml`).
    pub source: String,
}

/// What the host itself can authenticate with.
#[derive(Default)]
pub enum HostCredential {
    #[default]
    None,
    /// A bearer token (a kubeconfig `token`, a ServiceAccount token).
    Token(SecretString),
    /// A client certificate, presented by the TLS layer ([`Tls::identity_pem`]).
    ClientCertificate,
}

impl std::fmt::Debug for HostCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HostCredential::None => "None",
            HostCredential::Token(_) => "Token(..)",
            HostCredential::ClientCertificate => "ClientCertificate",
        })
    }
}

impl Endpoint {
    /// An address with no credential (a Buckle-issued token is added later).
    pub fn address(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            source: "the cluster's api".into(),
            ..Default::default()
        }
    }

    /// The first kubeconfig found: `KUBECONFIG` (its first entry), then
    /// [`KUBECONFIG_PATHS`]. `None` when there is none.
    pub fn local() -> Result<Option<Self>> {
        for path in kubeconfig_candidates() {
            if path.is_file() {
                return Self::from_kubeconfig(&path).map(Some);
            }
        }
        Ok(None)
    }

    /// The pod's own ServiceAccount (`KUBERNETES_SERVICE_HOST`, the mounted
    /// token and CA). `None` outside a pod.
    pub fn in_cluster() -> Result<Option<Self>> {
        Self::in_cluster_at(
            std::env::var("KUBERNETES_SERVICE_HOST").ok().as_deref(),
            std::env::var("KUBERNETES_SERVICE_PORT").ok().as_deref(),
            Path::new(SERVICE_ACCOUNT_DIR),
        )
    }

    pub(crate) fn in_cluster_at(
        host: Option<&str>,
        port: Option<&str>,
        dir: &Path,
    ) -> Result<Option<Self>> {
        let Some(host) = host.filter(|h| !h.is_empty()) else {
            return Ok(None);
        };
        let port = port.filter(|p| !p.is_empty()).unwrap_or("443");
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let token = std::fs::read_to_string(dir.join("token")).map_err(|e| {
            wrap_user(
                e,
                "The pod's ServiceAccount token could not be read.",
                &["Mount a projected ServiceAccount token (automountServiceAccountToken: true)."],
            )
        })?;
        let mut tls = Tls::default();
        if let Ok(ca) = std::fs::read(dir.join("ca.crt")) {
            tls.ca_pem.push(ca);
        }
        Ok(Some(Self {
            server: format!("https://{host}:{port}"),
            tls,
            credential: HostCredential::Token(SecretString::from(token.trim().to_owned())),
            source: "the pod's ServiceAccount".into(),
        }))
    }

    /// The current context of the kubeconfig at `path`.
    pub fn from_kubeconfig(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            wrap_user(
                e,
                format!("The kubeconfig {} could not be read.", path.display()),
                &["The executor runs as root; check the file's permissions."],
            )
        })?;
        let base = path.parent().unwrap_or(Path::new("/"));
        Self::parse_kubeconfig(&text, base, &path.display().to_string())
    }

    pub(crate) fn parse_kubeconfig(text: &str, base: &Path, source: &str) -> Result<Self> {
        let invalid = |why: String| {
            user(
                format!("The kubeconfig {source} cannot be used: {why}."),
                &[
                    "Point KUBECONFIG at a kubeconfig whose current context has a server and a token or client certificate.",
                ],
            )
        };
        let cfg: Kubeconfig = serde_yaml::from_str(text).map_err(|e| invalid(e.to_string()))?;
        let context = match &cfg.current_context {
            Some(name) => cfg
                .contexts
                .iter()
                .find(|c| &c.name == name)
                .ok_or_else(|| invalid(format!("its current context {name} is not defined")))?,
            None => cfg
                .contexts
                .first()
                .ok_or_else(|| invalid("it defines no context".into()))?,
        };
        let cluster = cfg
            .clusters
            .iter()
            .find(|c| c.name == context.context.cluster)
            .ok_or_else(|| {
                invalid(format!(
                    "cluster {} is not defined",
                    context.context.cluster
                ))
            })?;
        let user_entry = context
            .context
            .user
            .as_ref()
            .and_then(|u| cfg.users.iter().find(|x| &x.name == u));

        let read = |inline: &Option<String>,
                    file: &Option<String>,
                    what: &str|
         -> Result<Option<Vec<u8>>> {
            if let Some(data) = inline {
                return base64::engine::general_purpose::STANDARD
                    .decode(data.trim())
                    .map(Some)
                    .map_err(|e| invalid(format!("its {what} is not base64 ({e})")));
            }
            if let Some(file) = file {
                let p = base.join(file);
                return std::fs::read(&p).map(Some).map_err(|e| {
                    invalid(format!("its {what} {} cannot be read ({e})", p.display()))
                });
            }
            Ok(None)
        };

        let mut tls = Tls::default();
        if let Some(ca) = read(
            &cluster.cluster.certificate_authority_data,
            &cluster.cluster.certificate_authority,
            "certificate-authority",
        )? {
            tls.ca_pem.push(ca);
        }
        let mut credential = HostCredential::None;
        if let Some(u) = user_entry.map(|u| &u.user) {
            if u.exec.is_some() || u.auth_provider.is_some() {
                return Err(invalid(
                    "its user authenticates through an exec plugin or auth provider, which Buckle does not run".into(),
                ));
            }
            if let Some(token) = &u.token {
                credential = HostCredential::Token(SecretString::from(token.trim().to_owned()));
            } else if let Some(file) = &u.token_file {
                let token = std::fs::read_to_string(base.join(file))
                    .map_err(|e| invalid(format!("its tokenFile cannot be read ({e})")))?;
                credential = HostCredential::Token(SecretString::from(token.trim().to_owned()));
            } else if let (Some(mut cert), Some(key)) = (
                read(
                    &u.client_certificate_data,
                    &u.client_certificate,
                    "client-certificate",
                )?,
                read(&u.client_key_data, &u.client_key, "client-key")?,
            ) {
                cert.push(b'\n');
                cert.extend_from_slice(&key);
                tls.identity_pem = Some(SecretBox::new(Box::new(cert)));
                credential = HostCredential::ClientCertificate;
            }
        }
        Ok(Self {
            server: cluster.cluster.server.trim_end_matches('/').to_owned(),
            tls,
            credential,
            source: source.to_owned(),
        })
    }

    /// Drops the host's credential, keeping the address and CA: for a
    /// Buckle-issued token.
    pub fn without_credential(mut self) -> Self {
        self.credential = HostCredential::None;
        self.tls.identity_pem = None;
        self
    }
}

fn kubeconfig_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(list) = std::env::var("KUBECONFIG") {
        out.extend(std::env::split_paths(&list).filter(|p| !p.as_os_str().is_empty()));
    }
    let home = std::env::var("HOME").ok();
    for p in KUBECONFIG_PATHS {
        match (p.strip_prefix("~/"), &home) {
            (Some(rest), Some(home)) => out.push(Path::new(home).join(rest)),
            (Some(_), None) => {}
            (None, _) => out.push(PathBuf::from(p)),
        }
    }
    out
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Kubeconfig {
    #[serde(default)]
    clusters: Vec<Named<ClusterEntry>>,
    #[serde(default)]
    users: Vec<NamedUser>,
    #[serde(default)]
    contexts: Vec<NamedContext>,
    #[serde(default)]
    current_context: Option<String>,
}

#[derive(Deserialize)]
struct Named<T> {
    name: String,
    cluster: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct ClusterEntry {
    server: String,
    #[serde(default)]
    certificate_authority_data: Option<String>,
    #[serde(default)]
    certificate_authority: Option<String>,
}

#[derive(Deserialize)]
struct NamedUser {
    name: String,
    #[serde(default)]
    user: UserEntry,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
struct UserEntry {
    #[serde(default)]
    token: Option<String>,
    #[serde(default, rename = "tokenFile")]
    token_file: Option<String>,
    #[serde(default)]
    client_certificate_data: Option<String>,
    #[serde(default)]
    client_certificate: Option<String>,
    #[serde(default)]
    client_key_data: Option<String>,
    #[serde(default)]
    client_key: Option<String>,
    #[serde(default)]
    exec: Option<serde_yaml::Value>,
    #[serde(default)]
    auth_provider: Option<serde_yaml::Value>,
}

#[derive(Deserialize)]
struct NamedContext {
    name: String,
    context: ContextEntry,
}

#[derive(Deserialize)]
struct ContextEntry {
    cluster: String,
    #[serde(default)]
    user: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const K3S: &str = r#"
apiVersion: v1
clusters:
- cluster:
    certificate-authority-data: Q0EtUEVN
    server: https://127.0.0.1:6443
  name: default
contexts:
- context:
    cluster: default
    user: default
  name: default
current-context: default
kind: Config
users:
- name: default
  user:
    client-certificate-data: Q0VSVA==
    client-key-data: S0VZ
"#;

    #[test]
    fn a_k3s_kubeconfig_gives_the_server_ca_and_client_certificate() {
        let e = Endpoint::parse_kubeconfig(K3S, Path::new("/etc"), "k3s.yaml").unwrap();
        assert_eq!(e.server, "https://127.0.0.1:6443");
        assert_eq!(e.tls.ca_pem, vec![b"CA-PEM".to_vec()]);
        assert!(matches!(e.credential, HostCredential::ClientCertificate));
        use secrecy::ExposeSecret;
        assert_eq!(
            e.tls
                .identity_pem
                .as_ref()
                .unwrap()
                .expose_secret()
                .as_slice(),
            b"CERT\nKEY"
        );
        let bare = e.without_credential();
        assert!(matches!(bare.credential, HostCredential::None));
        assert!(bare.tls.identity_pem.is_none() && !bare.tls.ca_pem.is_empty());
    }

    #[test]
    fn a_token_user_and_a_named_context_are_followed() {
        let text = r#"
clusters:
- { name: a, cluster: { server: "https://a:6443/" } }
- { name: b, cluster: { server: "https://b:6443" } }
users:
- { name: ops, user: { token: " tok " } }
contexts:
- { name: one, context: { cluster: a, user: ops } }
- { name: two, context: { cluster: b, user: ops } }
current-context: two
"#;
        let e = Endpoint::parse_kubeconfig(text, Path::new("/"), "x").unwrap();
        assert_eq!(e.server, "https://b:6443");
        use secrecy::ExposeSecret;
        match e.credential {
            HostCredential::Token(t) => assert_eq!(t.expose_secret(), "tok"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn exec_plugins_are_refused() {
        let text = r#"
clusters: [{ name: a, cluster: { server: "https://a" } }]
users: [{ name: u, user: { exec: { command: aws } } }]
contexts: [{ name: c, context: { cluster: a, user: u } }]
"#;
        let e = Endpoint::parse_kubeconfig(text, Path::new("/"), "x").unwrap_err();
        assert!(
            e.description().contains("exec plugin"),
            "{}",
            e.description()
        );
    }

    #[test]
    fn in_cluster_reads_the_mounted_service_account() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "sa-token\n").unwrap();
        std::fs::write(dir.path().join("ca.crt"), "CA").unwrap();
        let e = Endpoint::in_cluster_at(Some("10.43.0.1"), Some("443"), dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(e.server, "https://10.43.0.1:443");
        assert!(matches!(e.credential, HostCredential::Token(_)));
        assert!(
            Endpoint::in_cluster_at(None, None, dir.path())
                .unwrap()
                .is_none()
        );
        let v6 = Endpoint::in_cluster_at(Some("fd00::1"), None, dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(v6.server, "https://[fd00::1]:443");
    }
}
