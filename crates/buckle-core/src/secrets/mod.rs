//! Secrets that cannot leak by accident.
//!
//! Module authors and operators are trusted; the threat is accidental exposure
//! in the UI, logs, telemetry, plans and state. So the type system makes the
//! accident impossible: a [`Secret`] formats as its reference token, serialises
//! as its reference token, and gives up its value only to code holding a
//! [`sinks::Witness`], which only the sinks in this crate can construct.
//!
//! Rendered content that touched a secret is [`Sensitive`] and has the same
//! property; content proven secret-free (or rendered with tokens in place of
//! values) is [`Redacted`] and may be shown anywhere.

pub mod providers;
pub mod sinks;

use crate::model::SecretRef;
use hmac::{Hmac, KeyInit, Mac};
use indexmap::IndexMap;
use secrecy::{ExposeSecret, SecretBox, SecretString};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fmt;
use std::sync::Arc;

/// The key used to fingerprint secret values. Fingerprints are HMAC-SHA256 so
/// that a low-entropy secret cannot be brute-forced from its fingerprint.
pub struct InstallKey(SecretBox<[u8; 32]>);

impl Clone for InstallKey {
    fn clone(&self) -> Self {
        Self(SecretBox::new(Box::new(*self.0.expose_secret())))
    }
}

impl InstallKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(SecretBox::new(Box::new(bytes)))
    }

    /// A fresh random key.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        Self::from_bytes(bytes)
    }

    /// Loads the key from `path`, creating it (mode 0600) when missing.
    pub fn load_or_create(path: &std::path::Path) -> crate::Result<Self> {
        use crate::error::HumanizableResult;
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() == 32 => {
                let mut arr = [0u8; 32];
                arr.copy_from_slice(&bytes);
                Ok(Self::from_bytes(arr))
            }
            Ok(_) => Err(crate::error::system(
                format!(
                    "The install key at {} is corrupt (unexpected length).",
                    path.display()
                ),
                &["Delete the file to generate a new key; existing fingerprints will change."],
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let key = Self::generate();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).to_human_error()?;
                }
                std::fs::write(path, key.0.expose_secret()).to_human_error()?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                        .to_human_error()?;
                }
                Ok(key)
            }
            Err(e) => Err(e).to_human_error(),
        }
    }

    fn fingerprint(&self, value: &str) -> Fingerprint {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.expose_secret())
            .expect("HMAC accepts any key length");
        mac.update(value.as_bytes());
        let out = mac.finalize().into_bytes();
        let mut short = [0u8; 4];
        short.copy_from_slice(&out[..4]);
        Fingerprint(short)
    }
}

impl fmt::Debug for InstallKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("InstallKey(..)")
    }
}

/// A short, keyed fingerprint of a secret value. Changes when the value changes;
/// safe to show, log and diff.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct Fingerprint([u8; 4]);

impl Fingerprint {
    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl From<Fingerprint> for String {
    fn from(f: Fingerprint) -> String {
        f.as_hex()
    }
}

impl TryFrom<String> for Fingerprint {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        let bytes = hex::decode(&s).map_err(|e| e.to_string())?;
        let arr: [u8; 4] = bytes
            .try_into()
            .map_err(|_| "a fingerprint is 8 hex characters".to_string())?;
        Ok(Fingerprint(arr))
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_hex())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.as_hex())
    }
}

/// A resolved secret. See the module docs for the guarantees.
#[derive(Clone)]
pub struct Secret {
    name: Arc<str>,
    fingerprint: Fingerprint,
    value: Arc<SecretString>,
}

impl Secret {
    /// Seals `value` under `name`, fingerprinting it with `key`.
    pub fn new(name: impl Into<Arc<str>>, value: String, key: &InstallKey) -> Self {
        let fingerprint = key.fingerprint(&value);
        Self {
            name: name.into(),
            fingerprint,
            value: Arc::new(SecretString::from(value)),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    /// The reference token shown wherever the value must not appear:
    /// `«secret:NAME@FINGERPRINT»`.
    pub fn token(&self) -> String {
        format!("\u{ab}secret:{}@{}\u{bb}", self.name, self.fingerprint)
    }

    /// The only way to read the value. [`sinks::Witness`] has a private
    /// constructor, so every call site is in this crate's `sinks` or
    /// `template` module and is greppable.
    pub fn reveal(&self, _witness: sinks::Witness) -> &str {
        self.value.expose_secret()
    }

    /// The value as a filt-rs secret, which compares like a string and displays
    /// as `[REDACTED]`.
    pub fn as_filter_value(&self) -> filt_rs::FilterValue<'static> {
        filt_rs::FilterValue::Secret(SecretString::from(self.value.expose_secret().to_owned()))
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.token())
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.token())
    }
}

impl Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.token())
    }
}

impl PartialEq for Secret {
    /// Two secrets are equal when they are the same named value (same fingerprint).
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name && self.fingerprint == other.fingerprint
    }
}

/// The resolved secrets available to a host or a local run.
#[derive(Clone, Default, Debug)]
pub struct SecretStore {
    secrets: IndexMap<String, Secret>,
}

impl SecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, secret: Secret) {
        self.secrets.insert(secret.name().to_owned(), secret);
    }

    pub fn get(&self, name: &str) -> Option<&Secret> {
        self.secrets.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.secrets.contains_key(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.secrets.keys().map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Secret> {
        self.secrets.values()
    }

    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// The subset of secrets with the given names (missing names are skipped).
    pub fn subset<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> SecretStore {
        let mut out = SecretStore::new();
        for n in names {
            if let Some(s) = self.secrets.get(n) {
                out.insert(s.clone());
            }
        }
        out
    }
}

/// Content that is safe to display: it either never contained a secret or had
/// every secret replaced by its reference token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Redacted<T>(T);

impl<T> Redacted<T> {
    /// Wraps content the caller asserts contains no secret value. Use this for
    /// data read from the configuration repository or produced from facts;
    /// anything that passed through a secret-aware render arrives already wrapped.
    pub fn plain(value: T) -> Self {
        Redacted(value)
    }

    pub(crate) fn new(value: T) -> Self {
        Redacted(value)
    }

    pub fn get(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Redacted<U> {
        Redacted(f(self.0))
    }
}

impl<T: fmt::Debug> fmt::Debug for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for Redacted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: AsRef<str>> AsRef<str> for Redacted<T> {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

/// Content that contained at least one secret value. It can be written to disk
/// through a sink, compared by digest, or exposed to a witness; never printed.
#[derive(Clone)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    pub(crate) fn new(value: T) -> Self {
        Sensitive(value)
    }

    /// Reads the value. Requires a witness, like [`Secret::reveal`].
    pub fn expose(&self, _witness: sinks::Witness) -> &T {
        &self.0
    }
}

impl Sensitive<String> {
    /// The digest of the real content: safe to store and compare for drift.
    pub fn digest(&self) -> crate::Digest {
        crate::Digest::of(self.0.as_bytes())
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Sensitive<Vec<u8>> {
    pub fn digest(&self) -> crate::Digest {
        crate::Digest::of(&self.0)
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sensitive(..)")
    }
}

/// A rendered string that is either safe or sensitive depending on whether the
/// template referenced a secret.
#[derive(Debug, Clone)]
pub enum Rendered {
    Plain(String),
    Sensitive(Sensitive<String>),
}

impl Rendered {
    pub fn digest(&self) -> crate::Digest {
        match self {
            Rendered::Plain(s) => crate::Digest::of(s.as_bytes()),
            Rendered::Sensitive(s) => s.digest(),
        }
    }

    pub fn is_sensitive(&self) -> bool {
        matches!(self, Rendered::Sensitive(_))
    }
}

/// Resolves [`SecretRef`]s into a [`SecretStore`] using the configured providers.
pub struct Resolver {
    key: InstallKey,
    providers: Vec<Box<dyn providers::SecretProvider>>,
}

impl Resolver {
    pub fn new(key: InstallKey) -> Self {
        Self {
            key,
            providers: Vec::new(),
        }
    }

    pub fn with_provider(mut self, provider: impl providers::SecretProvider + 'static) -> Self {
        self.providers.push(Box::new(provider));
        self
    }

    pub fn key(&self) -> &InstallKey {
        &self.key
    }

    /// Resolves every reference. A reference no provider handles, or that a
    /// provider finds missing, is a user error naming the secret.
    pub async fn resolve_all(
        &self,
        refs: &IndexMap<String, SecretRef>,
    ) -> crate::Result<SecretStore> {
        let mut store = SecretStore::new();
        for (name, reference) in refs {
            store.insert(self.resolve_one(name, reference).await?);
        }
        Ok(store)
    }

    pub async fn resolve_one(&self, name: &str, reference: &SecretRef) -> crate::Result<Secret> {
        for provider in &self.providers {
            if !provider.handles(reference) {
                continue;
            }
            return match provider.resolve(name, reference).await? {
                Some(value) => Ok(Secret::new(name.to_owned(), value, &self.key)),
                None => Err(crate::error::user(
                    format!(
                        "The secret '{name}' could not be found by the {} provider.",
                        provider.kind()
                    ),
                    &[
                        "Check the secret reference in buckle.yaml and that the value exists where it points.",
                    ],
                )),
            };
        }
        Err(crate::error::user(
            format!("No secret provider is configured that can resolve the secret '{name}'."),
            &[
                "Enable the matching provider (env, file or exec) in the server or agent configuration.",
            ],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> InstallKey {
        InstallKey::from_bytes([7u8; 32])
    }

    #[test]
    fn debug_display_and_serde_show_only_the_token() {
        let s = Secret::new("db_password", "hunter2".into(), &key());
        let token = s.token();
        assert!(token.starts_with("\u{ab}secret:db_password@"));
        for rendered in [
            format!("{s:?}"),
            format!("{s}"),
            serde_json::to_string(&s).unwrap(),
            serde_yaml::to_string(&s).unwrap(),
        ] {
            assert!(!rendered.contains("hunter2"), "leaked in {rendered}");
            assert!(rendered.contains("secret:db_password@"));
        }
    }

    #[test]
    fn fingerprint_changes_with_value_and_key() {
        let a = Secret::new("x", "one".into(), &key());
        let b = Secret::new("x", "two".into(), &key());
        let c = Secret::new("x", "one".into(), &InstallKey::from_bytes([9u8; 32]));
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_eq!(
            a.fingerprint(),
            Secret::new("x", "one".into(), &key()).fingerprint()
        );
        assert_eq!(a.fingerprint().as_hex().len(), 8);
    }

    #[test]
    fn fingerprint_round_trips_through_serde() {
        let f = Secret::new("x", "v".into(), &key()).fingerprint();
        let json = serde_json::to_string(&f).unwrap();
        let back: Fingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn sensitive_debug_never_shows_content() {
        let s = Sensitive::new("top secret".to_string());
        assert_eq!(format!("{s:?}"), "Sensitive(..)");
        assert_eq!(s.digest(), crate::Digest::of(b"top secret"));
    }

    #[test]
    fn install_key_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("install.key");
        let a = InstallKey::load_or_create(&path).unwrap();
        let b = InstallKey::load_or_create(&path).unwrap();
        let s = "value".to_string();
        assert_eq!(
            Secret::new("n", s.clone(), &a).fingerprint(),
            Secret::new("n", s, &b).fingerprint()
        );
    }

    #[tokio::test]
    async fn resolver_reports_unresolvable_references_as_user_errors() {
        let resolver = Resolver::new(key());
        let mut refs = IndexMap::new();
        refs.insert(
            "x".to_string(),
            SecretRef::Env {
                env: "DEFINITELY_UNSET_BUCKLE_TEST".into(),
            },
        );
        let err = resolver.resolve_all(&refs).await.unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        assert!(err.description().contains("'x'"));
    }
}
