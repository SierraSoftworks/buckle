//! Secret providers turn a [`SecretRef`] into a value. The server resolves
//! secrets for agents; a local run resolves them itself.

use crate::model::SecretRef;
use async_trait::async_trait;

#[async_trait]
pub trait SecretProvider: Send + Sync {
    /// Short name used in errors and logs: `env`, `file`, `exec`.
    fn kind(&self) -> &'static str;

    /// Whether this provider resolves references of this shape.
    fn handles(&self, reference: &SecretRef) -> bool;

    /// Resolves the reference. `Ok(None)` means "handled, but no such secret".
    async fn resolve(&self, name: &str, reference: &SecretRef) -> crate::Result<Option<String>>;
}

/// Reads secrets from the resolving process's environment, any variable.
/// Right for a local run, where the environment is the operator's own; a
/// process whose environment also holds its own credentials (the server)
/// uses [`ScopedEnvProvider`].
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvProvider;

#[async_trait]
impl SecretProvider for EnvProvider {
    fn kind(&self) -> &'static str {
        "env"
    }

    fn handles(&self, reference: &SecretRef) -> bool {
        matches!(reference, SecretRef::Env { .. })
    }

    async fn resolve(&self, _name: &str, reference: &SecretRef) -> crate::Result<Option<String>> {
        let SecretRef::Env { env: var } = reference else {
            return Ok(None);
        };
        Ok(std::env::var(var).ok())
    }
}

/// Reads secrets from the environment, confined by name.
///
/// Patterns are exact variable names or a prefix ending in `*`
/// (`BUCKLE_*`). A variable matching `deny` is never read; when `allow` is
/// set, only variables matching it are. Deny wins.
///
/// ```
/// use buckle_core::secrets::providers::ScopedEnvProvider;
///
/// let p = ScopedEnvProvider {
///     allow: None,
///     deny: vec!["BUCKLE_*".into(), "OIDC_CLIENT_SECRET".into()],
/// };
/// assert!(p.permits("NOMAD_TOKEN"));
/// assert!(!p.permits("BUCKLE_BOOTSTRAP_TOKEN"));
/// assert!(!p.permits("OIDC_CLIENT_SECRET"));
/// ```
#[derive(Debug, Default, Clone)]
pub struct ScopedEnvProvider {
    /// When set, the only variables that may be read.
    pub allow: Option<Vec<String>>,
    /// Variables that may never be read.
    pub deny: Vec<String>,
}

fn env_pattern_matches(pattern: &str, var: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => var.starts_with(prefix),
        None => pattern == var,
    }
}

impl ScopedEnvProvider {
    /// Whether `var` may be read.
    pub fn permits(&self, var: &str) -> bool {
        if self.deny.iter().any(|p| env_pattern_matches(p, var)) {
            return false;
        }
        match &self.allow {
            Some(allow) => allow.iter().any(|p| env_pattern_matches(p, var)),
            None => true,
        }
    }
}

#[async_trait]
impl SecretProvider for ScopedEnvProvider {
    fn kind(&self) -> &'static str {
        "env"
    }

    fn handles(&self, reference: &SecretRef) -> bool {
        matches!(reference, SecretRef::Env { .. })
    }

    async fn resolve(&self, name: &str, reference: &SecretRef) -> crate::Result<Option<String>> {
        let SecretRef::Env { env: var } = reference else {
            return Ok(None);
        };
        if !self.permits(var) {
            return Err(crate::error::user(
                format!(
                    "The secret '{name}' reads the environment variable `{var}`, which this server does not hand out."
                ),
                &[
                    "The server's own credentials (BUCKLE_*, the variables server.yaml interpolates, and those listed under secrets.providers.env.deny) are never available as fleet secrets.",
                    "Deliver the value under another variable name, or through the file provider.",
                ],
            ));
        }
        Ok(std::env::var(var).ok())
    }
}

/// Reads secrets from files (one value per file, a single trailing newline trimmed).
/// Typically fed by a Nomad `template` stanza rendering Nomad Variables.
#[derive(Debug, Clone, Default)]
pub struct FileProvider {
    /// When set, relative paths resolve against this root, and a path that
    /// leaves it is refused: absolute paths elsewhere, `..` components, and
    /// symbolic links that resolve outside the (canonicalised) root.
    pub root: Option<std::path::PathBuf>,
}

fn outside_root(name: &str) -> crate::Error {
    crate::error::user(
        format!("The secret '{name}' points outside the configured secrets root."),
        &[
            "Keep secret files under the `secrets.providers.file.root` directory, without `..` or links that leave it.",
        ],
    )
}

fn unreadable(e: std::io::Error, name: &str) -> crate::Error {
    crate::error::wrap_user(
        e,
        format!("The secret file for '{name}' could not be read."),
        &["Check the file's permissions and that the path in buckle.yaml is correct."],
    )
}

/// Resolves `path` under `root`, following links, and refuses anything that
/// ends up outside it. `Ok(None)` when the file (or the root) does not exist.
async fn confine(
    name: &str,
    root: &std::path::Path,
    path: &std::path::Path,
) -> crate::Result<Option<std::path::PathBuf>> {
    use std::path::Component;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(outside_root(name));
    }
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let real_root = tokio::fs::canonicalize(root).await;
    let lexically_inside = joined.starts_with(root)
        || real_root
            .as_ref()
            .is_ok_and(|real_root| joined.starts_with(real_root));
    if !lexically_inside {
        return Err(outside_root(name));
    }
    let real_root = match real_root {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e, name)),
    };
    let real = match tokio::fs::canonicalize(&joined).await {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(unreadable(e, name)),
    };
    if !real.starts_with(&real_root) {
        return Err(outside_root(name));
    }
    Ok(Some(real))
}

#[async_trait]
impl SecretProvider for FileProvider {
    fn kind(&self) -> &'static str {
        "file"
    }

    fn handles(&self, reference: &SecretRef) -> bool {
        matches!(reference, SecretRef::File { .. })
    }

    async fn resolve(&self, name: &str, reference: &SecretRef) -> crate::Result<Option<String>> {
        let SecretRef::File { file: path } = reference else {
            return Ok(None);
        };
        let path = match &self.root {
            Some(root) => match confine(name, root, path).await? {
                Some(p) => p,
                None => return Ok(None),
            },
            None => path.clone(),
        };
        match tokio::fs::read_to_string(&path).await {
            Ok(mut content) => {
                if content.ends_with('\n') {
                    content.pop();
                    if content.ends_with('\r') {
                        content.pop();
                    }
                }
                Ok(Some(content))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(unreadable(e, name)),
        }
    }
}

/// Runs a program and uses its stdout as the value: `{ exec: [op, read, "op://vault/item"] }`.
///
/// **Development only.** Design §06: production secrets arrive through the
/// `file` and `env` providers (Nomad renders them); `exec` exists so that a
/// developer can point a local run at a password manager CLI. It is off unless
/// the server or agent configuration lists the program in an allowlist, and an
/// empty allowlist (the default) refuses everything.
///
/// * The program (`exec[0]`) must equal an allowlist entry exactly. A bare
///   name such as `op` is looked up on `PATH`; an absolute path pins one
///   binary. `op` does not permit `/tmp/op`.
/// * The value is stdout with one trailing newline (`\n` or `\r\n`) trimmed.
/// * A non-zero exit is a user error that includes stderr but never stdout,
///   because stdout may be (part of) the secret.
/// * The child runs with stdin closed and is killed after the timeout
///   (30 seconds by default).
///
/// ```
/// use buckle_core::model::SecretRef;
/// use buckle_core::secrets::providers::{ExecProvider, SecretProvider};
///
/// let provider = ExecProvider::new(vec!["op".into()]);
/// assert!(provider.allows("op"));
/// assert!(!provider.allows("/tmp/op"));
/// assert!(provider.handles(&SecretRef::Exec { exec: vec!["op".into()] }));
/// ```
#[derive(Debug, Clone)]
pub struct ExecProvider {
    allow: Vec<String>,
    timeout: std::time::Duration,
}

impl Default for ExecProvider {
    /// An exec provider with an empty allowlist: every program is refused.
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl ExecProvider {
    /// The default time a secret program may run before it is killed.
    pub const DEFAULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

    /// A provider permitted to run exactly the programs in `allow`.
    pub fn new(allow: Vec<String>) -> Self {
        Self {
            allow,
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Overrides how long a program may run.
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Whether `program` (an `exec[0]`) is on the allowlist.
    pub fn allows(&self, program: &str) -> bool {
        self.allow.iter().any(|a| a == program)
    }

    /// The configured allowlist.
    pub fn allowlist(&self) -> &[String] {
        &self.allow
    }
}

/// Most of a program's stderr that is quoted in an error.
const STDERR_LIMIT: usize = 4096;

fn quote_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    if text.is_empty() {
        return "(no output on stderr)".to_owned();
    }
    if text.len() <= STDERR_LIMIT {
        return text.to_owned();
    }
    let mut cut = STDERR_LIMIT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}… (truncated)", &text[..cut])
}

#[async_trait]
impl SecretProvider for ExecProvider {
    fn kind(&self) -> &'static str {
        "exec"
    }

    fn handles(&self, reference: &SecretRef) -> bool {
        matches!(reference, SecretRef::Exec { .. })
    }

    async fn resolve(&self, name: &str, reference: &SecretRef) -> crate::Result<Option<String>> {
        let SecretRef::Exec { exec } = reference else {
            return Ok(None);
        };
        let Some((program, args)) = exec.split_first() else {
            return Err(crate::error::user(
                format!("The secret '{name}' has an empty `exec` command."),
                &[
                    "Give the program and its arguments, e.g. `{ exec: [op, read, \"op://vault/item/field\"] }`.",
                ],
            ));
        };
        if !self.allows(program) {
            return Err(crate::error::user(
                format!(
                    "The secret '{name}' runs '{program}', which is not on the exec provider's allowlist."
                ),
                &[
                    "Add the program to `secrets.providers.exec.allow` in the server or agent configuration (development only).",
                    "In production, deliver secrets with the file or env providers instead.",
                ],
            ));
        }

        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|e| {
            crate::error::wrap_user(
                e,
                format!("The program '{program}' for the secret '{name}' could not be started."),
                &["Check that the program is installed and on the PATH of the resolving process."],
            )
        })?;
        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(crate::error::wrap_system(
                    e,
                    format!("Waiting for '{program}' to produce the secret '{name}' failed."),
                    &[
                        "Read the internal error message for details and report the issue if it persists.",
                    ],
                ));
            }
            Err(_) => {
                return Err(crate::error::user(
                    format!(
                        "The program '{program}' for the secret '{name}' did not finish within {}s.",
                        self.timeout.as_secs()
                    ),
                    &[
                        "Check whether the program is waiting for interactive input (for example a password manager that needs unlocking).",
                    ],
                ));
            }
        };
        if !output.status.success() {
            let status = match output.status.code() {
                Some(code) => format!("exit code {code}"),
                None => "a signal".to_owned(),
            };
            // Never include stdout: it may hold the secret, or part of it.
            return Err(crate::error::user(
                format!(
                    "The program '{program}' for the secret '{name}' failed with {status}: {}",
                    quote_stderr(&output.stderr)
                ),
                &[
                    "Run the command by hand to check that it prints the secret, e.g. that the password manager is signed in.",
                ],
            ));
        }
        let mut value = String::from_utf8(output.stdout).map_err(|_| {
            crate::error::user(
                format!("The program '{program}' printed a value for the secret '{name}' that is not valid UTF-8."),
                &["Secrets must be text; encode binary values (for example as base64) before printing them."],
            )
        })?;
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        Ok(Some(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("value\n", "value")]
    #[case("value\r\n", "value")]
    #[case("value", "value")]
    #[case("two\nlines\n", "two\nlines")]
    #[tokio::test]
    async fn file_provider_trims_one_trailing_newline(
        #[case] content: &str,
        #[case] expected: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s");
        std::fs::write(&path, content).unwrap();
        let got = FileProvider::default()
            .resolve("s", &SecretRef::File { file: path })
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(expected));
    }

    #[tokio::test]
    async fn file_provider_refuses_paths_outside_its_root() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FileProvider {
            root: Some(dir.path().join("secrets")),
        };
        let err = provider
            .resolve(
                "x",
                &SecretRef::File {
                    file: "/etc/passwd".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
    }

    #[rstest]
    #[case::parent_relative("../outside")]
    #[case::parent_inside("sub/../../outside")]
    #[case::link_out("link")]
    #[case::absolute_elsewhere("ABS")]
    #[tokio::test]
    async fn file_provider_refuses_every_way_out_of_its_root(#[case] reference: &str) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("secrets");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(dir.path().join("outside"), "server-only").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path().join("outside"), root.join("link")).unwrap();
        #[cfg(not(unix))]
        if reference == "link" {
            return;
        }
        let file: std::path::PathBuf = if reference == "ABS" {
            dir.path().join("outside")
        } else {
            reference.into()
        };
        let err = FileProvider { root: Some(root) }
            .resolve("x", &SecretRef::File { file })
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        assert!(err.message().contains("outside"), "{}", err.message());
    }

    #[tokio::test]
    async fn file_provider_reads_inside_its_root_and_through_links_that_stay_inside() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("secrets");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/tok"), "inside\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("sub/tok"), root.join("alias")).unwrap();
        let provider = FileProvider {
            root: Some(root.clone()),
        };
        for file in ["sub/tok", "alias"] {
            #[cfg(not(unix))]
            if file == "alias" {
                continue;
            }
            let got = provider
                .resolve("x", &SecretRef::File { file: file.into() })
                .await
                .unwrap();
            assert_eq!(got.as_deref(), Some("inside"), "{file}");
        }
        let absolute = provider
            .resolve(
                "x",
                &SecretRef::File {
                    file: root.join("sub/tok"),
                },
            )
            .await
            .unwrap();
        assert_eq!(absolute.as_deref(), Some("inside"));
        let missing = provider
            .resolve(
                "x",
                &SecretRef::File {
                    file: "nope".into(),
                },
            )
            .await
            .unwrap();
        assert!(missing.is_none());
    }

    #[tokio::test]
    async fn scoped_env_provider_refuses_denied_and_unlisted_variables() {
        // SAFETY: the variables are unique to this test.
        unsafe {
            std::env::set_var("BUCKLE_TEST_SCOPED_OWN", "server");
            std::env::set_var("BUCKLE_TEST_SCOPED_FLEET", "fleet");
        }
        let env = |v: &str| SecretRef::Env { env: v.into() };
        let deny = ScopedEnvProvider {
            allow: None,
            deny: vec!["BUCKLE_TEST_SCOPED_OWN".into()],
        };
        assert!(
            deny.resolve("x", &env("BUCKLE_TEST_SCOPED_OWN"))
                .await
                .is_err()
        );
        assert_eq!(
            deny.resolve("x", &env("BUCKLE_TEST_SCOPED_FLEET"))
                .await
                .unwrap()
                .as_deref(),
            Some("fleet")
        );
        let allow = ScopedEnvProvider {
            allow: Some(vec!["FLEET_*".into()]),
            deny: vec![],
        };
        let err = allow
            .resolve("x", &env("BUCKLE_TEST_SCOPED_FLEET"))
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        assert!(err.message().contains("BUCKLE_TEST_SCOPED_FLEET"));
        assert!(!err.message().contains("fleet\""), "never the value");
    }

    #[tokio::test]
    async fn file_provider_reports_missing_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let got = FileProvider::default()
            .resolve(
                "x",
                &SecretRef::File {
                    file: dir.path().join("nope"),
                },
            )
            .await
            .unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn env_provider_reads_variables() {
        // SAFETY: tests in this module run single-threaded with respect to this variable.
        unsafe { std::env::set_var("BUCKLE_TEST_SECRET_ENV", "shh") };
        let got = EnvProvider
            .resolve(
                "x",
                &SecretRef::Env {
                    env: "BUCKLE_TEST_SECRET_ENV".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some("shh"));
        assert!(!EnvProvider.handles(&SecretRef::File { file: "/x".into() }));
    }

    fn exec(argv: &[&str]) -> SecretRef {
        SecretRef::Exec {
            exec: argv.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[cfg(unix)]
    #[rstest]
    #[case("printf 'tok3n\\n'", "tok3n")]
    #[case("printf 'tok3n\\r\\n'", "tok3n")]
    #[case("printf 'tok3n'", "tok3n")]
    #[case("printf 'a\\nb\\n\\n'", "a\nb\n")]
    #[tokio::test]
    async fn exec_provider_runs_allowlisted_programs(#[case] script: &str, #[case] expected: &str) {
        let got = ExecProvider::new(vec!["sh".into()])
            .resolve("x", &exec(&["sh", "-c", script]))
            .await
            .unwrap();
        assert_eq!(got.as_deref(), Some(expected));
    }

    #[tokio::test]
    async fn exec_provider_refuses_programs_off_the_allowlist() {
        let provider = ExecProvider::new(vec!["op".into()]);
        for argv in [&["sh", "-c", "echo hi"][..], &["/tmp/op", "read"][..]] {
            let err = provider.resolve("db", &exec(argv)).await.unwrap_err();
            assert!(err.is(crate::error::Kind::User));
            assert!(err.message().contains(argv[0]), "{}", err.message());
            assert!(err.message().contains("'db'"));
        }
        let err = ExecProvider::default()
            .resolve("db", &exec(&["sh"]))
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
    }

    #[tokio::test]
    async fn exec_provider_rejects_an_empty_command() {
        let err = ExecProvider::new(vec!["sh".into()])
            .resolve("db", &exec(&[]))
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_provider_failure_quotes_stderr_but_never_stdout() {
        let err = ExecProvider::new(vec!["sh".into()])
            .resolve(
                "db",
                &exec(&[
                    "sh",
                    "-c",
                    "echo partial-SECRET-stdout; echo 'vault is locked' >&2; exit 3",
                ]),
            )
            .await
            .unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        let shown = format!("{} {} {err:?}", err.message(), err.description());
        assert!(shown.contains("vault is locked"), "{shown}");
        assert!(shown.contains("exit code 3"), "{shown}");
        assert!(!shown.contains("partial-SECRET-stdout"), "{shown}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exec_provider_reports_missing_programs_and_timeouts() {
        let missing = ExecProvider::new(vec!["buckle-no-such-program".into()])
            .resolve("db", &exec(&["buckle-no-such-program"]))
            .await
            .unwrap_err();
        assert!(missing.is(crate::error::Kind::User));
        assert!(missing.message().contains("buckle-no-such-program"));

        let slow = ExecProvider::new(vec!["sleep".into()])
            .with_timeout(std::time::Duration::from_millis(100))
            .resolve("db", &exec(&["sleep", "5"]))
            .await
            .unwrap_err();
        assert!(slow.is(crate::error::Kind::User));
        assert!(slow.message().contains("did not finish"));
    }

    #[test]
    fn stderr_quotes_are_bounded() {
        let long = "é".repeat(STDERR_LIMIT);
        let quoted = quote_stderr(long.as_bytes());
        assert!(quoted.len() < STDERR_LIMIT + 32);
        assert!(quoted.ends_with("(truncated)"));
        assert_eq!(quote_stderr(b"  \n"), "(no output on stderr)");
    }
}
