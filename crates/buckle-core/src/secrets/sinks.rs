//! The only exits for secret values. A [`Witness`] cannot be constructed
//! outside this module (and the crate-internal template renderer), so every
//! place a plaintext secret leaves its wrapper is here.
//!
//! [`Sink`] is sealed: only the sinks defined in this module implement it, so
//! code outside `buckle-core` cannot mint a [`Witness`] by implementing the
//! trait for its own type.

use super::{Fingerprint, InstallKey, Rendered, Secret, Sensitive};
use crate::system::{Command, FilePerms, System};
use std::path::Path;

/// Proof that the caller is a sink. No public constructor.
pub struct Witness(());

impl Witness {
    pub(crate) fn new() -> Self {
        Witness(())
    }
}

mod sealed {
    /// Prevents implementations of [`super::Sink`] outside this module.
    pub trait Sealed {}
    impl Sealed for super::FileSink {}
    impl Sealed for super::ProcessEnvSink {}
    impl Sealed for super::TransportSink {}
    impl Sealed for super::ApiSink {}
}

/// Implemented by the types that may open secrets. Sealed: the
/// implementors are [`FileSink`], [`ProcessEnvSink`], [`TransportSink`] and
/// [`ApiSink`].
pub trait Sink: sealed::Sealed {
    /// A witness that lets this sink call [`Secret::reveal`] or [`Sensitive::expose`].
    fn witness(&self) -> Witness {
        Witness(())
    }
}

/// Writes sensitive content to the host filesystem.
///
/// ```
/// use buckle_core::secrets::sinks::FileSink;
/// use buckle_core::secrets::Rendered;
/// use buckle_core::system::{FilePerms, RealSystem};
///
/// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
/// let dir = tempfile::tempdir().unwrap();
/// let path = dir.path().join("plain.txt");
/// let content = Rendered::Plain("hi\n".into());
/// FileSink
///     .write_rendered(&RealSystem::new(), &path, &content, &FilePerms::mode(0o600))
///     .await
///     .unwrap();
/// assert_eq!(std::fs::read_to_string(&path).unwrap(), "hi\n");
/// # });
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct FileSink;

impl Sink for FileSink {}

impl FileSink {
    /// Writes `content` atomically (temp file + rename) with `perms`.
    pub async fn write(
        &self,
        system: &dyn System,
        path: &Path,
        content: &Sensitive<String>,
        perms: &FilePerms,
    ) -> crate::Result<()> {
        system
            .write_atomic(path, content.expose(self.witness()).as_bytes(), perms)
            .await
    }

    /// Writes sensitive bytes atomically with `perms`.
    pub async fn write_bytes(
        &self,
        system: &dyn System,
        path: &Path,
        content: &Sensitive<Vec<u8>>,
        perms: &FilePerms,
    ) -> crate::Result<()> {
        system
            .write_atomic(path, content.expose(self.witness()), perms)
            .await
    }

    /// Writes the output of [`crate::template::Templater::render_sensitive`],
    /// whichever variant it is, atomically with `perms`. This is what
    /// `file.template` calls so that it need not match on the variant.
    pub async fn write_rendered(
        &self,
        system: &dyn System,
        path: &Path,
        content: &Rendered,
        perms: &FilePerms,
    ) -> crate::Result<()> {
        match content {
            Rendered::Plain(text) => system.write_atomic(path, text.as_bytes(), perms).await,
            Rendered::Sensitive(sensitive) => self.write(system, path, sensitive, perms).await,
        }
    }
}

/// Places secrets into a child process's environment.
///
/// Every method that puts a secret or a [`Sensitive`] value into the
/// environment sets [`Command::sensitive_env`], which tells the executor not
/// to record the command's output verbatim (scrub it with
/// [`super::redact::redact_known`] first).
///
/// ```
/// use buckle_core::secrets::sinks::ProcessEnvSink;
/// use buckle_core::secrets::{InstallKey, Rendered, Secret};
/// use buckle_core::system::Command;
///
/// let secret = Secret::new("db", "hunter2".into(), &InstallKey::from_bytes([7u8; 32]));
/// let mut cmd = Command::new("psql");
/// ProcessEnvSink.inject(&mut cmd, "PGPASSWORD", &secret);
/// assert!(cmd.sensitive_env);
/// assert!(!format!("{cmd:?}").contains("hunter2"));
///
/// let mut plain = Command::new("env");
/// ProcessEnvSink.inject_rendered(&mut plain, "MODE", &Rendered::Plain("prod".into()));
/// assert!(!plain.sensitive_env);
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnvSink;

impl Sink for ProcessEnvSink {}

impl ProcessEnvSink {
    /// Adds `name=<value>` to the command's environment. [`Command`]'s `Debug`
    /// prints environment keys only, so the value stays out of logs.
    pub fn inject(&self, command: &mut Command, name: impl Into<String>, secret: &Secret) {
        command
            .env
            .insert(name.into(), secret.reveal(self.witness()).to_owned());
        command.sensitive_env = true;
    }

    /// Adds a credential (a cluster token) to the environment.
    pub fn inject_credential(
        &self,
        command: &mut Command,
        name: impl Into<String>,
        value: &secrecy::SecretString,
    ) {
        use secrecy::ExposeSecret as _;
        command
            .env
            .insert(name.into(), value.expose_secret().to_owned());
        command.sensitive_env = true;
    }

    /// Adds a rendered sensitive value to the environment.
    pub fn inject_sensitive(
        &self,
        command: &mut Command,
        name: impl Into<String>,
        value: &Sensitive<String>,
    ) {
        command
            .env
            .insert(name.into(), value.expose(self.witness()).clone());
        command.sensitive_env = true;
    }

    /// Adds a rendered value of either variant. A [`Rendered::Plain`] value
    /// is an ordinary variable and leaves [`Command::sensitive_env`] as it
    /// was; a [`Rendered::Sensitive`] one sets it.
    pub fn inject_rendered(
        &self,
        command: &mut Command,
        name: impl Into<String>,
        value: &Rendered,
    ) {
        match value {
            Rendered::Plain(text) => {
                command.env.insert(name.into(), text.clone());
            }
            Rendered::Sensitive(sensitive) => self.inject_sensitive(command, name, sensitive),
        }
    }

    /// Writes sensitive content to the command's stdin (for programs such as
    /// `chpasswd` that read secrets there). Sets [`Command::sensitive_env`]
    /// because the content can be echoed into the output.
    pub fn stdin_sensitive(&self, command: &mut Command, value: &Sensitive<String>) {
        command.stdin = Some(value.expose(self.witness()).as_bytes().to_vec());
        command.sensitive_env = true;
    }
}

/// Opens secrets for transport between processes: used by the server when
/// assigning secrets to a host (see [`super::SecretStore::export`]) and by
/// the agent when caching them (see [`super::cache::SecretCache`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct TransportSink;

impl Sink for TransportSink {}

impl TransportSink {
    /// The key material of `key`, for the wire: the server hands a cluster
    /// target's fingerprint key ([`InstallKey::derive`]) to its executor.
    pub fn key_bytes(&self, key: &InstallKey) -> Vec<u8> {
        key.bytes(self.witness()).to_vec()
    }
}

/// Sends values to a remote API that stores them (a Nomad variable, a
/// Kubernetes Secret), and reads them back for comparison.
///
/// Values read from such an API are [`Sensitive`] from the moment they are
/// parsed ([`ApiSink::receive`]); desired and live values are compared here
/// ([`ApiSink::matches`]) and shown only as keyed fingerprints
/// ([`ApiSink::fingerprint`]), so neither side leaves the executor. A request
/// body is assembled as [`Sensitive`] bytes and opened only at the call that
/// puts it on the wire ([`ApiSink::open`]).
///
/// ```
/// use buckle_core::secrets::sinks::ApiSink;
/// use buckle_core::secrets::{InstallKey, Rendered};
///
/// let key = InstallKey::from_bytes([7u8; 32]);
/// let desired = Rendered::Plain("TZ=Europe/London".into());
/// let live = ApiSink.receive("TZ=Europe/London".into());
/// assert!(ApiSink.matches(&desired, &live));
/// assert_eq!(ApiSink.fingerprint(&key, &desired), ApiSink.fingerprint_live(&key, &live));
///
/// let body = ApiSink.json_body(serde_json::json!({"Path": "nomad/jobs"}), "Items", [("TZ", &desired)]);
/// let wire: serde_json::Value = serde_json::from_slice(ApiSink.open(&body)).unwrap();
/// assert_eq!(wire["Items"]["TZ"], "TZ=Europe/London");
/// ```
#[derive(Debug, Default, Clone, Copy)]
pub struct ApiSink;

impl Sink for ApiSink {}

impl ApiSink {
    /// Wraps a value read back from an API, which may hold a secret.
    pub fn receive(&self, value: String) -> Sensitive<String> {
        Sensitive::new(value)
    }

    /// Whether the desired value equals the live one, revealing neither.
    pub fn matches(&self, desired: &Rendered, live: &Sensitive<String>) -> bool {
        self.text(desired) == live.expose(self.witness()).as_str()
    }

    /// The keyed fingerprint of a desired value: safe to show, digest and stamp.
    pub fn fingerprint(&self, key: &InstallKey, value: &Rendered) -> Fingerprint {
        key.fingerprint(self.text(value))
    }

    /// The keyed fingerprint of a live value, comparable with [`ApiSink::fingerprint`].
    pub fn fingerprint_live(&self, key: &InstallKey, value: &Sensitive<String>) -> Fingerprint {
        key.fingerprint(value.expose(self.witness()))
    }

    /// A JSON request body: `envelope` (an object, secret-free) with `field`
    /// set to an object of string `items`, which may hold secrets.
    pub fn json_body<'a>(
        &self,
        mut envelope: serde_json::Value,
        field: &str,
        items: impl IntoIterator<Item = (&'a str, &'a Rendered)>,
    ) -> Sensitive<Vec<u8>> {
        let object: serde_json::Map<String, serde_json::Value> = items
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_owned(),
                    serde_json::Value::String(self.text(v).to_owned()),
                )
            })
            .collect();
        if let Some(map) = envelope.as_object_mut() {
            map.insert(field.to_owned(), serde_json::Value::Object(object));
        }
        Sensitive::new(serde_json::to_vec(&envelope).unwrap_or_default())
    }

    /// [`ApiSink::json_body`] with each value passed through `encode` (a
    /// Kubernetes Secret's base64 `data`).
    pub fn json_body_encoded<'a>(
        &self,
        mut envelope: serde_json::Value,
        field: &str,
        items: impl IntoIterator<Item = (&'a str, &'a Rendered)>,
        encode: impl Fn(&str) -> String,
    ) -> Sensitive<Vec<u8>> {
        let object: serde_json::Map<String, serde_json::Value> = items
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_owned(),
                    serde_json::Value::String(encode(self.text(v))),
                )
            })
            .collect();
        if let Some(map) = envelope.as_object_mut() {
            map.insert(field.to_owned(), serde_json::Value::Object(object));
        }
        Sensitive::new(serde_json::to_vec(&envelope).unwrap_or_default())
    }

    /// A secret used as the API's credential (a `secret://` cluster
    /// identity), for the client that sends it as a header.
    pub fn credential(&self, secret: &Secret) -> secrecy::SecretString {
        secrecy::SecretString::from(secret.reveal(self.witness()).trim().to_owned())
    }

    /// The bytes of a request body, for the call that sends it and nothing else.
    pub fn open<'a>(&self, body: &'a Sensitive<Vec<u8>>) -> &'a [u8] {
        body.expose(self.witness())
    }

    fn text<'a>(&self, value: &'a Rendered) -> &'a str {
        match value {
            Rendered::Plain(text) => text,
            Rendered::Sensitive(s) => s.expose(self.witness()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::InstallKey;
    use crate::testing::FakeSystem;

    fn secret() -> Secret {
        Secret::new("db", "hunter2".into(), &InstallKey::from_bytes([7u8; 32]))
    }

    #[tokio::test]
    async fn write_rendered_writes_both_variants() {
        let system = FakeSystem::new();
        let perms = FilePerms::mode(0o640);
        FileSink
            .write_rendered(
                &system,
                Path::new("/etc/a"),
                &Rendered::Plain("a".into()),
                &perms,
            )
            .await
            .unwrap();
        FileSink
            .write_rendered(
                &system,
                Path::new("/etc/b"),
                &Rendered::Sensitive(Sensitive::new("b=hunter2".into())),
                &perms,
            )
            .await
            .unwrap();
        assert_eq!(system.file_string("/etc/a").as_deref(), Some("a"));
        assert_eq!(system.file_string("/etc/b").as_deref(), Some("b=hunter2"));
    }

    #[test]
    fn every_secret_entry_into_a_command_marks_it_sensitive() {
        let sink = ProcessEnvSink;
        let mut a = Command::new("x");
        sink.inject(&mut a, "A", &secret());
        let mut b = Command::new("x");
        sink.inject_sensitive(&mut b, "B", &Sensitive::new("v".into()));
        let mut c = Command::new("x");
        sink.inject_rendered(
            &mut c,
            "C",
            &Rendered::Sensitive(Sensitive::new("v".into())),
        );
        let mut d = Command::new("x");
        sink.stdin_sensitive(&mut d, &Sensitive::new("v".into()));
        for cmd in [&a, &b, &c, &d] {
            assert!(cmd.sensitive_env, "{cmd:?}");
        }
        assert_eq!(a.env["A"], "hunter2");
        assert_eq!(c.env["C"], "v");
        assert_eq!(d.stdin.as_deref(), Some(&b"v"[..]));

        let mut plain = Command::new("x");
        sink.inject_rendered(&mut plain, "P", &Rendered::Plain("p".into()));
        assert!(!plain.sensitive_env);
        assert_eq!(plain.env["P"], "p");
        // A plain value never clears the flag set by an earlier secret.
        sink.inject_rendered(&mut a, "P", &Rendered::Plain("p".into()));
        assert!(a.sensitive_env);
    }
}
