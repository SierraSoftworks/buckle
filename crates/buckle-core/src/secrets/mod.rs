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
//!
//! The places that open a secret, all greppable by `Witness`:
//!
//! * [`sinks`] — [`sinks::FileSink`] (rendered files), [`sinks::ProcessEnvSink`]
//!   (child process environment and stdin) and [`sinks::TransportSink`]
//!   (server-to-agent assignment via [`SecretStore::export`], and the agent's
//!   [`cache`]) and [`sinks::ApiSink`] (values stored through a cluster API,
//!   such as Nomad variables). [`sinks::Sink`] is sealed.
//! * the template renderer (`crate::template`), for real renders.
//! * [`redact`], which searches command output for known values.
//!
//! Around them: [`providers`] resolve references (`env`, `file`, and the
//! development-only, allowlisted `exec`); [`SecretField`] is the spec field
//! type for the few task fields that may hold secrets; [`redact_known`]
//! scrubs text Buckle did not render itself.

pub mod cache;
pub mod field;
pub mod providers;
pub mod redact;
pub mod sinks;

#[cfg(test)]
mod leak_tests;

pub use field::SecretField;
pub use redact::redact_known;

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
///
/// ```
/// use buckle_core::secrets::{InstallKey, Secret};
///
/// let key = InstallKey::from_bytes([7u8; 32]);
/// let a = Secret::new("db", "hunter2".into(), &key);
/// let b = Secret::new("db", "hunter2".into(), &key);
/// assert_eq!(a.fingerprint(), b.fingerprint());
/// assert_eq!(format!("{key:?}"), "InstallKey(..)");
/// ```
pub struct InstallKey(SecretBox<[u8; 32]>);

impl Clone for InstallKey {
    fn clone(&self) -> Self {
        Self(SecretBox::new(Box::new(*self.0.expose_secret())))
    }
}

impl InstallKey {
    /// Wraps 32 bytes of key material.
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
        let out = self.mac(&[], value.as_bytes());
        let mut short = [0u8; 4];
        short.copy_from_slice(&out[..4]);
        Fingerprint(short)
    }

    /// A key for `purpose` and `subject`, derived from this one: stable for
    /// as long as this key is, and revealing nothing about it.
    pub fn derive(&self, purpose: &str, subject: &str) -> InstallKey {
        let domain = format!("buckle.derive/{purpose}\0");
        InstallKey::from_bytes(self.mac(domain.as_bytes(), subject.as_bytes()))
    }

    /// The key material, for a sink.
    pub(crate) fn bytes(&self, _: sinks::Witness) -> &[u8; 32] {
        self.0.expose_secret()
    }

    /// Wraps key material received on the wire; `None` unless it is 32 bytes.
    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; 32]>::try_from(bytes).ok().map(Self::from_bytes)
    }

    /// HMAC-SHA256 of `domain || bytes` under this key. Fingerprints use an
    /// empty domain (so they stay stable); other uses, such as the agent's
    /// cache integrity tag, pass a distinct domain string.
    pub(crate) fn mac(&self, domain: &[u8], bytes: &[u8]) -> [u8; 32] {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.0.expose_secret())
            .expect("HMAC accepts any key length");
        mac.update(domain);
        mac.update(bytes);
        mac.finalize().into_bytes().into()
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
    /// The eight lowercase hex characters operators see.
    pub fn as_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parses the eight-hex-character form (as shown in tokens, plans and the
    /// UI). Case-insensitive; anything else is a user error.
    ///
    /// ```
    /// use buckle_core::secrets::Fingerprint;
    ///
    /// let fp = Fingerprint::parse("3F9A00b2").unwrap();
    /// assert_eq!(fp.as_hex(), "3f9a00b2");
    /// assert!(Fingerprint::parse("3f9a").is_err());
    /// ```
    pub fn parse(s: &str) -> crate::Result<Fingerprint> {
        Fingerprint::try_from(s.trim().to_owned()).map_err(|_| {
            crate::error::user(
                format!("'{s}' is not a secret fingerprint."),
                &["A fingerprint is the 8 hexadecimal characters after the '@' in «secret:NAME@FINGERPRINT»."],
            )
        })
    }
}

impl std::str::FromStr for Fingerprint {
    type Err = crate::Error;

    fn from_str(s: &str) -> crate::Result<Self> {
        Fingerprint::parse(s)
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
        Self::with_fingerprint(name, fingerprint, value)
    }

    /// Seals `value` with a fingerprint computed elsewhere (by the server, for a
    /// secret delivered to an agent). Crate-private so that only the transport
    /// and cache paths, which carry the fingerprint alongside the value, use it.
    pub(crate) fn with_fingerprint(
        name: impl Into<Arc<str>>,
        fingerprint: Fingerprint,
        value: String,
    ) -> Self {
        Self {
            name: name.into(),
            fingerprint,
            value: Arc::new(SecretString::from(value)),
        }
    }

    /// The name the secret is declared under in `buckle.yaml`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The keyed fingerprint of the value.
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
///
/// `Debug` lists reference tokens; `Serialize` emits a map of name to
/// fingerprint. Neither ever contains a value.
///
/// ```
/// use buckle_core::secrets::{InstallKey, Secret, SecretStore};
///
/// let key = InstallKey::from_bytes([7u8; 32]);
/// let mut store = SecretStore::new();
/// store.insert(Secret::new("db", "hunter2".into(), &key));
/// let json = serde_json::to_string(&store).unwrap();
/// assert!(json.starts_with(r#"{"db":""#));
/// assert!(!json.contains("hunter2"));
/// assert!(!format!("{store:?}").contains("hunter2"));
/// ```
#[derive(Clone, Default, Debug)]
pub struct SecretStore {
    secrets: IndexMap<String, Secret>,
}

impl Serialize for SecretStore {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(self.secrets.len()))?;
        for (name, secret) in &self.secrets {
            map.serialize_entry(name, &secret.fingerprint())?;
        }
        map.end()
    }
}

impl SecretStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces the secret with this name.
    pub fn insert(&mut self, secret: Secret) {
        self.secrets.insert(secret.name().to_owned(), secret);
    }

    /// The secret with this (exact) name.
    pub fn get(&self, name: &str) -> Option<&Secret> {
        self.secrets.get(name)
    }

    /// Whether a secret with this name is present.
    pub fn contains(&self, name: &str) -> bool {
        self.secrets.contains_key(name)
    }

    /// Secret names, in insertion order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.secrets.keys().map(String::as_str)
    }

    /// Secrets, in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = &Secret> {
        self.secrets.values()
    }

    /// The number of secrets.
    pub fn len(&self) -> usize {
        self.secrets.len()
    }

    /// Whether the store holds no secrets.
    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Opens every secret for transport: `(name, fingerprint, value bytes)`.
    ///
    /// Used by the server when assigning secrets to a host (the result goes
    /// straight into the assignment message) and by the agent when caching
    /// them. Requires a [`sinks::Sink`] so that every call site is greppable;
    /// pass [`sinks::TransportSink`]. The returned bytes are plaintext: hand
    /// them to the wire encoder and drop them.
    ///
    /// ```
    /// use buckle_core::secrets::sinks::TransportSink;
    /// use buckle_core::secrets::{InstallKey, Secret, SecretStore};
    ///
    /// let key = InstallKey::from_bytes([7u8; 32]);
    /// let mut store = SecretStore::new();
    /// store.insert(Secret::new("db", "hunter2".into(), &key));
    /// let wire = store.export(&TransportSink);
    /// let back = SecretStore::import(&key, wire.into_iter().map(|(n, _, v)| (n, v))).unwrap();
    /// assert_eq!(back.get("db"), store.get("db"));
    /// ```
    pub fn export(&self, sink: &dyn sinks::Sink) -> Vec<(String, Fingerprint, Vec<u8>)> {
        self.secrets
            .values()
            .map(|s| {
                (
                    s.name().to_owned(),
                    s.fingerprint(),
                    s.reveal(sink.witness()).as_bytes().to_vec(),
                )
            })
            .collect()
    }

    /// Seals transported values, fingerprinting them with `key`. Use this
    /// when the receiving side's key is the one fingerprints must agree with
    /// (a local run, or an agent sharing the server's key); use
    /// [`SecretStore::import_fingerprinted`] to keep the sender's fingerprints.
    ///
    /// A value that is not UTF-8 is a system error naming the secret: every
    /// exported value came from a `Secret`, so this means corruption.
    pub fn import(
        key: &InstallKey,
        items: impl IntoIterator<Item = (String, Vec<u8>)>,
    ) -> crate::Result<SecretStore> {
        let mut store = SecretStore::new();
        for (name, bytes) in items {
            let value = utf8_value(&name, bytes)?;
            store.insert(Secret::new(name, value, key));
        }
        Ok(store)
    }

    /// Seals transported values under the fingerprints the sender computed.
    ///
    /// This is what an agent in server mode uses for its assignment: plans
    /// show the server's fingerprints, so a rotation yields the same plan
    /// digest on every host that uses the secret (design §11). The agent does
    /// not need the server's [`InstallKey`].
    pub fn import_fingerprinted(
        items: impl IntoIterator<Item = (String, Fingerprint, Vec<u8>)>,
    ) -> crate::Result<SecretStore> {
        let mut store = SecretStore::new();
        for (name, fingerprint, bytes) in items {
            let value = utf8_value(&name, bytes)?;
            store.insert(Secret::with_fingerprint(name, fingerprint, value));
        }
        Ok(store)
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

fn utf8_value(name: &str, bytes: Vec<u8>) -> crate::Result<String> {
    String::from_utf8(bytes).map_err(|_| {
        crate::error::system(
            format!("The transported value of the secret '{name}' is not valid UTF-8."),
            &["The secret was corrupted in transit or in the agent's cache; resynchronise the host with the server."],
        )
    })
}

impl fmt::Display for SecretStore {
    /// The reference tokens, comma-separated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, secret) in self.secrets.values().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            f.write_str(&secret.token())?;
        }
        Ok(())
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

    /// The displayable content.
    pub fn get(&self) -> &T {
        &self.0
    }

    /// Unwraps the displayable content.
    pub fn into_inner(self) -> T {
        self.0
    }

    /// Transforms the content. The caller is responsible for not mixing a
    /// secret value in; the result is still asserted displayable.
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

    /// The length in bytes (safe to show: "412 bytes, contains secrets").
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the content is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Sensitive<Vec<u8>> {
    /// The digest of the real bytes: safe to store and compare for drift.
    pub fn digest(&self) -> crate::Digest {
        crate::Digest::of(&self.0)
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sensitive(..)")
    }
}

impl<T> fmt::Display for Sensitive<T> {
    /// A placeholder, like `Debug`: the content is never formatted.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Sensitive(..)")
    }
}

/// A rendered string that is either safe or sensitive depending on whether the
/// template referenced a secret.
#[derive(Debug, Clone)]
pub enum Rendered {
    /// The template touched no secret; the text is displayable.
    Plain(String),
    /// The template revealed at least one secret.
    Sensitive(Sensitive<String>),
}

impl Rendered {
    /// The digest of the real content, for drift detection.
    pub fn digest(&self) -> crate::Digest {
        match self {
            Rendered::Plain(s) => crate::Digest::of(s.as_bytes()),
            Rendered::Sensitive(s) => s.digest(),
        }
    }

    /// Whether a secret was revealed into this content.
    pub fn is_sensitive(&self) -> bool {
        matches!(self, Rendered::Sensitive(_))
    }

    /// The content when it is displayable, `None` when it is sensitive.
    pub fn as_plain(&self) -> Option<&str> {
        match self {
            Rendered::Plain(s) => Some(s),
            Rendered::Sensitive(_) => None,
        }
    }
}

/// Resolves [`SecretRef`]s into a [`SecretStore`] using the configured providers.
pub struct Resolver {
    key: InstallKey,
    providers: Vec<Box<dyn providers::SecretProvider>>,
}

impl Resolver {
    /// A resolver with no providers, fingerprinting under `key`.
    pub fn new(key: InstallKey) -> Self {
        Self {
            key,
            providers: Vec::new(),
        }
    }

    /// Adds a provider. Providers are consulted in the order added.
    pub fn with_provider(mut self, provider: impl providers::SecretProvider + 'static) -> Self {
        self.providers.push(Box::new(provider));
        self
    }

    /// The key fingerprints are computed under.
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

    /// Resolves one reference with the first provider that handles it.
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
