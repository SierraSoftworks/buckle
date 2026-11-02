//! Test helpers shared by the task kinds' unit tests.

use buckle_core::Digest;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, Planned, ResourceKey, RevisionId};
use buckle_core::secrets::{InstallKey, Secret, SecretStore};
use buckle_core::state::ResourceRecord;
use buckle_core::task::{Applied, Cx, Mode, Task};
use buckle_core::testing::{CxBuilder, FakeSystem, MemorySource};
use serde::de::DeserializeOwned;

/// A context in apply mode over `sys` with an empty module source.
pub fn cx(sys: &FakeSystem) -> Cx {
    CxBuilder::new(sys.clone()).build()
}

/// A context with module files.
pub fn cx_src(sys: &FakeSystem, source: MemorySource) -> Cx {
    CxBuilder::new(sys.clone()).source(source).build()
}

/// A context with module files and secrets.
pub fn cx_secrets(sys: &FakeSystem, source: MemorySource, secrets: SecretStore) -> Cx {
    CxBuilder::new(sys.clone())
        .source(source)
        .secrets(secrets)
        .build()
}

/// A plan-mode context.
pub fn cx_plan(sys: &FakeSystem, source: MemorySource) -> Cx {
    CxBuilder::new(sys.clone())
        .source(source)
        .mode(Mode::Plan)
        .build()
}

/// A secret store with one secret.
pub fn secrets(name: &str, value: &str) -> SecretStore {
    let mut store = SecretStore::new();
    store.insert(Secret::new(
        name.to_owned(),
        value.to_owned(),
        &InstallKey::from_bytes([5u8; 32]),
    ));
    store
}

/// Parses a spec from JSON, as the engine would after templating.
pub fn spec<S: DeserializeOwned>(json: serde_json::Value) -> S {
    serde_json::from_value(json).expect("the test spec parses")
}

/// The coarse outcome of a plan, for table tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    NoOp,
    Change,
    Blocked,
}

pub fn class(planned: &Planned) -> Class {
    match planned {
        Planned::NoOp => Class::NoOp,
        Planned::Change { .. } => Class::Change,
        Planned::Blocked { .. } => Class::Blocked,
    }
}

pub fn diff(planned: &Planned) -> &Diff {
    match planned {
        Planned::Change { diff } => diff,
        other => panic!("expected a change, got {other:?}"),
    }
}

/// Plans, asserts the outcome class, applies when it is a change and returns the result.
pub async fn plan_class<T: Task>(task: &T, cx: &Cx, spec: &T::Spec) -> Class {
    class(&task.plan(cx, spec).await.expect("plan succeeds"))
}

/// Plans (expecting a change), applies and returns what the apply did.
pub async fn apply<T: Task>(task: &T, cx: &Cx, spec: &T::Spec) -> Applied {
    let planned = task.plan(cx, spec).await.expect("plan succeeds");
    assert!(planned.is_change(), "expected a change, got {planned:?}");
    task.apply(cx, spec, &planned)
        .await
        .expect("apply succeeds")
}

/// Builds the state record the engine would store for an apply result.
pub fn record(kind: &str, key: &str, applied: &Applied) -> ResourceRecord {
    ResourceRecord {
        key: ResourceKey::new("test", kind, key),
        spec_digest: Digest::of(b"spec"),
        shadow: applied.shadow.clone(),
        content_digest: applied.content_digest.clone(),
        artefacts: applied.artefacts.clone(),
        removal: RemovalPolicy::Cleanup,
        revision: RevisionId::new("r1"),
        applied_at: chrono::Utc::now(),
        introduced_in: RevisionId::new("r1"),
    }
}

/// Everything a plan or apply produced, serialised, for leak assertions.
pub fn serialised(planned: &Planned, applied: &Applied) -> String {
    format!(
        "{}\n{}\n{planned:?}\n{applied:?}",
        serde_json::to_string(planned).unwrap(),
        serde_json::to_string(applied).unwrap()
    )
}

/// Writes a file into the fake host with default permissions.
pub async fn put(sys: &FakeSystem, path: &str, content: &str) {
    use buckle_core::system::{FilePerms, System};
    sys.write_atomic(
        std::path::Path::new(path),
        content.as_bytes(),
        &FilePerms::default(),
    )
    .await
    .expect("fake write");
}

/// Removes a path from the fake host.
pub trait FakeExt {
    fn remove_for_test(&self, path: &str) -> impl std::future::Future<Output = ()>;
}

impl FakeExt for FakeSystem {
    async fn remove_for_test(&self, path: &str) {
        use buckle_core::system::System;
        self.remove(std::path::Path::new(path), true)
            .await
            .expect("fake remove");
    }
}

/// Builds a `.tar.gz` in memory from `(path, content, mode)`; a path ending in `/` is a directory.
pub fn tar_gz(entries: &[(&str, &str, u32)]) -> Vec<u8> {
    use std::io::Write;
    let mut builder = tar::Builder::new(Vec::new());
    for (path, content, mode) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_mode(*mode);
        if path.ends_with('/') {
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_data(&mut header, path, std::io::empty())
                .unwrap();
        } else {
            header.set_size(content.len() as u64);
            header.set_cksum();
            builder
                .append_data(&mut header, path, content.as_bytes())
                .unwrap();
        }
    }
    let tar = builder.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

/// Builds a `.zip` in memory from `(path, content)`, every file mode 0755.
pub fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write;
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, content) in entries {
        writer
            .start_file(
                *path,
                zip::write::SimpleFileOptions::default().unix_permissions(0o755),
            )
            .unwrap();
        writer.write_all(content.as_bytes()).unwrap();
    }
    writer.finish().unwrap().into_inner()
}

/// The value following `flag` in a command's arguments.
pub fn flag_value(cmd: &buckle_core::system::Command, flag: &str) -> Option<String> {
    cmd.args
        .iter()
        .position(|a| a == flag)
        .and_then(|i| cmd.args.get(i + 1).cloned())
}

/// Scripts `useradd`, `usermod`, `groupadd`, `groupmod`, `chpasswd` and `sh` so
/// that they update the fake host's user and group databases. (`userdel` and
/// `groupdel` are scripted to succeed; the fake cannot forget entries, so tests
/// assert the commands that ran.)
pub fn with_account_tools(sys: FakeSystem) -> FakeSystem {
    use buckle_core::system::{GroupInfo, UserInfo};
    use buckle_core::testing::ok_output;
    let host = sys.clone();
    let on_useradd = {
        let host = host.clone();
        move |cmd: &buckle_core::system::Command| {
            let name = cmd.args.last().cloned().unwrap_or_default();
            let gid = flag_value(cmd, "-g")
                .and_then(|g| g.parse().ok())
                .unwrap_or(1000);
            let _ = host.clone().with_user(UserInfo {
                uid: flag_value(cmd, "-u")
                    .and_then(|u| u.parse().ok())
                    .unwrap_or(1000),
                gid,
                home: flag_value(cmd, "-d")
                    .map(Into::into)
                    .unwrap_or_else(|| format!("/home/{name}").into()),
                shell: flag_value(cmd, "-s")
                    .unwrap_or_else(|| "/bin/sh".into())
                    .into(),
                gecos: flag_value(cmd, "-c").unwrap_or_default(),
                name,
            });
            ok_output("")
        }
    };
    let on_usermod = {
        let host = host.clone();
        move |cmd: &buckle_core::system::Command| {
            let name = cmd.args.last().cloned().unwrap_or_default();
            if let Some(mut user) = futures_lite_user(&host, &name) {
                if let Some(uid) = flag_value(cmd, "-u").and_then(|u| u.parse().ok()) {
                    user.uid = uid;
                }
                if let Some(shell) = flag_value(cmd, "-s") {
                    user.shell = shell.into();
                }
                if let Some(home) = flag_value(cmd, "-d") {
                    user.home = home.into();
                }
                if let Some(comment) = flag_value(cmd, "-c") {
                    user.gecos = comment;
                }
                let _ = host.clone().with_user(user);
            }
            if let Some(groups) = flag_value(cmd, "-G") {
                for g in groups.split(',') {
                    let mut group = futures_lite_group(&host, g).unwrap_or(GroupInfo {
                        name: g.into(),
                        gid: 2000,
                        members: vec![],
                    });
                    if !group.members.contains(&name) {
                        group.members.push(name.clone());
                    }
                    let _ = host.clone().with_group(group);
                }
            }
            ok_output("")
        }
    };
    let on_groupadd = {
        let host = host.clone();
        move |cmd: &buckle_core::system::Command| {
            let name = cmd.args.last().cloned().unwrap_or_default();
            let _ = host.clone().with_group(GroupInfo {
                gid: flag_value(cmd, "-g")
                    .and_then(|g| g.parse().ok())
                    .unwrap_or(2000),
                members: vec![],
                name,
            });
            ok_output("")
        }
    };
    let on_groupmod = {
        let host = host.clone();
        move |cmd: &buckle_core::system::Command| {
            let name = cmd.args.last().cloned().unwrap_or_default();
            if let (Some(mut group), Some(gid)) = (
                futures_lite_group(&host, &name),
                flag_value(cmd, "-g").and_then(|g| g.parse().ok()),
            ) {
                group.gid = gid;
                let _ = host.clone().with_group(group);
            }
            ok_output("")
        }
    };
    sys.on_command("useradd", &[], on_useradd)
        .on_command("usermod", &[], on_usermod)
        .on_command("groupadd", &[], on_groupadd)
        .on_command("groupmod", &[], on_groupmod)
        .on_command_ok("userdel", &[], "")
        .on_command_ok("groupdel", &[], "")
        .on_command_ok("chpasswd", &[], "")
}

fn futures_lite_user(sys: &FakeSystem, name: &str) -> Option<buckle_core::system::UserInfo> {
    // The fake's lookups are synchronous under the hood; poll them to completion.
    use buckle_core::system::System;
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut fut = std::pin::pin!(sys.lookup_user(name));
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Ok(u)) => u,
        _ => None,
    }
}

fn futures_lite_group(sys: &FakeSystem, name: &str) -> Option<buckle_core::system::GroupInfo> {
    use buckle_core::system::System;
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut fut = std::pin::pin!(sys.lookup_group(name));
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Ok(g)) => g,
        _ => None,
    }
}
