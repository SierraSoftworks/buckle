//! The only exits for secret values. A [`Witness`] cannot be constructed
//! outside this module (and the crate-internal template renderer), so every
//! place a plaintext secret leaves its wrapper is here.
//!
//! [`Sink`] is sealed: only the sinks defined in this module implement it, so
//! code outside `buckle-core` cannot mint a [`Witness`] by implementing the
//! trait for its own type.

use super::{Rendered, Secret, Sensitive};
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
}

/// Implemented by the types that may open secrets. Sealed: the
/// implementors are [`FileSink`], [`ProcessEnvSink`] and [`TransportSink`].
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
