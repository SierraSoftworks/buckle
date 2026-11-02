//! `validate:` for the file writers and editors (`file.copy`, `file.template`,
//! `file.line`, `file.block`): check the new content with a command before it
//! replaces the destination.
//!
//! The candidate content is written to a private temporary file under
//! `<state_dir>/tmp` (never next to the destination, so planning writes
//! nothing outside Buckle's own scratch space and directory-globbing
//! consumers such as `sudoers.d` never see a half-checked file), the command
//! runs with `{path}` replaced by that file, and the file is always deleted
//! afterwards.
//!
//! Without a `validate` field, critical files get a validator automatically:
//! `visudo -cf {path}` for `/etc/sudoers` and `/etc/sudoers.d/*`, and
//! `sshd -t -f {path}` for files named `sshd_config`. `validate: []` turns
//! validation off. An automatic validator whose program is not installed is
//! skipped; an explicit one is required.

use crate::tasks::sys_exec::state_dir;
use buckle_core::secrets::Rendered;
use buckle_core::secrets::sinks::FileSink;
use buckle_core::system::{Command, FilePerms};
use buckle_core::task::Cx;
use buckle_core::{Digest, Result, error};
use std::path::{Path, PathBuf};

/// A validator to run, and whether the author asked for it explicitly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validator {
    pub argv: Vec<String>,
    pub explicit: bool,
}

/// The automatic validator for a destination path, if it is a critical file.
pub fn automatic(dest: &Path) -> Option<Vec<String>> {
    let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect();
    if dest == Path::new("/etc/sudoers") || dest.starts_with("/etc/sudoers.d") {
        return Some(argv(&["visudo", "-cf", "{path}"]));
    }
    if dest.file_name().is_some_and(|n| n == "sshd_config") {
        return Some(argv(&["sshd", "-t", "-f", "{path}"]));
    }
    None
}

/// The validator for `dest`: the explicit one, none for `validate: []`, or the automatic one.
pub fn validator_for(dest: &Path, explicit: Option<&[String]>) -> Option<Validator> {
    match explicit {
        Some([]) => None,
        Some(argv) => Some(Validator {
            argv: argv.to_vec(),
            explicit: true,
        }),
        None => automatic(dest).map(|argv| Validator {
            argv,
            explicit: false,
        }),
    }
}

/// The scratch path for a candidate of `dest`.
fn scratch_path(cx: &Cx, dest: &Path, digest: &Digest) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    state_dir(cx)
        .join("tmp")
        .join(format!("validate-{}-{name}", digest.short()))
}

/// The candidate content to validate.
pub enum Content<'a> {
    /// A template render (possibly sensitive), written through the file sink.
    Rendered(&'a Rendered),
    /// Bytes from the module or the host (never sensitive).
    Bytes(&'a [u8]),
}

impl Content<'_> {
    fn digest(&self) -> Digest {
        match self {
            Content::Rendered(r) => r.digest(),
            Content::Bytes(b) => Digest::of(b),
        }
    }

    fn is_sensitive(&self) -> bool {
        matches!(self, Content::Rendered(r) if r.is_sensitive())
    }
}

/// Checks `content` (the candidate for `dest`) with the validator. Errors
/// when the validator fails (the destination is untouched); the validator's
/// output is withheld when the content is sensitive.
pub async fn check(
    cx: &Cx,
    dest: &Path,
    explicit: Option<&[String]>,
    content: Content<'_>,
) -> Result<()> {
    let Some(validator) = validator_for(dest, explicit) else {
        return Ok(());
    };
    if !validator.argv.iter().any(|a| a.contains("{path}")) {
        return Err(error::user(
            "The `validate` command does not contain the `{path}` placeholder.",
            &[
                "Add `{path}` where the command takes the file to check, e.g. [\"nginx\", \"-t\", \"-c\", \"{path}\"].",
            ],
        ));
    }
    let Some(program) = validator.argv.first() else {
        return Ok(());
    };
    if !validator.explicit && cx.system.which(program).await?.is_none() {
        tracing::warn!(
            dest = %dest.display(),
            validator = %program,
            "the automatic validator is not installed; skipping validation"
        );
        return Ok(());
    }
    let tmp = scratch_path(cx, dest, &content.digest());
    if let Some(parent) = tmp.parent() {
        cx.system.mkdir_all(parent, &FilePerms::mode(0o700)).await?;
    }
    match &content {
        Content::Rendered(r) => {
            FileSink
                .write_rendered(&*cx.system, &tmp, r, &FilePerms::mode(0o600))
                .await?
        }
        Content::Bytes(b) => {
            cx.system
                .write_atomic(&tmp, b, &FilePerms::mode(0o600))
                .await?
        }
    }
    let substituted: Vec<String> = validator
        .argv
        .iter()
        .map(|a| a.replace("{path}", &tmp.to_string_lossy()))
        .collect();
    let outcome = match Command::from_argv(&substituted) {
        Some(command) => cx.system.run(command).await,
        None => Err(error::user(
            "The `validate` command is empty.",
            &["Give the program and its arguments as a list."],
        )),
    };
    // Always clean up, whatever the validator said.
    let _ = cx.system.remove(&tmp, false).await;
    let out = outcome?;
    if out.success() {
        return Ok(());
    }
    let detail = if content.is_sensitive() {
        "Its output is withheld because the file contains secrets.".to_owned()
    } else {
        let redactor = buckle_core::secrets::redact::Redactor::new(&cx.secrets);
        format!(
            "stderr: {}",
            redactor
                .redact(&super::output_tail(&out.stderr))
                .into_inner()
        )
    };
    Err(error::user(
        format!(
            "The new content for {} failed validation: `{}` {}. {detail}",
            dest.display(),
            validator.argv.join(" "),
            match out.code {
                Some(c) => format!("exited with status {c}"),
                None => "did not exit cleanly".to_owned(),
            },
        ),
        &[
            "Fix the content (or the variables it uses); the destination file was not changed.",
            "Run the validator by hand against the new content to see the full message.",
            "Set `validate: []` to skip an automatic validator.",
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("/etc/sudoers", Some("visudo"))]
    #[case("/etc/sudoers.d/90-deploy", Some("visudo"))]
    #[case("/etc/ssh/sshd_config", Some("sshd"))]
    #[case("/etc/ssh/sshd_config.d/10-x.conf", None)]
    #[case("/etc/hosts", None)]
    fn critical_files_get_an_automatic_validator(#[case] path: &str, #[case] tool: Option<&str>) {
        assert_eq!(
            automatic(Path::new(path)).map(|a| a[0].clone()).as_deref(),
            tool
        );
    }

    #[test]
    fn an_empty_list_disables_and_an_explicit_one_wins() {
        let p = Path::new("/etc/sudoers");
        assert_eq!(validator_for(p, Some(&[])), None);
        let own = vec!["true".to_owned(), "{path}".to_owned()];
        assert_eq!(validator_for(p, Some(&own)).unwrap().argv, own);
        assert!(!validator_for(p, None).unwrap().explicit);
    }
}
