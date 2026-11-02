//! TLS for the gRPC listener: a certificate from files, or one generated on
//! first start under `data_dir/tls/` whose SHA-256 fingerprint agents pin.

use crate::config::{TlsConfig, TlsMode};
use buckle_core::Result;
use buckle_core::error::HumanizableResult;
use std::path::Path;

/// A certificate and key in PEM form, with the certificate's fingerprint.
#[derive(Clone)]
pub struct ServerIdentity {
    pub cert_pem: String,
    pub key_pem: String,
    /// Lower-case hex SHA-256 of the certificate's DER, colon-free.
    pub fingerprint: String,
}

impl std::fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// The identity per `listen.tls`, or `None` for plaintext.
pub fn load(config: &TlsConfig, data_dir: &Path) -> Result<Option<ServerIdentity>> {
    match config.mode {
        TlsMode::None => Ok(None),
        TlsMode::Auto => auto(&data_dir.join("tls")).map(Some),
        TlsMode::Files => {
            let (Some(cert), Some(key)) = (&config.cert, &config.key) else {
                return Err(buckle_core::error::user(
                    "listen.tls.mode is `files`, but `cert` and `key` are not both set.",
                    &["Set listen.tls.cert and listen.tls.key to PEM files, or use mode: auto."],
                ));
            };
            let cert_pem = std::fs::read_to_string(cert).map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("Could not read the TLS certificate {}.", cert.display()),
                    &["Check listen.tls.cert."],
                )
            })?;
            let key_pem = std::fs::read_to_string(key).map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("Could not read the TLS key {}.", key.display()),
                    &["Check listen.tls.key."],
                )
            })?;
            let fingerprint = fingerprint(&cert_pem)?;
            Ok(Some(ServerIdentity {
                cert_pem,
                key_pem,
                fingerprint,
            }))
        }
    }
}

/// Loads `dir/server.crt` + `dir/server.key`, generating them when missing.
pub fn auto(dir: &Path) -> Result<ServerIdentity> {
    let cert_path = dir.join("server.crt");
    let key_path = dir.join("server.key");
    if !(cert_path.is_file() && key_path.is_file()) {
        let generated = rcgen::generate_simple_self_signed(vec![
            "buckle-server".to_string(),
            "localhost".to_string(),
        ])
        .map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "Could not generate the server's TLS certificate.",
                &["Report this issue."],
            )
        })?;
        std::fs::create_dir_all(dir).to_human_error()?;
        std::fs::write(&cert_path, generated.cert.pem()).to_human_error()?;
        std::fs::write(&key_path, generated.signing_key.serialize_pem()).to_human_error()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .to_human_error()?;
        }
        tracing::info!(path = %cert_path.display(), "generated a TLS certificate for the gRPC listener");
    }
    let cert_pem = std::fs::read_to_string(&cert_path).to_human_error()?;
    let key_pem = std::fs::read_to_string(&key_path).to_human_error()?;
    let fingerprint = fingerprint(&cert_pem)?;
    Ok(ServerIdentity {
        cert_pem,
        key_pem,
        fingerprint,
    })
}

/// SHA-256 over the DER of the first certificate in `pem`.
pub fn fingerprint(pem: &str) -> Result<String> {
    use base64::Engine as _;
    let mut inside = false;
    let mut body = String::new();
    for line in pem.lines() {
        let line = line.trim();
        if line.starts_with("-----BEGIN CERTIFICATE") {
            inside = true;
            continue;
        }
        if line.starts_with("-----END CERTIFICATE") {
            break;
        }
        if inside {
            body.push_str(line);
        }
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                "The TLS certificate is not valid PEM.",
                &["Check the certificate file."],
            )
        })?;
    if der.is_empty() {
        return Err(buckle_core::error::user(
            "The TLS certificate file contains no certificate.",
            &["Check the certificate file."],
        ));
    }
    Ok(buckle_core::Digest::of(&der).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_generates_once_and_keeps_a_stable_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let a = auto(dir.path()).unwrap();
        let b = auto(dir.path()).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.fingerprint.len(), 64);
        assert!(a.cert_pem.contains("BEGIN CERTIFICATE"));
    }
}
