//! Test support: an in-memory [`System`] and helpers to build task contexts.
//! Enabled with the `testing` feature (and always in this crate's own tests).

use crate::secrets::SecretStore;
use crate::selectors::{HostContext, HostIdentity};
use crate::system::*;
use crate::task::{Cx, Mode, Source};
use crate::{Platform, Result};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    File { bytes: Vec<u8>, perms: FilePerms },
    Dir { perms: FilePerms },
    Symlink { target: PathBuf },
}

/// A scripted response to a command.
#[derive(Clone)]
pub struct CommandScript {
    /// Matches when the program equals this and the args start with `args_prefix`.
    pub program: String,
    pub args_prefix: Vec<String>,
    pub response: Arc<dyn Fn(&Command) -> Output + Send + Sync>,
}

type HttpRoute = (
    String,
    Arc<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>,
);

#[derive(Default)]
struct Inner {
    fs: BTreeMap<PathBuf, Node>,
    commands: Vec<CommandScript>,
    ran: Vec<Command>,
    users: BTreeMap<String, UserInfo>,
    groups: BTreeMap<String, GroupInfo>,
    env: BTreeMap<String, String>,
    http: Vec<HttpRoute>,
    http_requests: Vec<HttpRequest>,
    tcp_open: Vec<String>,
    programs: Vec<String>,
    platform: Platform,
    hostname: String,
    now: chrono::DateTime<chrono::Utc>,
    /// Explicit modification times; paths absent here report `now`.
    mtimes: BTreeMap<PathBuf, chrono::DateTime<chrono::Utc>>,
}

/// An in-memory host. Clone it freely; clones share the same state.
#[derive(Clone)]
pub struct FakeSystem {
    inner: Arc<Mutex<Inner>>,
}

impl Default for FakeSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSystem {
    pub fn new() -> Self {
        let mut inner = Inner {
            platform: Platform::Linux,
            hostname: "test-host".into(),
            now: chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            ..Default::default()
        };
        inner.fs.insert(
            PathBuf::from("/"),
            Node::Dir {
                perms: FilePerms::mode(0o755),
            },
        );
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    pub fn with_platform(self, platform: Platform) -> Self {
        self.inner.lock().unwrap().platform = platform;
        self
    }

    pub fn with_hostname(self, hostname: &str) -> Self {
        self.inner.lock().unwrap().hostname = hostname.into();
        self
    }

    pub fn with_now(self, now: chrono::DateTime<chrono::Utc>) -> Self {
        self.inner.lock().unwrap().now = now;
        self
    }

    /// Adds a file with content and perms (parents are created).
    pub fn with_file(
        self,
        path: impl AsRef<Path>,
        bytes: impl AsRef<[u8]>,
        perms: FilePerms,
    ) -> Self {
        self.put_file(path.as_ref(), bytes.as_ref().to_vec(), perms);
        self
    }

    pub fn with_dir(self, path: impl AsRef<Path>, perms: FilePerms) -> Self {
        self.ensure_dir(path.as_ref(), perms);
        self
    }

    /// Sets the modification time `stat` reports for an existing path. Paths
    /// without an explicit time report the fake clock's `now`; writing a file
    /// through the system clears its explicit time.
    pub fn with_file_mtime(
        self,
        path: impl AsRef<Path>,
        modified: chrono::DateTime<chrono::Utc>,
    ) -> Self {
        self.inner
            .lock()
            .unwrap()
            .mtimes
            .insert(path.as_ref().to_path_buf(), modified);
        self
    }

    pub fn with_user(self, user: UserInfo) -> Self {
        self.inner
            .lock()
            .unwrap()
            .users
            .insert(user.name.clone(), user);
        self
    }

    pub fn with_group(self, group: GroupInfo) -> Self {
        self.inner
            .lock()
            .unwrap()
            .groups
            .insert(group.name.clone(), group);
        self
    }

    pub fn with_env(self, key: &str, value: &str) -> Self {
        self.inner
            .lock()
            .unwrap()
            .env
            .insert(key.into(), value.into());
        self
    }

    /// Marks a program as present on the PATH (for `which`).
    pub fn with_program(self, program: &str) -> Self {
        self.inner.lock().unwrap().programs.push(program.into());
        self
    }

    pub fn with_tcp_open(self, address: &str) -> Self {
        self.inner.lock().unwrap().tcp_open.push(address.into());
        self
    }

    /// Scripts a command: `program` with args starting with `args_prefix` returns the output.
    pub fn on_command(
        self,
        program: &str,
        args_prefix: &[&str],
        response: impl Fn(&Command) -> Output + Send + Sync + 'static,
    ) -> Self {
        self.inner.lock().unwrap().commands.push(CommandScript {
            program: program.into(),
            args_prefix: args_prefix.iter().map(|s| s.to_string()).collect(),
            response: Arc::new(response),
        });
        self.inner.lock().unwrap().programs.push(program.into());
        self
    }

    /// Scripts a command that succeeds with the given stdout.
    pub fn on_command_ok(self, program: &str, args_prefix: &[&str], stdout: &str) -> Self {
        let stdout = stdout.to_owned();
        self.on_command(program, args_prefix, move |_| ok_output(&stdout))
    }

    /// Scripts a command that fails with the given exit code and stderr.
    pub fn on_command_fail(
        self,
        program: &str,
        args_prefix: &[&str],
        code: i32,
        stderr: &str,
    ) -> Self {
        let stderr = stderr.to_owned();
        self.on_command(program, args_prefix, move |_| Output {
            code: Some(code),
            stdout: vec![],
            stderr: stderr.clone().into_bytes(),
            timed_out: false,
        })
    }

    /// Scripts an HTTP route: requests whose URL starts with `url_prefix` get the response.
    pub fn on_http(
        self,
        url_prefix: &str,
        response: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static,
    ) -> Self {
        self.inner
            .lock()
            .unwrap()
            .http
            .push((url_prefix.into(), Arc::new(response)));
        self
    }

    pub fn on_http_ok(self, url_prefix: &str, body: &str) -> Self {
        let body = body.to_owned();
        self.on_http(url_prefix, move |_| HttpResponse {
            status: 200,
            headers: vec![],
            body: body.clone().into_bytes(),
        })
    }

    // --- inspection ---

    pub fn file(&self, path: impl AsRef<Path>) -> Option<Vec<u8>> {
        match self.inner.lock().unwrap().fs.get(path.as_ref()) {
            Some(Node::File { bytes, .. }) => Some(bytes.clone()),
            _ => None,
        }
    }

    pub fn file_string(&self, path: impl AsRef<Path>) -> Option<String> {
        self.file(path)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    pub fn node(&self, path: impl AsRef<Path>) -> Option<Node> {
        self.inner.lock().unwrap().fs.get(path.as_ref()).cloned()
    }

    pub fn exists(&self, path: impl AsRef<Path>) -> bool {
        self.inner.lock().unwrap().fs.contains_key(path.as_ref())
    }

    /// Commands run so far, in order.
    pub fn commands_run(&self) -> Vec<Command> {
        self.inner.lock().unwrap().ran.clone()
    }

    /// Display lines of the commands run, for assertions.
    pub fn command_lines(&self) -> Vec<String> {
        self.commands_run()
            .iter()
            .map(Command::display_line)
            .collect()
    }

    pub fn http_requests(&self) -> Vec<HttpRequest> {
        self.inner.lock().unwrap().http_requests.clone()
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.inner.lock().unwrap().fs.keys().cloned().collect()
    }

    fn put_file(&self, path: &Path, bytes: Vec<u8>, perms: FilePerms) {
        if let Some(parent) = path.parent() {
            self.ensure_dir(parent, FilePerms::mode(0o755));
        }
        let mut inner = self.inner.lock().unwrap();
        inner
            .fs
            .insert(path.to_path_buf(), Node::File { bytes, perms });
        inner.mtimes.remove(path);
    }

    fn ensure_dir(&self, path: &Path, perms: FilePerms) {
        let mut inner = self.inner.lock().unwrap();
        let mut current = PathBuf::new();
        for component in path.components() {
            current.push(component);
            inner
                .fs
                .entry(current.clone())
                .or_insert_with(|| Node::Dir {
                    perms: perms.clone(),
                });
        }
    }
}

pub fn ok_output(stdout: &str) -> Output {
    Output {
        code: Some(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: vec![],
        timed_out: false,
    }
}

fn meta_for(path: &Path, node: &Node, inner: &Inner) -> FileMeta {
    let (kind, size, perms) = match node {
        Node::File { bytes, perms } => (FileKind::File, bytes.len() as u64, Some(perms)),
        Node::Dir { perms } => (FileKind::Directory, 0, Some(perms)),
        Node::Symlink { .. } => (FileKind::Symlink, 0, None),
    };
    let owner = perms
        .and_then(|p| p.owner.clone())
        .or_else(|| Some("root".into()));
    let group = perms
        .and_then(|p| p.group.clone())
        .or_else(|| Some("root".into()));
    FileMeta {
        kind,
        size,
        mode: perms
            .and_then(|p| p.mode)
            .or(Some(if kind == FileKind::Directory {
                0o755
            } else {
                0o644
            })),
        uid: owner
            .as_ref()
            .and_then(|o| inner.users.get(o).map(|u| u.uid))
            .or(Some(0)),
        gid: group
            .as_ref()
            .and_then(|g| inner.groups.get(g).map(|g| g.gid))
            .or(Some(0)),
        owner,
        group,
        modified: Some(inner.mtimes.get(path).copied().unwrap_or(inner.now)),
    }
}

#[async_trait]
impl System for FakeSystem {
    fn platform(&self) -> Platform {
        self.inner.lock().unwrap().platform
    }

    fn hostname(&self) -> String {
        self.inner.lock().unwrap().hostname.clone()
    }

    fn env_var(&self, name: &str) -> Option<String> {
        self.inner.lock().unwrap().env.get(name).cloned()
    }

    async fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        // Like the real filesystem, reading follows symlinks.
        let mut current = path.to_path_buf();
        for _ in 0..40 {
            match self.node(&current) {
                Some(Node::File { bytes, .. }) => return Ok(Some(bytes)),
                Some(Node::Symlink { target }) => {
                    current = match current.parent() {
                        Some(parent) if target.is_relative() => parent.join(target),
                        _ => target,
                    };
                }
                Some(Node::Dir { .. }) | None => return Ok(None),
            }
        }
        Err(crate::error::user(
            format!("{} is a symlink loop.", path.display()),
            &["Fix the symlinks."],
        ))
    }

    async fn stat(&self, path: &Path) -> Result<Option<FileMeta>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner.fs.get(path).map(|n| meta_for(path, n, &inner)))
    }

    async fn write_atomic(&self, path: &Path, bytes: &[u8], perms: &FilePerms) -> Result<()> {
        // Like the real write (a new file renamed into place), the result has
        // exactly `perms`; nothing carries over from the file it replaces.
        self.put_file(path, bytes.to_vec(), perms.clone());
        Ok(())
    }

    async fn set_perms(&self, path: &Path, perms: &FilePerms) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        match inner.fs.get_mut(path) {
            Some(Node::File { perms: p, .. }) | Some(Node::Dir { perms: p }) => {
                if perms.mode.is_some() {
                    p.mode = perms.mode;
                }
                if perms.owner.is_some() {
                    p.owner = perms.owner.clone();
                }
                if perms.group.is_some() {
                    p.group = perms.group.clone();
                }
                Ok(())
            }
            Some(Node::Symlink { .. }) => Ok(()),
            None => Err(crate::error::user(
                format!("{} does not exist", path.display()),
                &["Create it first."],
            )),
        }
    }

    async fn mkdir_all(&self, path: &Path, perms: &FilePerms) -> Result<()> {
        self.ensure_dir(path, perms.clone());
        let mut inner = self.inner.lock().unwrap();
        if let Some(Node::Dir { perms: p }) = inner.fs.get_mut(path) {
            if perms.mode.is_some() {
                p.mode = perms.mode;
            }
            if perms.owner.is_some() {
                p.owner = perms.owner.clone();
            }
            if perms.group.is_some() {
                p.group = perms.group.clone();
            }
        }
        Ok(())
    }

    async fn remove(&self, path: &Path, recursive: bool) -> Result<bool> {
        let mut inner = self.inner.lock().unwrap();
        let Some(node) = inner.fs.get(path).cloned() else {
            return Ok(false);
        };
        if let Node::Dir { .. } = node {
            let children: Vec<PathBuf> = inner
                .fs
                .keys()
                .filter(|k| k.starts_with(path) && *k != path)
                .cloned()
                .collect();
            if !children.is_empty() && !recursive {
                return Err(crate::system::not_empty(path));
            }
            for c in children {
                inner.fs.remove(&c);
                inner.mtimes.remove(&c);
            }
        }
        inner.fs.remove(path);
        inner.mtimes.remove(path);
        Ok(true)
    }

    async fn symlink(&self, target: &Path, link: &Path) -> Result<()> {
        if let Some(parent) = link.parent() {
            self.ensure_dir(parent, FilePerms::mode(0o755));
        }
        self.inner.lock().unwrap().fs.insert(
            link.to_path_buf(),
            Node::Symlink {
                target: target.to_path_buf(),
            },
        );
        Ok(())
    }

    async fn read_link(&self, path: &Path) -> Result<Option<PathBuf>> {
        Ok(match self.node(path) {
            Some(Node::Symlink { target }) => Some(target),
            _ => None,
        })
    }

    async fn list_dir(&self, path: &Path) -> Result<Option<Vec<PathBuf>>> {
        let inner = self.inner.lock().unwrap();
        if !matches!(inner.fs.get(path), Some(Node::Dir { .. })) {
            return Ok(None);
        }
        Ok(Some(
            inner
                .fs
                .keys()
                .filter(|k| k.parent() == Some(path))
                .cloned()
                .collect(),
        ))
    }

    async fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let node = inner.fs.remove(from).ok_or_else(|| {
            crate::error::user(
                format!("{} does not exist", from.display()),
                &["Create it first."],
            )
        })?;
        inner.fs.insert(to.to_path_buf(), node);
        if let Some(mtime) = inner.mtimes.remove(from) {
            inner.mtimes.insert(to.to_path_buf(), mtime);
        }
        Ok(())
    }

    async fn run(&self, command: Command) -> Result<Output> {
        let script = {
            let mut inner = self.inner.lock().unwrap();
            inner.ran.push(command.clone());
            inner
                .commands
                .iter()
                .rev()
                .find(|s| s.program == command.program && command.args.starts_with(&s.args_prefix))
                .cloned()
        };
        match script {
            Some(s) => Ok((s.response)(&command)),
            None => Err(crate::error::user(
                format!("FakeSystem has no script for `{}`.", command.display_line()),
                &["Add one with FakeSystem::on_command(..) in the test."],
            )),
        }
    }

    async fn which(&self, program: &str) -> Result<Option<PathBuf>> {
        let inner = self.inner.lock().unwrap();
        Ok(inner
            .programs
            .iter()
            .any(|p| p == program)
            .then(|| PathBuf::from("/usr/bin").join(program)))
    }

    async fn lookup_user(&self, name: &str) -> Result<Option<UserInfo>> {
        Ok(self.inner.lock().unwrap().users.get(name).cloned())
    }

    async fn lookup_group(&self, name: &str) -> Result<Option<GroupInfo>> {
        Ok(self.inner.lock().unwrap().groups.get(name).cloned())
    }

    async fn list_users(&self) -> Result<Vec<UserInfo>> {
        Ok(self.inner.lock().unwrap().users.values().cloned().collect())
    }

    async fn http(&self, request: HttpRequest) -> Result<HttpResponse> {
        let route = {
            let mut inner = self.inner.lock().unwrap();
            inner.http_requests.push(request.clone());
            inner
                .http
                .iter()
                .rev()
                .find(|(prefix, _)| request.url.starts_with(prefix.as_str()))
                .map(|(_, r)| r.clone())
        };
        match route {
            Some(r) => Ok(r(&request)),
            None => Err(crate::error::user(
                format!("FakeSystem has no HTTP route for {}.", request.url),
                &["Add one with FakeSystem::on_http(..) in the test."],
            )),
        }
    }

    async fn tcp_connect(&self, address: &str, _timeout: Duration) -> Result<bool> {
        Ok(self
            .inner
            .lock()
            .unwrap()
            .tcp_open
            .iter()
            .any(|a| a == address))
    }

    fn now(&self) -> chrono::DateTime<chrono::Utc> {
        self.inner.lock().unwrap().now
    }
}

/// An in-memory module source: relative path → bytes.
#[derive(Debug, Default, Clone)]
pub struct MemorySource {
    files: BTreeMap<PathBuf, Vec<u8>>,
}

impl MemorySource {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(mut self, path: impl AsRef<Path>, content: impl AsRef<[u8]>) -> Self {
        self.files
            .insert(path.as_ref().to_path_buf(), content.as_ref().to_vec());
        self
    }
}

#[async_trait]
impl Source for MemorySource {
    async fn read(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        Ok(self.files.get(path).cloned())
    }

    async fn list(&self, path: &Path) -> Result<Vec<PathBuf>> {
        Ok(self
            .files
            .keys()
            .filter_map(|k| k.strip_prefix(path).ok().map(Path::to_path_buf))
            .filter(|p| !p.as_os_str().is_empty())
            .collect())
    }
}

/// Builds a task context for tests.
pub struct CxBuilder {
    system: FakeSystem,
    source: MemorySource,
    host: HostContext,
    mode: Mode,
    module: String,
    previous: Option<Arc<crate::state::ResourceRecord>>,
}

impl CxBuilder {
    pub fn new(system: FakeSystem) -> Self {
        Self {
            system,
            source: MemorySource::new(),
            host: HostContext::new(HostIdentity {
                id: "test-host".into(),
                ..Default::default()
            }),
            mode: Mode::Apply,
            module: "test".into(),
            previous: None,
        }
    }

    /// The resource's existing record, as the engine would pass it.
    pub fn previous(mut self, record: crate::state::ResourceRecord) -> Self {
        self.previous = Some(Arc::new(record));
        self
    }

    pub fn source(mut self, source: MemorySource) -> Self {
        self.source = source;
        self
    }

    pub fn host(mut self, host: HostContext) -> Self {
        self.host = host;
        self
    }

    pub fn secrets(mut self, secrets: SecretStore) -> Self {
        self.host.secrets = Arc::new(secrets);
        self
    }

    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub fn module(mut self, module: &str) -> Self {
        self.module = module.into();
        self
    }

    pub fn build(self) -> Cx {
        let secrets = self.host.secrets.clone();
        Cx {
            mode: self.mode,
            system: Arc::new(self.system),
            host: Arc::new(self.host.in_module(&self.module, &[])),
            templater: Arc::new(crate::template::Templater::new()),
            source: Arc::new(self.source),
            module: self.module,
            secrets,
            waived_checks: Arc::new(Vec::new()),
            previous: self.previous,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_fs_behaves_like_a_filesystem() {
        let sys = FakeSystem::new().with_file("/etc/a.conf", "x", FilePerms::mode(0o644));
        assert_eq!(
            sys.read(Path::new("/etc/a.conf")).await.unwrap().unwrap(),
            b"x"
        );
        assert_eq!(
            sys.stat(Path::new("/etc")).await.unwrap().unwrap().kind,
            FileKind::Directory
        );
        sys.write_atomic(Path::new("/etc/b/c.conf"), b"y", &FilePerms::mode(0o600))
            .await
            .unwrap();
        assert_eq!(
            sys.stat(Path::new("/etc/b/c.conf"))
                .await
                .unwrap()
                .unwrap()
                .mode,
            Some(0o600)
        );
        assert_eq!(
            sys.list_dir(Path::new("/etc"))
                .await
                .unwrap()
                .unwrap()
                .len(),
            2
        );
        assert!(
            sys.remove(Path::new("/etc"), false).await.is_err(),
            "non-recursive removal of a populated dir fails"
        );
        assert!(sys.remove(Path::new("/etc"), true).await.unwrap());
        assert!(!sys.exists("/etc/a.conf"));
    }

    #[tokio::test]
    async fn write_atomic_sets_perms_rather_than_merging_them() {
        let sys = FakeSystem::new().with_file("/etc/a", "x", FilePerms::mode(0o600));
        sys.write_atomic(Path::new("/etc/a"), b"y", &FilePerms::default())
            .await
            .unwrap();
        assert_eq!(
            sys.stat(Path::new("/etc/a")).await.unwrap().unwrap().mode,
            Some(0o644),
            "like a new file renamed into place"
        );
    }

    #[tokio::test]
    async fn explicit_mtimes_are_reported_until_the_file_is_rewritten() {
        let old = chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let sys = FakeSystem::new()
            .with_file("/var/cache/stamp", "x", FilePerms::mode(0o644))
            .with_file_mtime("/var/cache/stamp", old);
        let meta = sys
            .stat(Path::new("/var/cache/stamp"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.modified, Some(old));
        sys.write_atomic(Path::new("/var/cache/stamp"), b"y", &FilePerms::default())
            .await
            .unwrap();
        let meta = sys
            .stat(Path::new("/var/cache/stamp"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta.modified, Some(sys.now()));
    }

    #[tokio::test]
    async fn scripted_commands_match_by_prefix_and_record_calls() {
        let sys = FakeSystem::new()
            .on_command_ok("systemctl", &["is-active"], "active\n")
            .on_command_fail("systemctl", &["is-enabled"], 1, "disabled");
        let out = sys
            .run(Command::new("systemctl").args(["is-active", "nomad"]))
            .await
            .unwrap();
        assert!(out.success());
        let out = sys
            .run(Command::new("systemctl").args(["is-enabled", "nomad"]))
            .await
            .unwrap();
        assert_eq!(out.code, Some(1));
        assert!(
            sys.run(Command::new("apt-get").arg("update"))
                .await
                .is_err(),
            "unscripted commands are loud failures"
        );
        assert_eq!(
            sys.command_lines(),
            vec![
                "systemctl is-active nomad",
                "systemctl is-enabled nomad",
                "apt-get update"
            ]
        );
        assert!(sys.which("systemctl").await.unwrap().is_some());
    }
}
