//! The only exits for secret values. A [`Witness`] cannot be constructed
//! outside this module (and the crate-internal template renderer), so every
//! place a plaintext secret leaves its wrapper is here.

use super::{Secret, Sensitive};
use crate::system::{Command, FilePerms, System};
use std::path::Path;

/// Proof that the caller is a sink. No public constructor.
pub struct Witness(());

impl Witness {
    pub(crate) fn new() -> Self {
        Witness(())
    }
}

/// Implemented by the types that may open secrets.
pub trait Sink {
    fn witness(&self) -> Witness {
        Witness(())
    }
}

/// Writes sensitive content to the host filesystem.
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
}

/// Places secrets into a child process's environment.
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
}
