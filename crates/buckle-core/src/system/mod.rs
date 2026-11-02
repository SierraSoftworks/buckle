//! Everything a task may do to the host, behind one trait.
//!
//! Tasks never touch `std::fs` or spawn processes directly; they go through a
//! [`System`]. Production uses [`RealSystem`]; tests use
//! [`crate::testing::FakeSystem`], which is why the task library's unit tests
//! are hermetic and fast.

mod guard;
mod real;

pub use guard::PlanGuard;
pub use real::{HttpClient, RealSystem};

use crate::Platform;
use async_trait::async_trait;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Ownership and mode for a file or directory. Fields left `None` are not changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FilePerms {
    /// Octal mode such as `0644`, parsed from the string form in YAML.
    #[serde(default, with = "mode_serde")]
    #[schemars(with = "Option<String>")]
    pub mode: Option<u32>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

impl FilePerms {
    pub fn mode(mode: u32) -> Self {
        Self {
            mode: Some(mode),
            owner: None,
            group: None,
        }
    }
}

mod mode_serde {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(mode: &Option<u32>, s: S) -> Result<S::Ok, S::Error> {
        match mode {
            Some(m) => s.serialize_str(&format!("{m:04o}")),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(u32),
            Text(String),
        }
        match Option::<Raw>::deserialize(d)? {
            None => Ok(None),
            Some(Raw::Text(t)) => u32::from_str_radix(t.trim_start_matches("0o"), 8)
                .map(Some)
                .map_err(|_| {
                    serde::de::Error::custom(format!(
                        "'{t}' is not an octal file mode such as \"0644\""
                    ))
                }),
            // A bare YAML integer like 0644 is decimal 644; treat its digits as octal, which is what the author meant.
            Some(Raw::Num(n)) => u32::from_str_radix(&n.to_string(), 8)
                .map(Some)
                .map_err(|_| {
                    serde::de::Error::custom(format!(
                        "{n} is not an octal file mode; quote it, e.g. \"0644\""
                    ))
                }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Metadata about a path on the host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileMeta {
    pub kind: FileKind,
    pub size: u64,
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub owner: Option<String>,
    pub group: Option<String>,
    pub modified: Option<chrono::DateTime<chrono::Utc>>,
}

/// A process to run. Its `Debug` omits environment values because they may
/// hold secrets injected by [`crate::secrets::sinks::ProcessEnvSink`].
#[derive(Clone, Default)]
pub struct Command {
    pub program: String,
    pub args: Vec<String>,
    pub env: IndexMap<String, String>,
    /// Clear the inherited environment before applying `env`.
    pub clear_env: bool,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    /// Set by the process-env sink; tells callers not to capture stdout/stderr into telemetry.
    pub sensitive_env: bool,
}

impl Command {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            ..Default::default()
        }
    }

    /// Builds a command from an argv-style list; the first element is the program.
    pub fn from_argv(argv: &[String]) -> Option<Self> {
        let (program, args) = argv.split_first()?;
        Some(Self {
            program: program.clone(),
            args: args.to_vec(),
            ..Default::default()
        })
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Adds a non-secret environment variable. Secrets go through the sink.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    /// `program arg1 arg2`, for logs and plans.
    pub fn display_line(&self) -> String {
        let mut parts = vec![self.program.clone()];
        parts.extend(self.args.iter().map(|a| {
            if a.contains(' ') {
                format!("{a:?}")
            } else {
                a.clone()
            }
        }));
        parts.join(" ")
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A finished process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// Exit code, or `None` when terminated by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
    pub gecos: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupInfo {
    pub name: String,
    pub gid: u32,
    pub members: Vec<String>,
}

/// An HTTP request issued by a task or health check. Its `Debug` shows
/// header names but never their values (they carry tokens).
#[derive(Clone, Default)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
    /// Accept any TLS certificate (for local self-signed endpoints in health checks).
    pub insecure: bool,
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &self.url)
            .field(
                "header_names",
                &self
                    .headers
                    .iter()
                    .map(|(k, _)| k.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("body_bytes", &self.body.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .field("insecure", &self.insecure)
            .finish()
    }
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: "GET".into(),
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The error `System::remove` returns for a non-empty directory without
/// `recursive`, identical for every implementation.
pub fn not_empty(path: &Path) -> crate::Error {
    crate::error::user(
        format!("The directory {} is not empty.", path.display()),
        &["Remove its contents first, or remove it recursively."],
    )
}

/// The temp-file name [`RealSystem`]'s `write_atomic` uses next to `path`'s
/// file name: `.<name>.buckle-tmp-` followed by the writer's pid.
pub fn temp_prefix_for(file_name: &str) -> String {
    format!(".{file_name}.buckle-tmp-")
}

/// Removes files in `dir` named `<prefix><pid>` left by a process that is no
/// longer running (a crash between writing a temp file and renaming it).
/// Best effort: errors are ignored.
pub fn remove_temp_leftovers(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(pid) = name
            .strip_prefix(prefix)
            .and_then(|p| p.parse::<u32>().ok())
        else {
            continue;
        };
        if pid != me && !process_alive(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        match i32::try_from(pid) {
            Ok(raw) => !matches!(
                nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None),
                Err(nix::errno::Errno::ESRCH)
            ),
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        // Without a cheap liveness probe, only our own pid counts as alive.
        pid == std::process::id()
    }
}

/// The host, as tasks see it. Every method is fallible and async; paths are
/// absolute on the host.
#[async_trait]
pub trait System: Send + Sync {
    fn platform(&self) -> Platform;

    fn hostname(&self) -> String;

    /// The value of an environment variable of the agent process.
    fn env_var(&self, name: &str) -> Option<String>;

    /// Reads a whole file. `Ok(None)` when it does not exist.
    async fn read(&self, path: &Path) -> crate::Result<Option<Vec<u8>>>;

    /// Metadata for a path without following a final symlink. `Ok(None)` when it does not exist.
    async fn stat(&self, path: &Path) -> crate::Result<Option<FileMeta>>;

    /// Writes a file atomically (temp file in the same directory, then rename),
    /// creating parent directories, and applies `perms`.
    async fn write_atomic(&self, path: &Path, bytes: &[u8], perms: &FilePerms)
    -> crate::Result<()>;

    /// Applies ownership and mode without touching content.
    async fn set_perms(&self, path: &Path, perms: &FilePerms) -> crate::Result<()>;

    /// Creates a directory and any missing parents; existing directories are left alone.
    async fn mkdir_all(&self, path: &Path, perms: &FilePerms) -> crate::Result<()>;

    /// Removes a file, symlink or (with `recursive`) directory tree. `Ok(false)` when it did not exist.
    async fn remove(&self, path: &Path, recursive: bool) -> crate::Result<bool>;

    async fn symlink(&self, target: &Path, link: &Path) -> crate::Result<()>;

    async fn read_link(&self, path: &Path) -> crate::Result<Option<PathBuf>>;

    /// Entries of a directory (full paths), sorted. `Ok(None)` when it does not exist.
    async fn list_dir(&self, path: &Path) -> crate::Result<Option<Vec<PathBuf>>>;

    /// Renames a path, replacing the destination.
    async fn rename(&self, from: &Path, to: &Path) -> crate::Result<()>;

    /// Runs a process to completion, capturing output.
    async fn run(&self, command: Command) -> crate::Result<Output>;

    /// Whether `program` is on the PATH.
    async fn which(&self, program: &str) -> crate::Result<Option<PathBuf>>;

    async fn lookup_user(&self, name: &str) -> crate::Result<Option<UserInfo>>;

    async fn lookup_group(&self, name: &str) -> crate::Result<Option<GroupInfo>>;

    async fn list_users(&self) -> crate::Result<Vec<UserInfo>>;

    async fn http(&self, request: HttpRequest) -> crate::Result<HttpResponse>;

    /// Attempts a TCP connection; `Ok(true)` if it was accepted within `timeout`.
    async fn tcp_connect(&self, address: &str, timeout: Duration) -> crate::Result<bool>;

    /// Wall-clock now, so tests can control time.
    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    /// A directory for scratch files (validator inputs, downloads being
    /// checked). Planning may write here; nowhere else.
    fn temp_dir(&self) -> PathBuf {
        std::env::temp_dir()
    }

    /// Where Buckle keeps its own files on this host: ledgers, stashes,
    /// downloads and the `tmp` scratch directory planning may write to. The
    /// agent and the local commands set it from their `state_dir`; the
    /// default reads `BUCKLE_STATE_DIR` and falls back to `/var/lib/buckle`.
    fn state_dir(&self) -> PathBuf {
        self.env_var("BUCKLE_STATE_DIR")
            .filter(|v| !v.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/lib/buckle"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("\"0644\"", Some(0o644))]
    #[case("\"0o755\"", Some(0o755))]
    #[case("644", Some(0o644))]
    #[case("null", None)]
    fn file_mode_parses_octal_strings_and_bare_digits(
        #[case] yaml: &str,
        #[case] expected: Option<u32>,
    ) {
        let perms: FilePerms = serde_yaml::from_str(&format!("mode: {yaml}")).unwrap();
        assert_eq!(perms.mode, expected);
    }

    #[test]
    fn file_mode_rejects_non_octal() {
        assert!(serde_yaml::from_str::<FilePerms>("mode: \"0789\"").is_err());
    }

    #[test]
    fn file_mode_serialises_as_zero_padded_octal_string() {
        let yaml = serde_yaml::to_string(&FilePerms::mode(0o640)).unwrap();
        assert!(
            yaml.contains("'0640'") || yaml.contains("\"0640\""),
            "{yaml}"
        );
    }

    #[test]
    fn http_request_debug_shows_header_names_but_never_values() {
        let req =
            HttpRequest::get("https://api.example/x").header("Authorization", "Bearer s3cret");
        let dbg = format!("{req:?}");
        assert!(dbg.contains("Authorization"), "{dbg}");
        assert!(!dbg.contains("s3cret"), "{dbg}");
    }

    #[test]
    fn command_debug_shows_env_keys_but_never_values() {
        let cmd = Command::new("sh")
            .arg("-c")
            .arg("echo hi")
            .env("TOKEN", "s3cret");
        let dbg = format!("{cmd:?}");
        assert!(dbg.contains("TOKEN"));
        assert!(!dbg.contains("s3cret"));
        assert_eq!(cmd.display_line(), "sh -c \"echo hi\"");
    }
}
