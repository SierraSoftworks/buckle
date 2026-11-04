//! Certificate pinning for servers that generate their own certificate.
//!
//! `buckle server --print-fingerprint` prints the SHA-256 fingerprint of the server's
//! certificate as `sha256:<hex>`; `buckle up --ca-fingerprint`
//! pins it. The [`PinnedCertVerifier`] then accepts exactly that certificate
//! (whatever names it carries, since a generated certificate rarely matches
//! the address agents dial) and still checks the handshake signatures, so only
//! the holder of the pinned certificate's private key can complete a session.

use sha2::{Digest as _, Sha256};
use std::sync::Arc;
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{
    CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{DigitallySignedStruct, SignatureScheme};

/// Parses `sha256:<hex>` (or bare hex, with optional `:` separators) into the
/// 32-byte digest.
pub fn parse_fingerprint(text: &str) -> buckle_core::Result<[u8; 32]> {
    let trimmed = text.trim();
    let hex_part = trimmed
        .strip_prefix("sha256:")
        .or_else(|| trimmed.strip_prefix("SHA256:"))
        .unwrap_or(trimmed);
    let cleaned: String = hex_part.chars().filter(|c| *c != ':').collect();
    let bytes = hex::decode(&cleaned).map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            format!("The pinned certificate fingerprint '{text}' is not valid hex."),
            &["Copy the fingerprint printed by `buckle server --print-fingerprint`, e.g. `sha256:3f1c…`."],
        )
    })?;
    bytes.try_into().map_err(|_| {
        buckle_core::error::user(
            format!("The pinned certificate fingerprint '{text}' is not a SHA-256 digest."),
            &["A SHA-256 fingerprint is 64 hex characters, e.g. `sha256:3f1c…`."],
        )
    })
}

/// The `sha256:<hex>` fingerprint of a DER certificate.
pub fn fingerprint_of(der: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(der)))
}

/// Accepts exactly the server certificate with a given SHA-256 fingerprint.
#[derive(Debug)]
pub struct PinnedCertVerifier {
    fingerprint: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedCertVerifier {
    /// A verifier pinned to `fingerprint` (`sha256:<hex>`).
    pub fn new(fingerprint: &str) -> buckle_core::Result<Self> {
        Ok(Self {
            fingerprint: parse_fingerprint(fingerprint)?,
            algorithms: provider().signature_verification_algorithms,
        })
    }

    /// The verifier as the trait object tonic expects.
    pub fn into_arc(self) -> Arc<dyn ServerCertVerifier> {
        Arc::new(self)
    }
}

fn provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if actual == self.fingerprint {
            Ok(ServerCertVerified::assertion())
        } else {
            tracing::warn!(
                expected = %format!("sha256:{}", hex::encode(self.fingerprint)),
                actual = %fingerprint_of(end_entity.as_ref()),
                "the server presented a certificate that does not match the pinned fingerprint"
            );
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn verify(verifier: &PinnedCertVerifier, der: &[u8]) -> bool {
        verifier
            .verify_server_cert(
                &CertificateDer::from(der.to_vec()),
                &[],
                &ServerName::try_from("buckle.example").expect("valid name"),
                &[],
                UnixTime::now(),
            )
            .is_ok()
    }

    #[test]
    fn accepts_exactly_the_pinned_certificate() {
        let pinned = b"the server certificate (DER)";
        let verifier = PinnedCertVerifier::new(&fingerprint_of(pinned)).expect("valid pin");
        assert!(verify(&verifier, pinned));
        assert!(!verify(&verifier, b"another certificate"));
    }

    #[rstest]
    #[case::prefixed(true)]
    #[case::bare(false)]
    fn fingerprints_parse_with_or_without_prefix(#[case] prefixed: bool) {
        let fp = fingerprint_of(b"cert");
        let text = if prefixed {
            fp.clone()
        } else {
            fp.trim_start_matches("sha256:").to_owned()
        };
        assert_eq!(
            parse_fingerprint(&text).expect("parses").to_vec(),
            hex::decode(fp.trim_start_matches("sha256:")).expect("hex")
        );
    }

    #[rstest]
    #[case("sha256:zz")]
    #[case("sha256:abcd")]
    fn rejects_malformed_fingerprints(#[case] text: &str) {
        assert!(parse_fingerprint(text).is_err());
    }
}
