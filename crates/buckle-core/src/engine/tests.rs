//! Behavioural tests of the engine over `FakeSystem` and test-only kinds.
//!
//! The kinds below only exist in this crate's test binary. They record every
//! call into a global log keyed by a per-test `run` id, so tests observe
//! ordering, retries and parallelism without touching a real host.

use super::*;
use crate::Digest;
use crate::model::{ModuleConfig, RemovalPolicy};
use crate::plan::{Diff, Planned, ResourceKey, Step};
use crate::secrets::{InstallKey, Redacted, Secret, SecretStore};
use crate::selectors::{Labels, Vars};
use crate::state::{MemoryStateStore, ResourceRecord, State};
use crate::system::FilePerms;
use crate::task::{Applied, Artefact, Cx, Task};
use crate::testing::{FakeSystem, MemorySource};
use crate::{Filter, Platform, Platforms};
use async_trait::async_trait;
use pretty_assertions::assert_eq;
use rstest::rstest;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Shared instrumentation
// ---------------------------------------------------------------------------

static CALLS: LazyLock<Mutex<Vec<(String, String)>>> = LazyLock::new(Default::default);
static ATTEMPTS: LazyLock<Mutex<HashMap<String, u32>>> = LazyLock::new(Default::default);
static SIGNALS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

fn record(run: &str, event: impl Into<String>) {
    CALLS.lock().unwrap().push((run.to_owned(), event.into()));
}

fn calls(run: &str) -> Vec<String> {
    CALLS
        .lock()
        .unwrap()
        .iter()
        .filter(|(r, _)| r == run)
        .map(|(_, e)| e.clone())
        .collect()
}

fn bump(counter: &str) -> u32 {
    let mut map = ATTEMPTS.lock().unwrap();
    let n = map.entry(counter.to_owned()).or_default();
    *n += 1;
    *n
}

fn signal(name: &str) {
    SIGNALS.lock().unwrap().insert(name.to_owned());
}

async fn wait_for(name: &str) -> crate::Result<()> {
    for _ in 0..5000 {
        if SIGNALS.lock().unwrap().contains(name) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    Err(crate::error::system(
        format!("barrier '{name}' was never signalled"),
        &["The test expected another task to run concurrently."],
    ))
}

// ---------------------------------------------------------------------------
// Test task kinds
// ---------------------------------------------------------------------------

/// A file-like resource with knobs for failures, barriers and delays.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
struct FileSpec {
    path: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    run: String,
    /// Fail this many applies before succeeding (per run and path).
    #[serde(default)]
    fail_times: u32,
    #[serde(default)]
    signal: Option<String>,
    #[serde(default)]
    wait_for: Option<String>,
    #[serde(default)]
    sleep_ms: u64,
    /// A path that must exist before this task can be planned (a "tool").
    #[serde(default)]
    requires: Option<String>,
    /// Plan (and apply) deleting the path instead of writing it.
    #[serde(default)]
    delete: bool,
    /// Plan a change but find nothing to do at apply time.
    #[serde(default)]
    unchanged: bool,
}

async fn plan_file(cx: &Cx, spec: &FileSpec) -> crate::Result<Planned> {
    record(&spec.run, format!("plan {}/{}", cx.module, spec.path));
    if let Some(required) = &spec.requires
        && !cx.system.stat(Path::new(required)).await?.is_some()
    {
        return Ok(Planned::blocked(format!("{required} is not installed")));
    }
    let existing = cx.system.read(Path::new(&spec.path)).await?;
    if spec.delete {
        return Ok(match existing {
            Some(_) => Planned::change(Diff::Delete {
                summary: format!("delete {}", spec.path),
            }),
            None => Planned::NoOp,
        });
    }
    Ok(match existing {
        Some(bytes) if bytes == spec.content.as_bytes() => Planned::NoOp,
        Some(bytes) => Planned::change(Diff::unified(
            format!("update {}", spec.path),
            &Redacted::plain(String::from_utf8_lossy(&bytes).into_owned()),
            &Redacted::plain(spec.content.clone()),
        )),
        None => Planned::change(Diff::Create {
            summary: format!("create {}", spec.path),
            preview: Some(Redacted::plain(spec.content.clone())),
        }),
    })
}

async fn apply_file(cx: &Cx, spec: &FileSpec) -> crate::Result<Applied> {
    record(
        &spec.run,
        format!("apply:start {}/{}", cx.module, spec.path),
    );
    if let Some(s) = &spec.signal {
        signal(s);
    }
    if let Some(w) = &spec.wait_for {
        wait_for(w).await?;
    }
    if spec.sleep_ms > 0 {
        tokio::time::sleep(Duration::from_millis(spec.sleep_ms)).await;
    }
    let attempt = bump(&format!("{}:{}", spec.run, spec.path));
    if attempt <= spec.fail_times {
        record(&spec.run, format!("apply:fail {}/{}", cx.module, spec.path));
        return Err(crate::error::user(
            format!("transient failure #{attempt} writing {}", spec.path),
            &["Try again."],
        ));
    }
    if spec.unchanged {
        return Ok(Applied::unchanged(format!(
            "{} was already right",
            spec.path
        )));
    }
    if spec.delete {
        cx.system.remove(Path::new(&spec.path), false).await?;
        return Ok(Applied::changed(format!("deleted {}", spec.path)));
    }
    cx.system
        .write_atomic(
            Path::new(&spec.path),
            spec.content.as_bytes(),
            &FilePerms::mode(0o644),
        )
        .await?;
    record(&spec.run, format!("apply:end {}/{}", cx.module, spec.path));
    Ok(Applied::changed(format!("wrote {}", spec.path))
        .with_shadow(
            Redacted::plain(spec.content.clone()),
            Digest::of(spec.content.as_bytes()),
        )
        .with_artefact(Artefact::File {
            path: spec.path.clone().into(),
        }))
}

async fn remove_file(cx: &Cx, record: &ResourceRecord) -> crate::Result<Applied> {
    self::record("removals", format!("remove {}", record.key));
    cx.system.remove(Path::new(&record.key.key), false).await?;
    Ok(Applied::changed(format!("deleted {}", record.key.key)))
}

macro_rules! file_kind {
    ($name:ident, $kind:literal, locks = $locks:expr, platforms = $platforms:expr, caution = $caution:expr, always = $always:expr) => {
        file_kind!(
            $name,
            $kind,
            locks = $locks,
            platforms = $platforms,
            caution = $caution,
            always = $always,
            removal = RemovalPolicy::Cleanup
        );
    };
    ($name:ident, $kind:literal, locks = $locks:expr, platforms = $platforms:expr, caution = $caution:expr, always = $always:expr, removal = $removal:expr) => {
        struct $name;

        #[async_trait]
        impl Task for $name {
            type Spec = FileSpec;
            const KIND: &'static str = $kind;
            const LOCKS: &'static [&'static str] = $locks;
            const PLATFORMS: Platforms = $platforms;
            const REMOVAL_CAUTION: bool = $caution;
            const REMOVAL_DEFAULT: RemovalPolicy = $removal;

            fn key(spec: &FileSpec) -> String {
                spec.path.clone()
            }

            fn always_runs(_spec: &FileSpec) -> bool {
                $always
            }

            async fn plan(&self, cx: &Cx, spec: &FileSpec) -> crate::Result<Planned> {
                if $always {
                    record(&spec.run, format!("plan {}/{}", cx.module, spec.path));
                    return Ok(Planned::change(Diff::Opaque {
                        summary: format!("run {}", spec.path),
                    }));
                }
                plan_file(cx, spec).await
            }

            async fn apply(
                &self,
                cx: &Cx,
                spec: &FileSpec,
                _planned: &Planned,
            ) -> crate::Result<Applied> {
                apply_file(cx, spec).await
            }

            fn describe_removal(&self, record: &ResourceRecord) -> String {
                format!("delete {}", record.key.key)
            }

            async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> crate::Result<Applied> {
                remove_file(cx, record).await
            }
        }

        crate::register_task!($name);
    };
}

file_kind!(
    TestFile,
    "test.file",
    locks = &[],
    platforms = Platforms::ALL,
    caution = false,
    always = false
);
file_kind!(
    TestLocked,
    "test.locked",
    locks = &["test-lock"],
    platforms = Platforms::ALL,
    caution = false,
    always = false
);
file_kind!(
    TestCaution,
    "test.caution",
    locks = &[],
    platforms = Platforms::ALL,
    caution = true,
    always = false
);
file_kind!(
    TestLinux,
    "test.linux",
    locks = &[],
    platforms = Platforms::LINUX,
    caution = false,
    always = false
);
file_kind!(
    TestKept,
    "test.kept",
    locks = &[],
    platforms = Platforms::ALL,
    caution = false,
    always = false,
    removal = RemovalPolicy::Leave
);
file_kind!(
    TestAlways,
    "test.always",
    locks = &[],
    platforms = Platforms::ALL,
    caution = false,
    always = true
);

/// A `command.run` stand-in keyed (like the real one) by its command line,
/// with an optional `remove:` command.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
struct CmdSpec {
    run: Vec<String>,
    #[serde(default)]
    remove: Option<Vec<String>>,
    #[serde(default)]
    log: String,
}

struct TestCommand;

#[async_trait]
impl Task for TestCommand {
    type Spec = CmdSpec;
    const KIND: &'static str = "command.run";

    fn key(spec: &CmdSpec) -> String {
        spec.run.join(" ")
    }

    async fn plan(&self, _cx: &Cx, spec: &CmdSpec) -> crate::Result<Planned> {
        Ok(Planned::change(Diff::Opaque {
            summary: format!("run `{}`", spec.run.join(" ")),
        }))
    }

    async fn apply(&self, _cx: &Cx, spec: &CmdSpec, _planned: &Planned) -> crate::Result<Applied> {
        record(&spec.log, format!("run {}", spec.run.join(" ")));
        Ok(
            Applied::changed(format!("ran {}", spec.run.join(" "))).with_artefact(
                Artefact::Other {
                    description: format!(
                        "remove={}",
                        spec.remove.clone().unwrap_or_default().join(" ")
                    ),
                },
            ),
        )
    }

    async fn remove(&self, _cx: &Cx, record: &ResourceRecord) -> crate::Result<Applied> {
        self::record("removals", format!("undo {}", record.key));
        Ok(Applied::changed("undone"))
    }
}

crate::register_task!(TestCommand);

/// A `file.line` stand-in whose key (like buckle-std's) became the regex
/// when one is given, and that records the line it wrote.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
struct LineSpec {
    path: String,
    line: String,
    #[serde(default)]
    regex: Option<String>,
}

struct TestLine;

#[async_trait]
impl Task for TestLine {
    type Spec = LineSpec;
    const KIND: &'static str = "test.line";

    fn key(spec: &LineSpec) -> String {
        format!(
            "{}: {}",
            spec.path,
            spec.regex.as_deref().unwrap_or(&spec.line)
        )
    }

    async fn plan(&self, cx: &Cx, spec: &LineSpec) -> crate::Result<Planned> {
        let text = cx
            .system
            .read(Path::new(&spec.path))
            .await?
            .unwrap_or_default();
        Ok(
            if String::from_utf8_lossy(&text)
                .lines()
                .any(|l| l == spec.line)
            {
                Planned::NoOp
            } else {
                Planned::change(Diff::Opaque {
                    summary: format!("add `{}`", spec.line),
                })
            },
        )
    }

    async fn apply(&self, cx: &Cx, spec: &LineSpec, _planned: &Planned) -> crate::Result<Applied> {
        cx.system
            .write_atomic(
                Path::new(&spec.path),
                format!("{}\n", spec.line).as_bytes(),
                &FilePerms::mode(0o644),
            )
            .await?;
        Ok(Applied::changed("added").with_artefact(Artefact::Line {
            path: spec.path.clone().into(),
            marker: serde_json::json!({"Added": {"line": spec.line, "created": false}}).to_string(),
        }))
    }

    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> crate::Result<Applied> {
        self::record("removals", format!("revert {}", record.key));
        cx.system
            .write_atomic(
                Path::new("/lines/reverted"),
                b"yes",
                &FilePerms::mode(0o644),
            )
            .await?;
        Ok(Applied::changed("reverted"))
    }
}

crate::register_task!(TestLine);

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct HeaderSpec {
    token: crate::secrets::SecretField,
}

/// A check with a secret-capable field, like the http check's headers.
struct SecretHeaderCheck;

#[async_trait]
impl crate::health::HealthCheck for SecretHeaderCheck {
    type Spec = HeaderSpec;
    const KIND: &'static str = "test.secret_header";

    async fn check(&self, cx: &Cx, spec: &HeaderSpec) -> crate::health::HealthStatus {
        match spec.token.resolve(&cx.templater, &cx.host) {
            Ok(_) if spec.token.references_secrets() => crate::health::HealthStatus::Pass,
            Ok(_) => {
                crate::health::HealthStatus::fail("the field was rendered before the check saw it")
            }
            Err(e) => crate::health::HealthStatus::unknown(e.description()),
        }
    }
}

crate::register_health!(SecretHeaderCheck);

/// A buggy kind whose `plan` writes `path` (a validator scratch file when it
/// is under the temp dir, a real change otherwise).
struct TestImpure;

#[async_trait]
impl Task for TestImpure {
    type Spec = FileSpec;
    const KIND: &'static str = "test.impure";

    fn key(spec: &FileSpec) -> String {
        spec.path.clone()
    }

    async fn plan(&self, cx: &Cx, spec: &FileSpec) -> crate::Result<Planned> {
        cx.system
            .write_atomic(
                Path::new(&spec.path),
                b"written while planning",
                &FilePerms::default(),
            )
            .await?;
        Ok(Planned::NoOp)
    }

    async fn apply(
        &self,
        _cx: &Cx,
        _spec: &FileSpec,
        _planned: &Planned,
    ) -> crate::Result<Applied> {
        Ok(Applied::unchanged("nothing"))
    }
}

crate::register_task!(TestImpure);

/// A systemd-shaped handler kind: `name` is the unit.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
struct UnitSpec {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    run: String,
    #[serde(default)]
    fail: bool,
}

macro_rules! unit_kind {
    ($name:ident, $kind:literal) => {
        struct $name;

        #[async_trait]
        impl Task for $name {
            type Spec = UnitSpec;
            const KIND: &'static str = $kind;
            const LOCKS: &'static [&'static str] = &["systemd"];

            fn key(spec: &UnitSpec) -> String {
                spec.name.clone().unwrap_or_else(|| "all".into())
            }

            async fn plan(&self, _cx: &Cx, spec: &UnitSpec) -> crate::Result<Planned> {
                Ok(Planned::change(Diff::Opaque {
                    summary: format!("{} {}", $kind, Self::key(spec)),
                }))
            }

            async fn apply(
                &self,
                cx: &Cx,
                spec: &UnitSpec,
                _planned: &Planned,
            ) -> crate::Result<Applied> {
                record(
                    &spec.run,
                    format!("handler {}:{} {}", cx.module, $kind, Self::key(spec)),
                );
                if spec.fail {
                    return Err(crate::error::user(
                        "the unit refused to restart",
                        &["Check the unit."],
                    ));
                }
                Ok(Applied::changed(format!("{} {}", $kind, Self::key(spec))))
            }
        }

        crate::register_task!($name);
    };
}

unit_kind!(SystemdRestart, "systemd.restart");
unit_kind!(SystemdReload, "systemd.reload");
unit_kind!(SystemdDaemonReload, "systemd.daemon_reload");

/// Renders a module template secret-aware and writes it through the file sink.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct RenderSpec {
    src: String,
    dest: String,
}

struct TestRender;

#[async_trait]
impl Task for TestRender {
    type Spec = RenderSpec;
    const KIND: &'static str = "test.render";

    fn key(spec: &RenderSpec) -> String {
        spec.dest.clone()
    }

    async fn plan(&self, cx: &Cx, spec: &RenderSpec) -> crate::Result<Planned> {
        let source = cx
            .source
            .read(Path::new(&spec.src))
            .await?
            .ok_or_else(|| crate::error::user("missing template", &["Add it."]))?;
        let source = String::from_utf8_lossy(&source).into_owned();
        let shadow = cx.templater.render_redacted(&source, &cx.host)?;
        Ok(match cx.previous.as_ref().and_then(|p| p.shadow.clone()) {
            Some(old) if old == shadow => Planned::NoOp,
            Some(old) => Planned::change(Diff::unified(
                format!("update {}", spec.dest),
                &old,
                &shadow,
            )),
            None => Planned::change(Diff::Create {
                summary: format!("create {}", spec.dest),
                preview: Some(shadow),
            }),
        })
    }

    async fn apply(
        &self,
        cx: &Cx,
        spec: &RenderSpec,
        _planned: &Planned,
    ) -> crate::Result<Applied> {
        let source = cx
            .source
            .read(Path::new(&spec.src))
            .await?
            .ok_or_else(|| crate::error::user("missing template", &["Add it."]))?;
        let source = String::from_utf8_lossy(&source).into_owned();
        let shadow = cx.templater.render_redacted(&source, &cx.host)?;
        let real = cx.templater.render_sensitive(&source, &cx.host)?;
        let digest = real.digest();
        match real {
            crate::secrets::Rendered::Sensitive(s) => {
                crate::secrets::sinks::FileSink
                    .write(
                        cx.system.as_ref(),
                        Path::new(&spec.dest),
                        &s,
                        &FilePerms::mode(0o600),
                    )
                    .await?;
            }
            crate::secrets::Rendered::Plain(p) => {
                cx.system
                    .write_atomic(Path::new(&spec.dest), p.as_bytes(), &FilePerms::mode(0o644))
                    .await?;
            }
        }
        Ok(Applied::changed(format!("wrote {}", spec.dest)).with_shadow(shadow, digest))
    }
}

crate::register_task!(TestRender);

// ---------------------------------------------------------------------------
// Test health kinds and fact provider
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct ExistsSpec {
    path: String,
}

struct FileExists;

#[async_trait]
impl crate::health::HealthCheck for FileExists {
    type Spec = ExistsSpec;
    const KIND: &'static str = "test.file_exists";

    async fn check(&self, cx: &Cx, spec: &ExistsSpec) -> crate::health::HealthStatus {
        match cx.system.stat(Path::new(&spec.path)).await {
            Ok(Some(_)) => crate::health::HealthStatus::Pass,
            Ok(None) => crate::health::HealthStatus::fail(format!("{} does not exist", spec.path)),
            Err(e) => crate::health::HealthStatus::unknown(e.description()),
        }
    }
}

crate::register_health!(FileExists);

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct FlakySpec {
    run: String,
    pass_after: u32,
}

struct Flaky;

#[async_trait]
impl crate::health::HealthCheck for Flaky {
    type Spec = FlakySpec;
    const KIND: &'static str = "test.flaky";

    async fn check(&self, _cx: &Cx, spec: &FlakySpec) -> crate::health::HealthStatus {
        let n = bump(&format!("{}:health", spec.run));
        record(&spec.run, format!("health attempt {n}"));
        if n >= spec.pass_after {
            crate::health::HealthStatus::Pass
        } else {
            crate::health::HealthStatus::fail(format!("still warming up (attempt {n})"))
        }
    }
}

crate::register_health!(Flaky);

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
struct FilterSpec {
    expr: String,
}

/// Like the standard `filter` check: a filt-rs expression over the host.
struct FilterCheck;

#[async_trait]
impl crate::health::HealthCheck for FilterCheck {
    type Spec = FilterSpec;
    const KIND: &'static str = "test.filter";

    async fn check(&self, cx: &Cx, spec: &FilterSpec) -> crate::health::HealthStatus {
        let filter = Filter::new(spec.expr.as_str()).unwrap();
        match cx.host.try_matches(&filter) {
            Ok(true) => crate::health::HealthStatus::Pass,
            Ok(false) => crate::health::HealthStatus::fail(format!("`{}` is false", spec.expr)),
            Err(e) => crate::health::HealthStatus::unknown(e),
        }
    }
}

crate::register_health!(FilterCheck);

/// Mounts `host.facts.test.counter.value` from the fake file `/facts/counter`.
struct CounterFacts;

#[async_trait]
impl crate::facts::FactProvider for CounterFacts {
    const NAME: &'static str = "test.counter";

    async fn collect(&self, cx: &Cx) -> crate::Result<serde_json::Value> {
        // Like a real provider whose tool is not installed yet: no facts at all.
        Ok(match cx.system.read(Path::new("/facts/counter")).await? {
            Some(b) => serde_json::json!({ "value": String::from_utf8_lossy(&b) }),
            None => serde_json::Value::Null,
        })
    }
}

crate::register_facts!(CounterFacts);

/// Mounts `host.facts.test.imds.provider` from `/facts/imds`; fails while that
/// file says `fail` (an IMDS endpoint timing out).
struct ImdsFacts;

#[async_trait]
impl crate::facts::FactProvider for ImdsFacts {
    const NAME: &'static str = "test.imds";

    async fn collect(&self, cx: &Cx) -> crate::Result<serde_json::Value> {
        match cx.system.read(Path::new("/facts/imds")).await? {
            Some(b) if b == b"fail" => Err(crate::error::system(
                "the metadata endpoint timed out",
                &["Retry later."],
            )),
            Some(b) => Ok(serde_json::json!({ "provider": String::from_utf8_lossy(&b) })),
            None => Ok(serde_json::Value::Null),
        }
    }
}

crate::register_facts!(ImdsFacts);

// ---------------------------------------------------------------------------
// Builders
// ---------------------------------------------------------------------------

fn module(yaml: &str) -> LoadedModule {
    module_with_files(yaml, &[])
}

fn module_with_files(yaml: &str, files: &[(&str, &str)]) -> LoadedModule {
    let config = ModuleConfig::parse(yaml).unwrap_or_else(|e| panic!("{yaml}\n{}", e.message()));
    let mut source = MemorySource::new();
    let mut templates = Vec::new();
    for (path, content) in files {
        source = source.with(path, content);
        if path.ends_with(".j2") {
            templates.push((PathBuf::from(path), content.to_string()));
        }
    }
    LoadedModule {
        config,
        source: Arc::new(source),
        templates,
    }
}

fn revision(id: &str, fleet: &str, modules: Vec<LoadedModule>) -> Arc<Revision> {
    revision_with_vars(id, fleet, modules, &[])
}

fn revision_with_vars(
    id: &str,
    fleet: &str,
    modules: Vec<LoadedModule>,
    var_files: &[(&str, &str)],
) -> Arc<Revision> {
    let fleet = FleetConfig::parse(fleet).unwrap_or_else(|e| panic!("{}", e.message()));
    let mut files = indexmap::IndexMap::new();
    for (path, yaml) in var_files {
        files.insert(PathBuf::from(path), serde_yaml::from_str(yaml).unwrap());
    }
    Arc::new(Revision {
        id: RevisionId::new(id),
        fleet,
        modules,
        var_files: files,
        unloaded: Vec::new(),
        workloads: Vec::new(),
        unloaded_workloads: Vec::new(),
    })
}

#[tokio::test]
async fn steps_list_the_host_values_their_templates_emitted() {
    // `bookworm` is literal text that equals a fact; only the expressions'
    // values (in a field and in the content) are the host's.
    let rev = revision(
        "r1",
        FLEET,
        vec![module_with_files(
            "name: m\ntasks:\n  - test.render: { src: conf.j2, dest: '/out/{{ host.facts.arch }}' }\n",
            &[(
                "conf.j2",
                "codename=bookworm\nfamily={{ host.facts.os.family }}\n",
            )],
        )],
    );
    let sys = FakeSystem::new();
    let plan = plan(&inputs(rev, &sys)).await;
    assert_eq!(
        step(&plan, "m", "/out/aarch64").facts.as_deref(),
        Some(
            &[
                "host.facts.arch".to_owned(),
                "host.facts.os.family".to_owned()
            ][..]
        )
    );
}

fn facts(tree: serde_json::Value) -> Facts {
    Facts {
        tree,
        collected_at: HashMap::new(),
        stale: Default::default(),
    }
}

fn default_facts() -> Facts {
    facts(
        serde_json::json!({"arch": "aarch64", "os": {"family": "debian", "codename": "bookworm"}}),
    )
}

fn labels(pairs: &[(&str, serde_json::Value)]) -> Labels {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn inputs(rev: Arc<Revision>, sys: &FakeSystem) -> Inputs {
    inputs_with_store(rev, sys, Arc::new(MemoryStateStore::default()))
}

fn inputs_with_store(rev: Arc<Revision>, sys: &FakeSystem, store: Arc<MemoryStateStore>) -> Inputs {
    Inputs::new(
        rev,
        HostIdentity {
            id: "h1".into(),
            labels: Labels::new(),
        },
        Arc::new(sys.clone()),
        Arc::new(SecretStore::new()),
        store,
    )
    .with_frozen_facts(default_facts())
    .with_parallelism(4)
}

async fn plan(inputs: &Inputs) -> Plan {
    Engine::plan(inputs)
        .await
        .unwrap_or_else(|e| panic!("plan failed: {}", e.message()))
        .0
}

async fn plan_err(inputs: &Inputs) -> crate::Error {
    match Engine::plan(inputs).await {
        Ok(_) => panic!("expected the plan to fail"),
        Err(e) => e,
    }
}

async fn apply_plan(inputs: &Inputs, plan: &Plan) -> (ApplyOutcome, Vec<ApplyEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
    let collector = tokio::spawn(async move {
        let mut events = Vec::new();
        while let Some(e) = rx.recv().await {
            events.push(e);
        }
        events
    });
    let outcome = Engine::apply(inputs, plan, tx)
        .await
        .unwrap_or_else(|e| panic!("apply failed: {}", e.message()));
    let events = collector.await.unwrap();
    (outcome, events)
}

async fn apply(inputs: &Inputs) -> (ApplyOutcome, Vec<ApplyEvent>) {
    let plan = plan(inputs).await;
    apply_plan(inputs, &plan).await
}

fn step<'a>(plan: &'a Plan, module: &str, key: &str) -> &'a Step {
    plan.steps()
        .find(|s| s.task.module == module && s.resource.key == key)
        .unwrap_or_else(|| panic!("no step {module}/{key} in {plan:#?}"))
}

fn indeterminate<'a>(plan: &'a Plan, module: &str) -> &'a crate::plan::Indeterminate {
    plan.indeterminate
        .iter()
        .find(|i| i.module == module)
        .unwrap_or_else(|| panic!("{module} is not indeterminate in {plan:#?}"))
}

fn module_names(plan: &Plan) -> Vec<&str> {
    plan.modules.iter().map(|m| m.module.as_str()).collect()
}

fn status_of(outcome: &ApplyOutcome, module: &str) -> ModuleStatus {
    outcome
        .module_results
        .iter()
        .find(|m| m.module == module)
        .unwrap_or_else(|| panic!("no result for {module}: {outcome:#?}"))
        .status
}

fn preview(step: &Step) -> String {
    match &step.planned {
        Planned::Change {
            diff: Diff::Create {
                preview: Some(p), ..
            },
        } => p.get().clone(),
        other => panic!("expected a create with preview, got {other:?}"),
    }
}

fn record_for(
    module: &str,
    kind: &str,
    key: &str,
    policy: RemovalPolicy,
    introduced_in: &str,
) -> ResourceRecord {
    ResourceRecord {
        key: ResourceKey::new(module, kind, key),
        spec_digest: Digest::of(b"spec"),
        shadow: None,
        content_digest: None,
        artefacts: vec![],
        removal: policy,
        revision: RevisionId::new(introduced_in),
        applied_at: chrono::Utc::now(),
        introduced_in: RevisionId::new(introduced_in),
    }
}

const FLEET: &str = "version: 2\n";

// ---------------------------------------------------------------------------
// Vars, groups, selection
// ---------------------------------------------------------------------------

const VARS_FLEET: &str = r#"
version: 2
fleet:
  groups:
    home: 'host.labels.site == "home"'
vars:
  - file: vars/defaults.yaml
  - when: 'host.groups contains "home"'
    values: { greeting: "home overlay" }
"#;

const VARS_MODULE: &str = r#"
name: m
vars: { greeting: "module default", other: "x" }
tasks:
  - test.file: { path: /vars/out, content: '{{ vars.greeting }}' }
"#;

#[rstest]
#[case::module_default(&[], &[], false, "module default")]
#[case::file_overlay_beats_module(&[], &[], true, "file overlay")]
#[case::inline_overlay_with_when_beats_file(&[("site", serde_json::json!("home"))], &[], true, "home overlay")]
#[case::inline_overlay_skipped_when_false(&[("site", serde_json::json!("cloud"))], &[], true, "file overlay")]
#[case::agent_vars_win(&[("site", serde_json::json!("home"))], &[("greeting", "agent")], true, "agent")]
#[tokio::test]
async fn vars_follow_the_documented_precedence(
    #[case] host_labels: &[(&str, serde_json::Value)],
    #[case] agent: &[(&str, &str)],
    #[case] file_defines_greeting: bool,
    #[case] expected: &str,
) {
    let defaults = if file_defines_greeting {
        "greeting: file overlay\nunrelated: 1\n"
    } else {
        "unrelated: 1\n"
    };
    let rev = revision_with_vars(
        "r1",
        VARS_FLEET,
        vec![module(VARS_MODULE)],
        &[("vars/defaults.yaml", defaults)],
    );
    let sys = FakeSystem::new();
    let mut inputs = inputs(rev, &sys);
    inputs.host.labels = labels(host_labels);
    inputs.agent_vars = agent
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
        .collect::<Vars>();
    let plan = plan(&inputs).await;
    assert_eq!(preview(step(&plan, "m", "/vars/out")), expected);
}

#[tokio::test]
async fn vars_reference_facts_labels_and_other_vars_lazily() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            r#"
name: m
vars:
  version: "1.2"
  url: 'https://example/{{ vars.name }}-{{ vars.version }}-{{ host.facts.arch }}'
  name: 'tool-{{ host.labels.role }}'
tasks:
  - test.file: { path: /vars/out, content: '{{ vars.url }}' }
"#,
        )],
    );
    let sys = FakeSystem::new();
    let mut inputs = inputs(rev, &sys);
    inputs.host.labels = labels(&[("role", serde_json::json!("worker"))]);
    let plan = plan(&inputs).await;
    assert_eq!(
        preview(step(&plan, "m", "/vars/out")),
        "https://example/tool-worker-1.2-aarch64"
    );
}

#[tokio::test]
async fn var_reference_cycles_are_user_errors_naming_the_cycle() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\nvars: { a: '{{ vars.b }}', b: '{{ vars.c }}', c: '{{ vars.a }}' }\ntasks: [ { test.file: { path: /x, content: '{{ vars.a }}' } } ]\n",
        )],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    let broken = indeterminate(&plan, "m");
    assert!(
        broken.plan_error,
        "a selected module with broken vars is a plan error for that module"
    );
    assert!(broken.reason.contains("cycle"), "{}", broken.reason);
    assert!(
        broken.reason.contains("a -> b -> c -> a"),
        "{}",
        broken.reason
    );
    assert!(plan.modules.is_empty());
}

#[tokio::test]
async fn undefined_var_and_secret_references_in_vars_are_user_errors() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\nvars: { a: '{{ vars.nope }}' }\ntasks: [ { test.file: { path: /x } } ]\n",
        )],
    );
    let plan = self::plan(&inputs(rev, &FakeSystem::new())).await;
    let broken = indeterminate(&plan, "m");
    assert!(broken.plan_error);
    assert!(broken.reason.contains("vars.nope"), "{}", broken.reason);

    let rev = revision(
        "r1",
        "version: 2\nsecrets: { token: { env: T } }\n",
        vec![module(
            "name: m\nvars: { a: '{{ secrets.token }}' }\ntasks: [ { test.file: { path: /x } } ]\n",
        )],
    );
    let mut inputs = inputs(rev, &FakeSystem::new());
    let mut store = SecretStore::new();
    store.insert(Secret::new(
        "token",
        "hunter2-vars".into(),
        &InstallKey::from_bytes([1; 32]),
    ));
    inputs.secrets = Arc::new(store);
    let plan = self::plan(&inputs).await;
    let broken = indeterminate(&plan, "m");
    assert!(broken.plan_error);
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains("hunter2-vars"), "{json}");
    assert!(
        broken.reason.contains("could not be rendered"),
        "{}",
        broken.reason
    );
    // Applying reports the module as failed without running anything.
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(!outcome.succeeded);
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Failed);
    assert_eq!(outcome.failed_module.as_deref(), Some("m"));
}

#[tokio::test]
async fn vars_errors_in_unselected_modules_do_not_fail_the_plan() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: broken\nwhen: 'host.labels.role == \"never\"'\nvars: { a: '{{ vars.nope }}' }\n",
            ),
            module("name: fine\ntasks: [ { test.file: { path: /fine } } ]\n"),
        ],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    assert_eq!(module_names(&plan), vec!["fine"]);
    assert_eq!(plan.skipped, vec!["broken"]);
}

#[tokio::test]
async fn groups_derive_from_labels_and_facts_and_select_modules() {
    let rev = revision(
        "r1",
        r#"
version: 2
fleet:
  groups:
    home: 'host.labels.site == "home"'
    arm: 'host.facts.arch == "aarch64"'
    cloud: 'host.facts.cloud.provider in ["oci", "azure"]'
"#,
        vec![
            module("name: on_arm\nwhen: 'host.groups contains \"arm\"'\n"),
            module("name: on_home\nwhen: 'host.groups contains \"home\"'\n"),
            module("name: on_cloud\nwhen: 'host.groups contains \"cloud\"'\n"),
            module("name: always\n"),
        ],
    );
    let sys = FakeSystem::new();
    let mut inputs = inputs(rev, &sys);
    inputs.host.labels = labels(&[("site", serde_json::json!("home"))]);
    let plan = plan(&inputs).await;
    assert_eq!(module_names(&plan), vec!["always", "on_arm", "on_home"]);
    assert_eq!(plan.skipped, vec!["on_cloud"]);
}

#[tokio::test]
async fn server_labels_override_agent_labels_in_selection() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: workers\nwhen: 'host.labels.role == \"worker\"'\n",
        )],
    );
    let sys = FakeSystem::new();
    let mut inputs = inputs(rev, &sys);
    inputs.host.labels = labels(&[("role", serde_json::json!("server"))]);
    inputs.server_labels = labels(&[("role", serde_json::json!("worker"))]);
    let plan = plan(&inputs).await;
    assert_eq!(module_names(&plan), vec!["workers"]);
}

// ---------------------------------------------------------------------------
// Uncertain selection never removes anything
// ---------------------------------------------------------------------------

/// A host whose IMDS provider fails, with records for every module below.
fn uncertain_inputs(rev: Arc<Revision>, sys: &FakeSystem, modules: &[&str]) -> Inputs {
    let mut state = State::default();
    for m in modules {
        state.record(record_for(
            m,
            "test.file",
            &format!("/unc/{m}"),
            RemovalPolicy::Cleanup,
            "r0",
        ));
        let _ = sys
            .clone()
            .with_file(format!("/unc/{m}"), "", FilePerms::mode(0o644));
    }
    let mut inputs = inputs_with_store(rev, sys, Arc::new(MemoryStateStore::with(state)));
    inputs.facts_frozen = false;
    inputs.facts = Facts::new();
    inputs
}

#[tokio::test]
async fn a_failing_fact_provider_makes_dependent_selection_indeterminate_not_unselected() {
    let rev = revision(
        "r1",
        r#"
version: 2
fleet:
  groups:
    cloud: 'host.facts.test.imds.provider == "oci"'
    home: 'host.labels.site == "home"'
"#,
        vec![
            module(
                "name: oci\nwhen: 'host.facts.test.imds.provider == \"oci\"'\ntasks: [ { test.file: { path: /unc/oci } } ]\n",
            ),
            module(
                "name: cloud\nwhen: 'host.groups contains \"cloud\"'\ntasks: [ { test.file: { path: /unc/cloud } } ]\n",
            ),
            module(
                "name: home\nwhen: 'host.groups contains \"home\"'\ntasks: [ { test.file: { path: /unc/home } } ]\n",
            ),
            module("name: after\nneeds: [oci]\ntasks: [ { test.file: { path: /unc/after } } ]\n"),
        ],
    );
    let sys = FakeSystem::new().with_file("/facts/imds", "fail", FilePerms::mode(0o644));
    let inputs = uncertain_inputs(rev, &sys, &["oci", "cloud", "home", "after"]);
    let (plan, facts) = Engine::plan(&inputs).await.unwrap();
    assert!(facts.stale.contains("test.imds"));
    let names: Vec<&str> = plan
        .indeterminate
        .iter()
        .map(|i| i.module.as_str())
        .collect();
    assert_eq!(names, vec!["after", "cloud", "oci"]);
    assert!(plan.indeterminate.iter().all(|i| !i.plan_error));
    assert!(
        indeterminate(&plan, "oci")
            .reason
            .contains("host.facts.test.imds"),
        "{plan:#?}"
    );
    assert!(
        indeterminate(&plan, "cloud").reason.contains("cloud"),
        "{plan:#?}"
    );
    assert!(
        indeterminate(&plan, "after").reason.contains("needs 'oci'"),
        "{plan:#?}"
    );
    assert_eq!(
        plan.skipped,
        vec!["home"],
        "a decidable selector is still decided"
    );
    let removed: Vec<String> = plan
        .removals
        .iter()
        .map(|r| r.resource.to_string())
        .collect();
    assert_eq!(removed, vec!["home/test.file[/unc/home]"]);
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("'oci' is indeterminate")),
        "{:?}",
        plan.warnings
    );

    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(
        outcome.succeeded,
        "indeterminate modules are reported, not failed: {outcome:#?}"
    );
    assert_eq!(status_of(&outcome, "oci"), ModuleStatus::Indeterminate);
    assert!(sys.exists("/unc/oci") && sys.exists("/unc/cloud") && sys.exists("/unc/after"));
    let state = inputs.state.load().unwrap();
    for m in ["oci", "cloud", "after"] {
        assert!(
            state
                .get(&ResourceKey::new(m, "test.file", format!("/unc/{m}")))
                .is_some(),
            "{m} keeps its record"
        );
    }
}

#[tokio::test]
async fn a_provider_failure_after_a_good_collection_keeps_selecting_on_the_last_value() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: oci\nwhen: 'host.facts.test.imds.provider == \"oci\"'\ntasks: [ { test.file: { path: /unc/oci } } ]\n",
        )],
    );
    let sys = FakeSystem::new().with_file("/facts/imds", "fail", FilePerms::mode(0o644));
    let mut inputs = uncertain_inputs(rev, &sys, &["oci"]);
    let mut seeded = Facts::new();
    seeded.mount(
        "test.imds",
        serde_json::json!({"provider": "oci"}),
        sys.now() - chrono::Duration::days(1),
    );
    inputs.facts = seeded;
    let (plan, facts) = Engine::plan(&inputs).await.unwrap();
    assert!(facts.stale.contains("test.imds"));
    assert_eq!(facts.tree["test"]["imds"]["provider"], "oci");
    assert!(plan.indeterminate.is_empty(), "{plan:#?}");
    assert_eq!(module_names(&plan), vec!["oci"]);
    assert!(plan.removals.is_empty());
}

#[tokio::test]
async fn a_task_when_reading_unavailable_facts_keeps_the_resource_unplanned() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /unc/m }\n    when: 'host.facts.test.imds.provider == \"oci\"'\n  - test.file: { path: /unc/plain }\n",
        )],
    );
    let sys = FakeSystem::new().with_file("/facts/imds", "fail", FilePerms::mode(0o644));
    let inputs = uncertain_inputs(rev, &sys, &["m"]);
    let plan = plan(&inputs).await;
    let keys: Vec<&str> = plan.steps().map(|s| s.resource.key.as_str()).collect();
    assert_eq!(keys, vec!["/unc/plain"]);
    assert!(plan.removals.is_empty(), "{:#?}", plan.removals);
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("/unc/m") && w.contains("cannot be decided")),
        "{:?}",
        plan.warnings
    );
}

#[rstest]
#[case::bare_when_true(None, true)]
#[case::when_reads_vars(Some("vars.enabled == true"), false)]
#[tokio::test]
async fn a_module_whose_vars_fail_to_render_keeps_its_resources(
    #[case] when: Option<&str>,
    #[case] plan_error: bool,
) {
    let when = when.map(|w| format!("when: '{w}'\n")).unwrap_or_default();
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(&format!(
                "name: m\n{when}vars: {{ enabled: true, a: '{{{{ vars.nope }}}}' }}\ntasks: [ {{ test.file: {{ path: /unc/m }} }} ]\n"
            )),
            module("name: n\nneeds: [m]\ntasks: [ { test.file: { path: /unc/n } } ]\n"),
        ],
    );
    let sys = FakeSystem::new();
    let mut inputs = uncertain_inputs(rev, &sys, &["m", "n"]);
    inputs.facts_frozen = true;
    let plan = plan(&inputs).await;
    assert_eq!(
        indeterminate(&plan, "m").plan_error,
        plan_error,
        "{plan:#?}"
    );
    assert!(indeterminate(&plan, "n").reason.contains("needs 'm'"));
    assert!(plan.removals.is_empty(), "{:#?}", plan.removals);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert_eq!(outcome.succeeded, !plan_error, "{outcome:#?}");
    assert!(sys.exists("/unc/m") && sys.exists("/unc/n"));
    assert_eq!(inputs.state.load().unwrap().resources.len(), 2);
}

#[tokio::test]
async fn a_module_directory_without_module_yaml_keeps_its_resources() {
    let mut rev = Arc::try_unwrap(revision("r1", FLEET, vec![module("name: other\n")])).unwrap();
    rev.unloaded = vec!["half".into()];
    let rev = Arc::new(rev);
    let sys = FakeSystem::new();
    let mut inputs = uncertain_inputs(rev, &sys, &["half"]);
    inputs.facts_frozen = true;
    let plan = plan(&inputs).await;
    assert!(plan.removals.is_empty(), "{:#?}", plan.removals);
    assert!(indeterminate(&plan, "half").reason.contains("module.yaml"));
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);
    assert!(sys.exists("/unc/half"));
}

#[tokio::test]
async fn removals_can_be_listed_but_not_applied() {
    let rev = revision("r1", FLEET, vec![module("name: other\n")]);
    let sys = FakeSystem::new();
    let mut inputs = uncertain_inputs(rev, &sys, &["gone"]);
    inputs.facts_frozen = true;
    let inputs = inputs.with_removals(false);
    let plan = plan(&inputs).await;
    assert_eq!(plan.removals.len(), 1);
    let (outcome, events) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(outcome.removals, 0);
    assert!(sys.exists("/unc/gone"));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ApplyEvent::RemovalRun { .. }))
    );
    assert_eq!(inputs.state.load().unwrap().resources.len(), 1);
    // The same plan applied with removals enabled removes it.
    let inputs = inputs.with_removals(true);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert_eq!(outcome.removals, 1);
    assert!(!sys.exists("/unc/gone"));
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn modules_are_ordered_along_needs_deterministically() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module("name: c\nneeds: [b]\n"),
            module("name: zeta\n"),
            module("name: b\nneeds: [a]\n"),
            module("name: a\n"),
            module("name: alpha\n"),
        ],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    assert_eq!(module_names(&plan), vec!["a", "alpha", "zeta", "b", "c"]);
    assert_eq!(plan.modules[3].needs, vec!["a"]);
}

#[tokio::test]
async fn needs_cycles_are_reported() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module("name: a\nneeds: [b]\n"),
            module("name: b\nneeds: [a]\n"),
        ],
    );
    let report = validate(&rev).unwrap();
    assert!(
        report.errors.iter().any(|e| e.message.contains("cycle")),
        "{report}"
    );
    let err = plan_err(&inputs(rev, &FakeSystem::new())).await;
    assert!(err.message().contains("cycle"), "{}", err.message());
}

#[tokio::test]
async fn a_selected_module_needing_an_unselected_one_is_a_user_error_with_advice() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module("name: docker\nwhen: 'host.labels.docker == true'\n"),
            module("name: nomad\nneeds: [docker]\n"),
        ],
    );
    let err = plan_err(&inputs(rev, &FakeSystem::new())).await;
    assert!(err.is(crate::error::Kind::User));
    assert!(err.message().contains("'nomad'"), "{}", err.message());
    assert!(err.message().contains("'docker'"), "{}", err.message());
    assert!(err.message().contains("not selected"), "{}", err.message());
    assert!(err.advice().iter().any(|a| a.contains("when")));
}

// ---------------------------------------------------------------------------
// Scheduling: concurrency and locks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn independent_modules_apply_concurrently() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: a\ntasks: [ { test.file: { path: /par/a, run: par1, signal: par1-a, wait_for: par1-b } } ]\n",
            ),
            module(
                "name: b\ntasks: [ { test.file: { path: /par/b, run: par1, signal: par1-b, wait_for: par1-a } } ]\n",
            ),
        ],
    );
    let sys = FakeSystem::new();
    let (outcome, _) = tokio::time::timeout(Duration::from_secs(10), apply(&inputs(rev, &sys)))
        .await
        .expect("both modules must run at once or the barriers deadlock");
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(outcome.changes, 2);
    assert!(sys.exists("/par/a") && sys.exists("/par/b"));
}

#[tokio::test]
async fn parallelism_one_runs_modules_sequentially() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: a\ntasks: [ { test.file: { path: /seq/a, run: seq1, sleep_ms: 20 } } ]\n",
            ),
            module(
                "name: b\ntasks: [ { test.file: { path: /seq/b, run: seq1, sleep_ms: 20 } } ]\n",
            ),
        ],
    );
    let sys = FakeSystem::new();
    let inputs = inputs(rev, &sys).with_parallelism(1);
    let (outcome, _) = apply(&inputs).await;
    assert!(outcome.succeeded);
    let order: Vec<String> = calls("seq1")
        .into_iter()
        .filter(|c| c.starts_with("apply:"))
        .collect();
    assert_eq!(
        order,
        vec![
            "apply:start a//seq/a",
            "apply:end a//seq/a",
            "apply:start b//seq/b",
            "apply:end b//seq/b"
        ]
    );
}

#[tokio::test]
async fn locked_steps_serialise_across_modules_while_unlocked_ones_overlap() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: a\ntasks:\n  - test.file: { path: /lock/a-free, run: lock1, signal: lock1-a, wait_for: lock1-b }\n  - test.locked: { path: /lock/a-locked, run: lock1, sleep_ms: 15 }\n",
            ),
            module(
                "name: b\ntasks:\n  - test.file: { path: /lock/b-free, run: lock1, signal: lock1-b, wait_for: lock1-a }\n  - test.locked: { path: /lock/b-locked, run: lock1, sleep_ms: 15 }\n",
            ),
        ],
    );
    let sys = FakeSystem::new();
    let (outcome, _) = tokio::time::timeout(Duration::from_secs(10), apply(&inputs(rev, &sys)))
        .await
        .expect("unlocked steps overlap");
    assert!(outcome.succeeded, "{outcome:#?}");
    let locked: Vec<String> = calls("lock1")
        .into_iter()
        .filter(|c| c.contains("-locked") && c.starts_with("apply:"))
        .collect();
    // Whichever module took the lock first ran start → end before the other started.
    assert_eq!(locked.len(), 4, "{locked:?}");
    assert!(locked[0].starts_with("apply:start"), "{locked:?}");
    assert!(locked[1].starts_with("apply:end"), "{locked:?}");
    assert_eq!(
        locked[0].trim_start_matches("apply:start"),
        locked[1].trim_start_matches("apply:end")
    );
    assert!(
        locked[2].starts_with("apply:start") && locked[3].starts_with("apply:end"),
        "{locked:?}"
    );
}

#[tokio::test]
async fn tasks_within_a_module_run_in_declaration_order() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /ord/1, run: ord1 }\n  - test.file: { path: /ord/2, run: ord1 }\n  - test.file: { path: /ord/3, run: ord1 }\n",
        )],
    );
    let (outcome, events) = apply(&inputs(rev, &FakeSystem::new())).await;
    assert!(outcome.succeeded);
    let ends: Vec<String> = calls("ord1")
        .into_iter()
        .filter(|c| c.starts_with("apply:end"))
        .collect();
    assert_eq!(
        ends,
        vec![
            "apply:end m//ord/1",
            "apply:end m//ord/2",
            "apply:end m//ord/3"
        ]
    );
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            ApplyEvent::ModuleStarted { .. } => "module_started",
            ApplyEvent::StepStarted { .. } => "step_started",
            ApplyEvent::StepFinished { .. } => "step_finished",
            ApplyEvent::ModuleFinished { .. } => "module_finished",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "module_started",
            "step_started",
            "step_finished",
            "step_started",
            "step_finished",
            "step_started",
            "step_finished",
            "module_finished"
        ]
    );
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

const HANDLER_MODULES: &[&str] = &[
    r#"
name: nomad
needs: [docker]
tasks:
  - test.file: { path: /h/a, content: a, run: hand1 }
    notify: [reload-nomad, restart-nomad, docker:restart-docker]
  - test.file: { path: /h/b, content: b, run: hand1 }
    notify: [daemon-reload, restart-nomad, misc]
  - test.file: { path: /h/noop, content: present, run: hand1 }
    notify: [reload-other]
handlers:
  daemon-reload: { systemd.daemon_reload: { run: hand1 } }
  restart-nomad: { systemd.restart: { name: nomad, run: hand1 } }
  reload-nomad: { systemd.reload: { name: nomad, run: hand1 } }
  reload-other: { systemd.reload: { name: other, run: hand1 } }
  misc: { test.file: { path: /h/misc-handler, content: ran, run: hand1 } }
"#,
    r#"
name: docker
handlers:
  restart-docker: { systemd.restart: { name: docker, run: hand1 } }
"#,
];

#[tokio::test]
async fn handlers_dedupe_order_and_subsume_and_may_belong_to_other_modules() {
    let rev = revision(
        "r1",
        FLEET,
        HANDLER_MODULES.iter().map(|m| module(m)).collect(),
    );
    let sys = FakeSystem::new().with_file("/h/noop", "present", FilePerms::mode(0o644));
    let inputs = inputs(rev, &sys);
    let plan = plan(&inputs).await;
    let nomad = plan.modules.iter().find(|m| m.module == "nomad").unwrap();
    let runs: Vec<(&str, &str, &str)> = nomad
        .handlers
        .iter()
        .map(|h| (h.module.as_str(), h.handler.as_str(), h.kind.as_str()))
        .collect();
    assert_eq!(
        runs,
        vec![
            ("nomad", "daemon-reload", "systemd.daemon_reload"),
            ("nomad", "restart-nomad", "systemd.restart"),
            ("docker", "restart-docker", "systemd.restart"),
            ("nomad", "misc", "test.file"),
        ],
        "reload-nomad is subsumed by restart-nomad; reload-other is only notified by a NoOp step"
    );
    assert!(
        plan.modules
            .iter()
            .find(|m| m.module == "docker")
            .unwrap()
            .handlers
            .is_empty()
    );

    let (outcome, events) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    let handler_calls: Vec<String> = calls("hand1")
        .into_iter()
        .filter(|c| c.starts_with("handler"))
        .collect();
    assert_eq!(
        handler_calls,
        vec![
            "handler nomad:systemd.daemon_reload all",
            "handler nomad:systemd.restart nomad",
            "handler docker:systemd.restart docker",
        ]
    );
    assert_eq!(sys.file_string("/h/misc-handler").as_deref(), Some("ran"));
    let handler_events: Vec<(String, String)> = events
        .iter()
        .filter_map(|e| match e {
            ApplyEvent::HandlerRun { module, handler } => Some((module.clone(), handler.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(handler_events.len(), 4);
    assert_eq!(
        handler_events[2],
        ("docker".to_string(), "restart-docker".to_string())
    );
    // Handlers are never recorded as resources.
    let state = inputs.state.load().unwrap();
    assert!(
        state.resources.keys().all(|k| !k.contains("misc-handler")),
        "{:?}",
        state.resources.keys()
    );
}

fn handler_calls(run: &str) -> Vec<String> {
    calls(run)
        .into_iter()
        .filter(|c| c.starts_with("handler") || c.starts_with("apply:end"))
        .collect()
}

#[tokio::test]
async fn handlers_run_only_when_a_notifying_step_really_changed() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /hc/a, run: hand3, unchanged: true }\n    notify: [restart]\nhandlers:\n  restart: { systemd.restart: { name: m, run: hand3 } }\n",
        )],
    );
    let (outcome, events) = apply(&inputs(rev, &FakeSystem::new())).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert!(handler_calls("hand3").is_empty(), "{:?}", calls("hand3"));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ApplyEvent::HandlerRun { .. }))
    );
}

#[tokio::test]
async fn handlers_dedupe_by_kind_and_target_across_declarations() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: nomad\ntasks:\n  - test.file: { path: /hd/a, run: hand4 }\n    notify: [restart, docker:restart-nomad]\nhandlers:\n  restart: { systemd.restart: { name: nomad, run: hand4 } }\n",
            ),
            module(
                "name: docker\nhandlers:\n  restart-nomad: { systemd.restart: { name: nomad, run: hand4 } }\n",
            ),
        ],
    );
    let (outcome, _) = apply(&inputs(rev, &FakeSystem::new())).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    let restarts: Vec<String> = calls("hand4")
        .into_iter()
        .filter(|c| c.starts_with("handler"))
        .collect();
    assert_eq!(restarts, vec!["handler nomad:systemd.restart nomad"]);
}

#[tokio::test]
async fn a_handler_of_a_later_module_runs_once_at_the_end_of_that_module() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: common\ntasks:\n  - test.file: { path: /hl/common, run: hand5 }\n    notify: [nomad:restart]\n",
            ),
            module(
                "name: nomad\nneeds: [common]\ntasks:\n  - test.file: { path: /hl/nomad, run: hand5 }\n    notify: [restart]\nhandlers:\n  restart: { systemd.restart: { name: nomad, run: hand5 } }\n",
            ),
        ],
    );
    let inputs = inputs(rev, &FakeSystem::new());
    let plan = plan(&inputs).await;
    let common = plan.modules.iter().find(|m| m.module == "common").unwrap();
    let nomad = plan.modules.iter().find(|m| m.module == "nomad").unwrap();
    assert!(common.handlers.is_empty(), "{:#?}", common.handlers);
    assert_eq!(nomad.handlers.len(), 1, "{:#?}", nomad.handlers);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(
        handler_calls("hand5"),
        vec![
            "apply:end common//hl/common",
            "apply:end nomad//hl/nomad",
            "handler nomad:systemd.restart nomad",
        ]
    );
}

#[tokio::test]
async fn removing_a_task_runs_the_handlers_it_notified() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let handlers = "handlers:\n  restart: { systemd.restart: { name: svc, run: hand6 } }\n";
    let r1 = revision(
        "r1",
        FLEET,
        vec![module(&format!(
            "name: m\ntasks:\n  - test.file: {{ path: /hr/conf, run: hand6 }}\n    notify: [restart]\n{handlers}"
        ))],
    );
    let (outcome, _) = apply(&inputs_with_store(r1, &sys, store.clone())).await;
    assert!(outcome.succeeded);
    assert_eq!(
        store
            .load()
            .unwrap()
            .notifies_of(&ResourceKey::new("m", "test.file", "/hr/conf")),
        &["m:restart".to_string()]
    );
    let before = calls("hand6")
        .iter()
        .filter(|c| c.starts_with("handler"))
        .count();

    let r2 = revision("r2", FLEET, vec![module(&format!("name: m\n{handlers}"))]);
    let (outcome, events) = apply(&inputs_with_store(r2, &sys, store.clone())).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(outcome.removals, 1);
    assert!(!sys.exists("/hr/conf"));
    assert_eq!(
        calls("hand6")
            .iter()
            .filter(|c| c.starts_with("handler"))
            .count(),
        before + 1,
        "the removal ran the handler its task notified"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ApplyEvent::HandlerRun { handler, .. } if handler == "restart"))
    );
    assert!(store.load().unwrap().notifies.is_empty());
}

#[tokio::test]
async fn a_failing_handler_fails_its_module() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /hf/a, run: hand2 }\n    notify: [boom]\nhandlers:\n  boom: { systemd.restart: { name: x, run: hand2, fail: true } }\n",
        )],
    );
    let (outcome, events) = apply(&inputs(rev, &FakeSystem::new())).await;
    assert!(!outcome.succeeded);
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Failed);
    assert!(
        outcome.error.as_deref().unwrap_or("").contains("boom"),
        "{outcome:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ApplyEvent::StepFailed { task_id, .. } if task_id == "m:boom"))
    );
}

// ---------------------------------------------------------------------------
// Deferral and facts refresh
// ---------------------------------------------------------------------------

const PROVIDER_MODULES: &[&str] = &[
    "name: p\nfacts: { provides: [test.counter] }\ntasks: [ { test.file: { path: /facts/counter, content: \"42\", run: defer1 } } ]\n",
    "name: q\nneeds: [p]\ntasks: [ { test.file: { path: /defer/out, content: 'v={{ host.facts.test.counter.value }}', run: defer1 } } ]\n",
    "name: r\nneeds: [q]\ntasks: [ { test.file: { path: /defer/r, content: r, run: defer1 } } ]\n",
];

#[tokio::test]
async fn modules_needing_fresh_facts_are_deferred_in_plan_and_resolved_in_apply() {
    let rev = revision(
        "r1",
        FLEET,
        PROVIDER_MODULES.iter().map(|m| module(m)).collect(),
    );
    let sys = FakeSystem::new();
    let mut inputs = inputs(rev.clone(), &sys);
    inputs.facts_frozen = false;
    inputs.facts = Facts::new();

    let plan = plan(&inputs).await;
    assert_eq!(module_names(&plan), vec!["p"]);
    let deferred: Vec<(&str, &str)> = plan
        .deferred
        .iter()
        .map(|d| (d.module.as_str(), d.reason.as_str()))
        .collect();
    assert_eq!(
        deferred,
        vec![
            ("q", "waits for facts from p"),
            ("r", "waits for q, which is deferred")
        ]
    );

    // Both are previewed for review, the fact they wait for as a placeholder.
    let preview = |m: &str| {
        let d = plan.deferred.iter().find(|d| d.module == m).unwrap();
        format!("{:?}", d.preview.as_ref().expect("previewed"))
    };
    assert!(
        preview("q").contains("«fact:test.counter.value»"),
        "{}",
        preview("q")
    );
    assert!(preview("r").contains("/defer/r"), "{}", preview("r"));

    let (outcome, events) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(status_of(&outcome, "p"), ModuleStatus::Applied);
    assert_eq!(status_of(&outcome, "q"), ModuleStatus::Applied);
    assert_eq!(status_of(&outcome, "r"), ModuleStatus::Applied);
    assert_eq!(sys.file_string("/defer/out").as_deref(), Some("v=42"));
    assert!(events.iter().any(
        |e| matches!(e, ApplyEvent::FactsRefreshed { providers } if providers == &["test.counter"])
    ));

    // Once the provider module is a NoOp, nothing is deferred any more.
    let plan2 = self::plan(&inputs).await;
    assert!(plan2.deferred.is_empty(), "{:#?}", plan2.deferred);
    assert_eq!(module_names(&plan2), vec!["p", "q", "r"]);
    assert_eq!(step(&plan2, "q", "/defer/out").planned, Planned::NoOp);
}

#[tokio::test]
async fn a_deferred_module_that_diverges_from_its_preview_is_not_applied() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: p\nfacts: { provides: [test.counter] }\ntasks: [ { test.file: { path: /facts/counter, content: \"7\", run: diverge1 } } ]\n",
            ),
            // The resource itself is named by the provided fact.
            module(
                "name: q\nneeds: [p]\ntasks: [ { test.file: { path: '/diverge/{{ host.facts.test.counter.value }}', run: diverge1 } } ]\n",
            ),
        ],
    );
    let sys = FakeSystem::new();
    let mut inputs = inputs(rev, &sys);
    inputs.facts_frozen = false;
    inputs.facts = Facts::new();
    let plan = plan(&inputs).await;
    assert!(plan.deferred[0].preview.is_some(), "{plan:#?}");
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(!outcome.succeeded, "{outcome:#?}");
    assert_eq!(status_of(&outcome, "p"), ModuleStatus::Applied);
    let q = module_outcome(&outcome, "q");
    assert!(q.stale, "{q:#?}");
    assert!(!sys.exists("/diverge/7"), "nothing of it ran");
}

#[tokio::test]
async fn a_first_install_defers_modules_whose_vars_read_facts_a_provider_will_produce() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: p\nfacts: { provides: [test.counter] }\ntasks: [ { test.file: { path: /facts/counter, content: \"10.0.0.7\", run: lazy1 } } ]\n",
            ),
            module(
                "name: q\nneeds: [p]\nvars: { addr: '{{ host.facts.test.counter.value }}' }\ntasks: [ { test.file: { path: /lazy/q, content: 'bind={{ vars.addr }}', run: lazy1 } } ]\n",
            ),
        ],
    );
    let sys = FakeSystem::new().with_file("/lazy/old", "x", FilePerms::mode(0o644));
    let mut state = State::default();
    state.record(record_for(
        "q",
        "test.file",
        "/lazy/old",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    let mut inputs = inputs_with_store(rev, &sys, Arc::new(MemoryStateStore::with(state)));
    inputs.facts_frozen = false;
    inputs.facts = Facts::new();
    let plan = plan(&inputs).await;
    assert!(plan.indeterminate.is_empty(), "{:#?}", plan.indeterminate);
    assert_eq!(plan.deferred.len(), 1, "{plan:#?}");
    assert_eq!(plan.deferred[0].module, "q");
    assert!(
        plan.removals.is_empty(),
        "a deferred module's resources wait for its plan"
    );
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(sys.file_string("/lazy/q").as_deref(), Some("bind=10.0.0.7"));
    assert!(
        !sys.exists("/lazy/old"),
        "its orphans are computed in the same run"
    );
    assert_eq!(outcome.removals, 1);
}

#[tokio::test]
async fn a_task_excluded_by_when_never_renders_its_fields() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: '/when/{{ vars.only_on_db }}' }\n    when: 'host.labels.role == \"db\"'\n  - test.file: { path: /when/plain }\n",
        )],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    assert!(plan.indeterminate.is_empty(), "{:#?}", plan.indeterminate);
    let keys: Vec<&str> = plan.steps().map(|s| s.resource.key.as_str()).collect();
    assert_eq!(keys, vec!["/when/plain"]);
}

#[rstest]
#[case::selected_after_the_refresh("42", true)]
#[case::unselected_after_the_refresh("41", false)]
#[tokio::test]
async fn a_when_reading_provider_facts_is_decided_after_the_provider_applies(
    #[case] value: &str,
    #[case] selected: bool,
) {
    let run = format!("cond-{value}");
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(&format!(
                "name: p\nfacts: {{ provides: [test.counter] }}\ntasks: [ {{ test.file: {{ path: /facts/counter, content: \"{value}\", run: {run} }} }} ]\n"
            )),
            module(&format!(
                "name: r\nwhen: 'host.facts.test.counter.value == \"42\"'\ntasks: [ {{ test.file: {{ path: /cond/{value}, content: r, run: {run} }} }} ]\n"
            )),
        ],
    );
    let sys = FakeSystem::new().with_file(format!("/cond/{value}"), "old", FilePerms::mode(0o644));
    let mut state = State::default();
    state.record(record_for(
        "r",
        "test.file",
        &format!("/cond/{value}"),
        RemovalPolicy::Cleanup,
        "r0",
    ));
    let mut inputs = inputs_with_store(rev, &sys, Arc::new(MemoryStateStore::with(state)));
    inputs.facts_frozen = false;
    inputs.facts = Facts::new();
    let plan = plan(&inputs).await;
    assert_eq!(plan.deferred.len(), 1, "{plan:#?}");
    assert!(
        plan.deferred[0].reason.contains("decided once"),
        "{:?}",
        plan.deferred
    );
    assert!(
        plan.removals.is_empty(),
        "not removed on a guess: {:#?}",
        plan.removals
    );
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    if selected {
        assert_eq!(status_of(&outcome, "r"), ModuleStatus::Applied);
        assert_eq!(
            sys.file_string(format!("/cond/{value}")).as_deref(),
            Some("r")
        );
    } else {
        assert_eq!(status_of(&outcome, "r"), ModuleStatus::Skipped);
        assert!(
            !sys.exists(format!("/cond/{value}")),
            "decided unselected: its resources go"
        );
        assert_eq!(outcome.removals, 1);
    }
}

#[tokio::test]
async fn rendered_fields_take_the_type_the_kind_declares() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\nvars: { n: 2 }\ntasks: [ { test.file: { path: /typed, sleep_ms: '{{ vars.n }}', fail_times: '{{ vars.n - 2 }}' } } ]\n",
        )],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    assert!(plan.indeterminate.is_empty(), "{:#?}", plan.indeterminate);
    assert_eq!(plan.changes(), 1);
}

// ---------------------------------------------------------------------------
// Tasks: keys, when, select, platforms, always-runs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_resource_keys_are_user_errors_naming_both_tasks() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /dup }\n  - test.file: { path: /other }\n  - test.file: { path: /dup }\n    name: second\n",
        )],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    let broken = indeterminate(&plan, "m");
    assert!(broken.plan_error);
    let msg = &broken.reason;
    assert!(msg.contains("task #1"), "{msg}");
    assert!(msg.contains("task #3 'second'"), "{msg}");
    assert!(msg.contains("m/test.file[/dup]"), "{msg}");
    assert!(msg.contains("`id`"), "the advice is kept: {msg}");
}

#[tokio::test]
async fn explicit_ids_distinguish_otherwise_identical_tasks() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /dup }\n    id: first\n  - test.file: { path: /dup }\n    id: second\n",
        )],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    let keys: Vec<&str> = plan.steps().map(|s| s.resource.key.as_str()).collect();
    assert_eq!(keys, vec!["first", "second"]);
}

#[tokio::test]
async fn task_level_when_omits_tasks_and_select_narrows_without_orphaning() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            r#"
name: m
tasks:
  - test.file: { path: /sel/net, run: sel1 }
    tags: [network]
  - test.file: { path: /sel/plain, run: sel1 }
  - test.file: { path: /sel/never, run: sel1 }
    when: 'host.labels.role == "nope"'
"#,
        )],
    );
    let sys = FakeSystem::new();
    let mut state = State::default();
    state.record(record_for(
        "m",
        "test.file",
        "/sel/plain",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    state.record(record_for(
        "m",
        "test.file",
        "/sel/never",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    let store = Arc::new(MemoryStateStore::with(state));
    let inputs = inputs_with_store(rev, &sys, store).with_select(Some(
        Filter::new(r#"task.tags contains "network""#).unwrap(),
    ));
    let plan = plan(&inputs).await;
    let keys: Vec<&str> = plan.steps().map(|s| s.resource.key.as_str()).collect();
    assert_eq!(keys, vec!["/sel/net"], "only the selected task is planned");
    let removed: Vec<&str> = plan
        .removals
        .iter()
        .map(|r| r.resource.key.as_str())
        .collect();
    assert_eq!(
        removed,
        vec!["/sel/never"],
        "a task excluded by `when` is an orphan; one excluded by --select is not"
    );
}

#[tokio::test]
async fn a_task_kind_the_platform_does_not_support_is_a_plan_error_for_its_module() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: m\ntasks:\n  - test.linux: { path: /plat/linux-only, run: plat1 }\n  - test.file: { path: /plat/any, run: plat1 }\n",
            ),
            module(
                "name: guarded\ntasks:\n  - test.linux: { path: /plat/guarded, run: plat1 }\n    when: 'host.labels.os == \"linux\"'\n  - test.file: { path: /plat/other, run: plat1 }\n",
            ),
        ],
    );
    let sys = FakeSystem::new().with_platform(Platform::MacOs);
    let inputs = inputs(rev, &sys);
    let plan = plan(&inputs).await;
    let m = indeterminate(&plan, "m");
    assert!(m.plan_error);
    assert!(m.reason.contains("test.linux"), "{}", m.reason);
    assert!(m.reason.contains("macos"), "{}", m.reason);
    assert_eq!(
        module_names(&plan),
        vec!["guarded"],
        "a `when` can exclude the platform"
    );
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(!outcome.succeeded);
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Failed);
    assert!(!sys.exists("/plat/linux-only") && !sys.exists("/plat/any"));
    assert!(sys.exists("/plat/other"));
}

#[tokio::test]
async fn task_blocked_steps_are_planned_again_once_earlier_steps_changed_something() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            r#"
name: m
tasks:
  - test.file: { path: /tool/bin, content: bin, run: blk1 }
  - test.file: { path: /tool/conf, content: conf, run: blk1, requires: /tool/bin }
    notify: [restart]
handlers:
  restart: { systemd.restart: { name: tool, run: blk1 } }
"#,
        )],
    );
    let sys = FakeSystem::new();
    let inputs = inputs(rev, &sys);
    let plan = plan(&inputs).await;
    assert!(
        matches!(&step(&plan, "m", "/tool/conf").planned, Planned::Blocked { reason } if reason.contains("/tool/bin"))
    );
    assert!(
        plan.modules[0].handlers.is_empty(),
        "nothing changed at plan time notifies the handler"
    );
    let conf_warning = plan
        .warnings
        .iter()
        .find(|w| w.contains("/tool/conf"))
        .unwrap();
    assert!(conf_warning.contains("planned again"), "{conf_warning}");

    let (outcome, events) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(
        outcome.changes, 2,
        "the blocked step was planned again and applied"
    );
    assert_eq!(sys.file_string("/tool/conf").as_deref(), Some("conf"));
    assert!(
        calls("blk1").contains(&"handler m:systemd.restart tool".to_string()),
        "{:?}",
        calls("blk1")
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ApplyEvent::HandlerRun { handler, .. } if handler == "restart"))
    );
    let state = inputs.state.load().unwrap();
    assert!(
        state
            .get(&ResourceKey::new("m", "test.file", "/tool/conf"))
            .is_some()
    );
}

#[tokio::test]
async fn blocked_steps_stay_blocked_when_nothing_changed_before_them() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m
tasks:
  - test.file: { path: /tool2/conf, run: blk2, requires: /tool2/bin }
",
        )],
    );
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let (outcome, _) = apply(&inputs_with_store(rev, &sys, store.clone())).await;
    assert!(
        !outcome.succeeded,
        "a step left blocked needs attention: {outcome:#?}"
    );
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Blocked);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("/tool2/bin is not installed"),
        "{outcome:#?}"
    );
    assert!(outcome.rolled_back_modules.is_empty());
    let state = store.load().unwrap();
    assert_eq!(state.applied_revision, None);
    assert_eq!(
        state.runs.last().unwrap().outcome,
        crate::state::RunOutcome::NeedsAttention
    );
    assert_eq!(outcome.changes, 0);
    assert!(!sys.exists("/tool2/conf"));
    assert_eq!(
        calls("blk2")
            .iter()
            .filter(|c| c.starts_with("plan"))
            .count(),
        2,
        "plan, then the apply's stale check; no re-plan"
    );
}

#[tokio::test]
async fn a_blocked_step_that_plans_a_deletion_once_unblocked_stays_blocked() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /tool3/bin, content: bin, run: blk3 }\n  - test.file: { path: /tool3/victim, run: blk3, requires: /tool3/bin, delete: true }\n",
        )],
    );
    let sys = FakeSystem::new().with_file("/tool3/victim", "precious", FilePerms::mode(0o644));
    let inputs = inputs(rev, &sys);
    let plan = plan(&inputs).await;
    assert!(!plan.is_destructive(), "the reviewed plan deletes nothing");
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(!outcome.succeeded, "{outcome:#?}");
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Blocked);
    assert!(
        outcome.error.as_deref().unwrap_or("").contains("deletion"),
        "{outcome:#?}"
    );
    assert_eq!(
        sys.file_string("/tool3/victim").as_deref(),
        Some("precious")
    );
    assert_eq!(outcome.changes, 1, "only the reviewed change ran");
}

#[tokio::test]
async fn after_is_rejected_rather_than_silently_ignored() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /af/a }\n    id: a\n  - test.file: { path: /af/b }\n    after: [a]\n",
        )],
    );
    let report = validate(&rev).unwrap();
    assert!(
        report.errors.iter().any(|e| e.message.contains("`after:`")),
        "{report}"
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    assert!(indeterminate(&plan, "m").reason.contains("`after:`"));
}

#[tokio::test]
async fn always_running_steps_are_flagged() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.always: { path: /always/x, run: alw1 } } ]\n",
        )],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    assert!(step(&plan, "m", "/always/x").always_runs);
    assert!(
        plan.warnings.iter().any(|w| w.contains("always runs")),
        "{:?}",
        plan.warnings
    );
}

// ---------------------------------------------------------------------------
// Removals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn orphans_become_removals_with_policy_caution_and_origin() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module("name: m\n"),
            module("name: keep\ntasks: [ { test.file: { path: /rm/keep } } ]\n"),
        ],
    );
    let sys = FakeSystem::new()
        .with_file("/rm/cleanup", "x", FilePerms::mode(0o644))
        .with_file("/rm/leave", "x", FilePerms::mode(0o644))
        .with_file("/rm/caution", "x", FilePerms::mode(0o644))
        .with_file("/rm/keep", "", FilePerms::mode(0o644));
    let mut state = State::default();
    state.record(record_for(
        "m",
        "test.file",
        "/rm/cleanup",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    state.record(record_for(
        "m",
        "test.file",
        "/rm/leave",
        RemovalPolicy::Leave,
        "r0",
    ));
    state.record(record_for(
        "m",
        "test.caution",
        "/rm/caution",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    state.record(record_for(
        "gone",
        "no.such_kind",
        "/rm/unknown",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    state.record(record_for(
        "keep",
        "test.file",
        "/rm/keep",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    let store = Arc::new(MemoryStateStore::with(state));
    let inputs = inputs_with_store(rev, &sys, store.clone());

    let plan = plan(&inputs).await;
    let by_key: HashMap<&str, &crate::plan::Removal> = plan
        .removals
        .iter()
        .map(|r| (r.resource.key.as_str(), r))
        .collect();
    assert_eq!(by_key.len(), 4, "{:#?}", plan.removals);
    assert_eq!(by_key["/rm/cleanup"].policy, RemovalPolicy::Cleanup);
    assert_eq!(by_key["/rm/cleanup"].summary, "delete /rm/cleanup");
    assert!(!by_key["/rm/cleanup"].caution);
    assert_eq!(by_key["/rm/leave"].policy, RemovalPolicy::Leave);
    assert!(by_key["/rm/leave"].summary.contains("left in place"));
    assert!(by_key["/rm/caution"].caution);
    assert!(
        by_key["/rm/unknown"]
            .summary
            .contains("no longer available")
    );
    assert!(
        plan.removals
            .iter()
            .all(|r| r.introduced_in == Some(RevisionId::new("r0")))
    );
    assert_eq!(
        plan.removals[0].resource.module, "gone",
        "modules the revision no longer has go first"
    );
    assert_eq!(
        plan.warnings
            .iter()
            .filter(|w| w.starts_with("removal:"))
            .count(),
        4
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("/rm/caution") && w.contains("caution"))
    );
    assert!(plan.is_destructive());

    let (outcome, events) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(outcome.removals, 4);
    let removed: Vec<String> = calls("removals")
        .into_iter()
        .filter(|c| c.contains("/rm/"))
        .collect();
    assert_eq!(
        removed,
        vec![
            "remove m/test.caution[/rm/caution]",
            "remove m/test.file[/rm/cleanup]"
        ]
    );
    assert!(!sys.exists("/rm/cleanup") && !sys.exists("/rm/caution"));
    assert!(sys.exists("/rm/leave"), "leave never touches the host");
    let state = store.load().unwrap();
    let keys: Vec<&String> = state.resources.keys().collect();
    assert_eq!(keys, vec!["keep/test.file[/rm/keep]"]);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ApplyEvent::RemovalRun { .. }))
            .count(),
        4
    );
}

#[rstest]
#[case::kind_default(None, None, "test.file", RemovalPolicy::Cleanup)]
#[case::module_policy_beats_kind(Some("leave"), None, "test.file", RemovalPolicy::Leave)]
#[case::task_beats_module(Some("leave"), Some("cleanup"), "test.file", RemovalPolicy::Cleanup)]
#[case::kind_default_leave(None, None, "test.kept", RemovalPolicy::Leave)]
#[case::module_cleanup_beats_kind_leave(Some("cleanup"), None, "test.kept", RemovalPolicy::Cleanup)]
#[tokio::test]
async fn removal_policy_is_task_then_module_then_kind(
    #[case] module_policy: Option<&str>,
    #[case] task_policy: Option<&str>,
    #[case] kind: &str,
    #[case] expected: RemovalPolicy,
) {
    let module_removal = module_policy
        .map(|p| format!("removal: {{ policy: {p} }}\n"))
        .unwrap_or_default();
    let task_removal = task_policy
        .map(|p| format!("    removal: {p}\n"))
        .unwrap_or_default();
    let yaml =
        format!("name: m\n{module_removal}tasks:\n  - {kind}: {{ path: /pol/x }}\n{task_removal}");
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let inputs = inputs_with_store(
        revision("r1", FLEET, vec![module(&yaml)]),
        &sys,
        store.clone(),
    );
    let plan = plan(&inputs).await;
    assert_eq!(step(&plan, "m", "/pol/x").removal, expected);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);

    // Dropping the task removes or releases the file according to the policy.
    let inputs = inputs_with_store(
        revision("r2", FLEET, vec![module("name: m\n")]),
        &sys,
        store,
    );
    let plan = self::plan(&inputs).await;
    assert_eq!(plan.removals[0].policy, expected);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);
    assert_eq!(sys.exists("/pol/x"), expected == RemovalPolicy::Leave);
}

// ---------------------------------------------------------------------------
// Moved and re-keyed tasks are taken over, not removed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_task_moved_between_modules_keeps_its_resource_and_record() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let r1 = revision(
        "r1",
        FLEET,
        vec![
            module("name: a\ntasks: [ { test.file: { path: /mv/x, content: same, run: mv1 } } ]\n"),
            module("name: b\n"),
        ],
    );
    let (outcome, _) = apply(&inputs_with_store(r1, &sys, store.clone())).await;
    assert!(outcome.succeeded);

    let r2 = revision(
        "r2",
        FLEET,
        vec![
            module("name: a\n"),
            module("name: b\ntasks: [ { test.file: { path: /mv/x, content: same, run: mv1 } } ]\n"),
        ],
    );
    let inputs = inputs_with_store(r2, &sys, store.clone());
    let plan = plan(&inputs).await;
    assert!(plan.removals.is_empty(), "{:#?}", plan.removals);
    assert_eq!(plan.rehomed.len(), 1);
    assert_eq!(plan.rehomed[0].kind, crate::plan::RehomeKind::Moved);
    assert_eq!(plan.rehomed[0].to.to_string(), "b/test.file[/mv/x]");
    assert_eq!(step(&plan, "b", "/mv/x").planned, Planned::NoOp);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);
    assert_eq!(sys.file_string("/mv/x").as_deref(), Some("same"));
    let state = store.load().unwrap();
    let keys: Vec<&String> = state.resources.keys().collect();
    assert_eq!(keys, vec!["b/test.file[/mv/x]"]);
    assert_eq!(
        state.resources["b/test.file[/mv/x]"].introduced_in,
        RevisionId::new("r1"),
        "the moved record keeps its origin"
    );
    assert!(!calls("removals").iter().any(|c| c.contains("/mv/x")));
}

#[tokio::test]
async fn a_task_changing_kind_for_the_same_path_is_not_deleted() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let r1 = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /mv/y, content: one, run: mv2 } } ]\n",
        )],
    );
    let (outcome, _) = apply(&inputs_with_store(r1, &sys, store.clone())).await;
    assert!(outcome.succeeded);

    // As file.copy → file.template of the same destination.
    let r2 = revision(
        "r2",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.locked: { path: /mv/y, content: two, run: mv2 } } ]\n",
        )],
    );
    let inputs = inputs_with_store(r2, &sys, store.clone());
    let plan = plan(&inputs).await;
    assert!(plan.removals.is_empty(), "{:#?}", plan.removals);
    assert_eq!(plan.rehomed[0].kind, crate::plan::RehomeKind::Superseded);
    assert!(
        plan.rehomed[0].summary.contains("path /mv/y"),
        "{}",
        plan.rehomed[0].summary
    );
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(sys.file_string("/mv/y").as_deref(), Some("two"));
    let state = store.load().unwrap();
    let keys: Vec<&String> = state.resources.keys().collect();
    assert_eq!(keys, vec!["m/test.locked[/mv/y]"]);
    assert!(!calls("removals").iter().any(|c| c.contains("/mv/y")));
}

#[rstest]
#[case::without_id(None, true)]
#[case::with_id(Some("install-tool"), false)]
#[tokio::test]
async fn bumping_a_var_inside_a_command_only_undoes_it_when_the_task_has_no_id(
    #[case] id: Option<&str>,
    #[case] orphaned: bool,
) {
    let id_line = id.map(|i| format!("    id: {i}\n")).unwrap_or_default();
    let yaml = |version: &str| {
        format!(
            "name: m\nvars: {{ v: '{version}' }}\ntasks:\n  - command.run: {{ run: [install, 'tool-{{{{ vars.v }}}}'], remove: [uninstall, tool], log: cmd-{} }}\n{id_line}",
            id.unwrap_or("none")
        )
    };
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let r1 = revision("r1", FLEET, vec![module(&yaml("1.0"))]);
    let report = validate(&r1).unwrap();
    assert_eq!(
        report.errors.iter().any(|e| e.message.contains("`id`")),
        orphaned,
        "{report}"
    );
    let (outcome, _) = apply(&inputs_with_store(r1, &sys, store.clone())).await;
    assert!(outcome.succeeded);

    let r2 = revision("r2", FLEET, vec![module(&yaml("1.1"))]);
    let plan = plan(&inputs_with_store(r2, &sys, store)).await;
    assert_eq!(!plan.removals.is_empty(), orphaned, "{:#?}", plan.removals);
    assert_eq!(
        plan.warnings.iter().any(|w| w.contains("`id`")),
        orphaned,
        "{:?}",
        plan.warnings
    );
    assert_eq!(plan.changes(), 1);
}

#[rstest]
#[case::templated_string("'install tool-{{ vars.v }}'", None, true)]
#[case::templated_argv("[install, 'tool-{{ vars.v }}']", None, true)]
#[case::templated_with_id("'install tool-{{ vars.v }}'", Some("install-tool"), false)]
#[case::literal("[install, tool]", None, false)]
fn templated_commands_without_an_id_are_flagged(
    #[case] run: &str,
    #[case] id: Option<&str>,
    #[case] flagged: bool,
) {
    let id_line = id.map(|i| format!("    id: {i}\n")).unwrap_or_default();
    let yaml = format!(
        "name: m\nvars: {{ v: '1.0' }}\ntasks:\n  - command.run: {{ run: {run}, unless: [false] }}\n{id_line}"
    );
    let report = validate(&revision("r1", FLEET, vec![module(&yaml)])).unwrap();
    assert!(report.errors.is_empty(), "{report}");
    assert_eq!(
        report.warnings.iter().any(|w| w.message.contains("`id`")),
        flagged,
        "{report}"
    );
}

#[tokio::test]
async fn state_persists_across_runs_so_removed_modules_become_orphans() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let r1 = revision(
        "r1",
        FLEET,
        vec![
            module("name: a\ntasks: [ { test.file: { path: /runs/a, run: runs1 } } ]\n"),
            module("name: b\ntasks: [ { test.file: { path: /runs/b, run: runs1 } } ]\n"),
        ],
    );
    let (outcome, _) = apply(&inputs_with_store(r1, &sys, store.clone())).await;
    assert!(outcome.succeeded);
    assert_eq!(store.load().unwrap().resources.len(), 2);
    assert_eq!(
        store.load().unwrap().applied_revision,
        Some(RevisionId::new("r1"))
    );

    let r2 = revision(
        "r2",
        FLEET,
        vec![module(
            "name: a\ntasks: [ { test.file: { path: /runs/a, run: runs1 } } ]\n",
        )],
    );
    let inputs = inputs_with_store(r2, &sys, store.clone());
    let plan = plan(&inputs).await;
    assert_eq!(plan.removals.len(), 1);
    assert_eq!(
        plan.removals[0].resource.to_string(),
        "b/test.file[/runs/b]"
    );
    assert_eq!(step(&plan, "a", "/runs/a").planned, Planned::NoOp);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);
    assert!(!sys.exists("/runs/b"));
    let state = store.load().unwrap();
    assert_eq!(state.resources.len(), 1);
    assert_eq!(state.runs.len(), 2);
    assert_eq!(state.applied_revision, Some(RevisionId::new("r2")));
}

/// A store that keeps every snapshot it was asked to save, optionally
/// failing every save.
#[derive(Default)]
struct RecordingStore {
    snapshots: Mutex<Vec<State>>,
    fail: bool,
}

impl crate::state::StateStore for RecordingStore {
    fn load(&self) -> crate::Result<State> {
        Ok(self
            .snapshots
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default())
    }
    fn save(&self, state: &State) -> crate::Result<()> {
        if self.fail {
            return Err(crate::error::system(
                "the disk is full",
                &["Free some space."],
            ));
        }
        self.snapshots.lock().unwrap().push(state.clone());
        Ok(())
    }
}

fn three_step_inputs(sys: &FakeSystem, store: Arc<RecordingStore>) -> Inputs {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m
tasks:
  - test.file: { path: /sv/1 }
  - test.file: { path: /sv/2 }
  - test.file: { path: /sv/3 }
",
        )],
    );
    Inputs::new(
        rev,
        HostIdentity {
            id: "h1".into(),
            labels: Labels::new(),
        },
        Arc::new(sys.clone()),
        Arc::new(SecretStore::new()),
        store,
    )
    .with_frozen_facts(default_facts())
}

#[tokio::test]
async fn state_is_saved_after_every_step() {
    let store = Arc::new(RecordingStore::default());
    let (outcome, _) = apply(&three_step_inputs(&FakeSystem::new(), store.clone())).await;
    assert!(outcome.succeeded);
    let counts: Vec<usize> = store
        .snapshots
        .lock()
        .unwrap()
        .iter()
        .map(|s| s.resources.len())
        .collect();
    assert!(counts.starts_with(&[1, 2, 3]), "{counts:?}");
}

#[tokio::test]
async fn a_failed_state_save_is_reported_without_failing_the_apply() {
    let sys = FakeSystem::new();
    let store = Arc::new(RecordingStore {
        fail: true,
        ..Default::default()
    });
    let (outcome, _) = apply(&three_step_inputs(&sys, store)).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(outcome.changes, 3);
    let error = outcome.error.unwrap_or_default();
    assert!(
        error.contains("could not be saved") && error.contains("disk is full"),
        "{error}"
    );
    assert_eq!(
        error.matches("could not be saved").count(),
        1,
        "reported once: {error}"
    );
    assert!(sys.exists("/sv/3"));
}

#[tokio::test]
async fn noop_steps_refresh_their_records_and_keep_their_origin() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let yaml = "name: m\ntasks: [ { test.file: { path: /noop/a, content: same, run: noop1 } } ]\n";
    let (outcome, _) = apply(&inputs_with_store(
        revision("r1", FLEET, vec![module(yaml)]),
        &sys,
        store.clone(),
    ))
    .await;
    assert_eq!(outcome.changes, 1);
    let first = store
        .load()
        .unwrap()
        .get(&ResourceKey::new("m", "test.file", "/noop/a"))
        .cloned()
        .unwrap();
    assert_eq!(first.introduced_in, RevisionId::new("r1"));
    assert_eq!(
        first.shadow.as_ref().map(|s| s.get().as_str()),
        Some("same")
    );

    let inputs = inputs_with_store(
        revision("r2", FLEET, vec![module(yaml)]),
        &sys,
        store.clone(),
    );
    let plan = plan(&inputs).await;
    assert_eq!(step(&plan, "m", "/noop/a").planned, Planned::NoOp);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);
    assert_eq!(outcome.changes, 0);
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Unchanged);
    let second = store
        .load()
        .unwrap()
        .get(&ResourceKey::new("m", "test.file", "/noop/a"))
        .cloned()
        .unwrap();
    assert_eq!(second.revision, RevisionId::new("r2"));
    assert_eq!(second.introduced_in, RevisionId::new("r1"));
    assert_eq!(second.shadow, first.shadow);
    assert_eq!(second.artefacts, first.artefacts);
    assert!(
        !calls("noop1").iter().any(|c| c == "apply:start m//noop/a")
            || calls("noop1")
                .iter()
                .filter(|c| *c == "apply:start m//noop/a")
                .count()
                == 1
    );
}

// ---------------------------------------------------------------------------
// Apply: stale plans, retries, rollback, halt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stale_plans_are_refused_with_both_digests() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /stale/a, content: new, run: stale1 } } ]\n",
        )],
    );
    let sys = FakeSystem::new();
    let inputs = inputs(rev, &sys);
    let old = plan(&inputs).await;
    // Someone else wrote the file in between: the fresh plan is a NoOp.
    sys.write_atomic(Path::new("/stale/a"), b"new", &FilePerms::mode(0o644))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    let err = Engine::apply(&inputs, &old, tx)
        .await
        .expect_err("stale plan");
    assert!(err.is(crate::error::Kind::User));
    let msg = err.message();
    assert!(msg.contains("stale"), "{msg}");
    assert!(msg.contains(old.digest.short()), "{msg}");
    let stale = super::StalePlan::find(&err).expect("a typed stale-plan marker");
    assert_eq!(stale.planned, old.digest);
    assert_ne!(stale.fresh, old.digest);
    assert!(super::StalePlan::find(&crate::error::user("other", &[])).is_none());
    assert!(!calls("stale1").iter().any(|c| c.starts_with("apply")));
}

#[tokio::test]
async fn retries_recover_from_transient_failures() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /retry/a, run: retry1, fail_times: 2 }\n    retry: { limit: 3, delay: 1ms }\n",
        )],
    );
    let sys = FakeSystem::new();
    let (outcome, events) = apply(&inputs(rev, &sys)).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Applied);
    let attempts = calls("retry1")
        .iter()
        .filter(|c| c.starts_with("apply:start"))
        .count();
    assert_eq!(attempts, 3);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ApplyEvent::StepFailed { .. }))
            .count(),
        0
    );
    assert!(sys.exists("/retry/a"));
}

#[tokio::test]
async fn failures_beyond_the_retry_limit_fail_the_module() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /retry/b, run: retry2, fail_times: 5 }\n    retry: { limit: 1, delay: 1ms }\n",
        )],
    );
    let (outcome, events) = apply(&inputs(rev, &FakeSystem::new())).await;
    assert!(!outcome.succeeded);
    assert_eq!(outcome.failed_module.as_deref(), Some("m"));
    assert_eq!(
        calls("retry2")
            .iter()
            .filter(|c| c.starts_with("apply:start"))
            .count(),
        2
    );
    assert!(events.iter().any(|e| matches!(e, ApplyEvent::StepFailed { error, .. } if error.contains("transient failure #2"))));
}

fn rollback_modules(run: &str, b_content: &str, b_fail: u32) -> Vec<LoadedModule> {
    vec![
        module(&format!(
            "name: a\ntasks: [ {{ test.file: {{ path: /rb/{run}/a, content: a, run: {run} }} }} ]\n"
        )),
        module(&format!(
            "name: b\nneeds: [a]\ntasks: [ {{ test.file: {{ path: /rb/{run}/b, content: {b_content}, run: {run}, fail_times: {b_fail} }} }} ]\n"
        )),
        module(&format!(
            "name: c\nneeds: [b]\ntasks: [ {{ test.file: {{ path: /rb/{run}/c, content: c, run: {run} }} }} ]\n"
        )),
        module(&format!(
            "name: d\ntasks: [ {{ test.file: {{ path: /rb/{run}/d, content: d, run: {run}, wait_for: {run}-release }} }} ]\n"
        )),
    ]
}

#[tokio::test]
async fn a_failing_module_is_rolled_back_from_the_stable_revision_and_the_run_halts() {
    let sys = FakeSystem::new();
    let r1 = revision("r1", FLEET, rollback_modules("rb1", "new", 99));
    let stable = revision("r0", FLEET, rollback_modules("rb1", "stable", 0));
    let mut state = State::default();
    state.record(record_for(
        "gone",
        "test.file",
        "/rb/rb1/orphan",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    let store = Arc::new(MemoryStateStore::with(state));
    let inputs = inputs_with_store(r1, &sys, store.clone())
        .with_stable_revision(Some(stable))
        .with_parallelism(1);
    // d waits for a barrier that b's failure handling releases, so d is still
    // "not started" at the moment b fails: the halt must not start it.
    let plan = plan(&inputs).await;
    assert!(plan.removals.len() == 1);
    let inputs_for_release = inputs.parallelism;
    assert_eq!(inputs_for_release, 1);
    signal("rb1-release");
    let (outcome, events) = apply_plan(&inputs, &plan).await;

    assert!(!outcome.succeeded);
    assert_eq!(outcome.failed_module.as_deref(), Some("b"));
    assert_eq!(outcome.rolled_back_modules, vec!["b"]);
    assert_eq!(status_of(&outcome, "a"), ModuleStatus::Applied);
    assert_eq!(status_of(&outcome, "b"), ModuleStatus::RolledBack);
    assert_eq!(status_of(&outcome, "c"), ModuleStatus::NotStarted);
    assert!(
        outcome.not_started.contains(&"c".to_string()),
        "{outcome:#?}"
    );
    assert_eq!(sys.file_string("/rb/rb1/a").as_deref(), Some("a"));
    assert_eq!(
        sys.file_string("/rb/rb1/b").as_deref(),
        Some("stable"),
        "b was re-applied from r0"
    );
    assert!(!sys.exists("/rb/rb1/c"));
    assert_eq!(outcome.removals, 0, "removals are skipped on a halted run");
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("transient failure"),
        "{outcome:#?}"
    );

    let state = store.load().unwrap();
    let b = state
        .get(&ResourceKey::new("b", "test.file", "/rb/rb1/b"))
        .unwrap();
    assert_eq!(b.revision, RevisionId::new("r0"));
    assert!(
        state
            .get(&ResourceKey::new("gone", "test.file", "/rb/rb1/orphan"))
            .is_some()
    );
    assert_eq!(state.applied_revision, None);
    assert_eq!(
        state.runs.last().unwrap().outcome,
        crate::state::RunOutcome::RolledBack
    );
    assert_eq!(state.runs.last().unwrap().rolled_back_modules, vec!["b"]);
    assert!(events.iter().any(|e| matches!(e, ApplyEvent::ModuleFinished { module, status: ModuleStatus::Failed } if module == "b")));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ApplyEvent::ModuleStarted { module } if module == "c"))
    );
}

#[tokio::test]
async fn rollback_reapplies_the_stable_module_and_removes_what_the_failed_revision_added() {
    let sys = FakeSystem::new();
    let stable = revision(
        "r0",
        FLEET,
        vec![module(
            "name: b\ntasks: [ { test.file: { path: /rb2/b, content: stable, run: rb5 } } ]\n",
        )],
    );
    let r1 = revision(
        "r1",
        FLEET,
        vec![module(
            "name: b\ntasks:\n  - test.file: { path: /rb2/extra, content: x, run: rb5 }\n  - test.file: { path: /rb2/b, content: new, run: rb5, fail_times: 99 }\n",
        )],
    );
    let store = Arc::new(MemoryStateStore::default());
    let inputs = inputs_with_store(r1, &sys, store.clone()).with_stable_revision(Some(stable));
    let (outcome, _) = apply(&inputs).await;
    assert_eq!(outcome.rolled_back_modules, vec!["b"], "{outcome:#?}");
    assert_eq!(sys.file_string("/rb2/b").as_deref(), Some("stable"));
    assert!(
        !sys.exists("/rb2/extra"),
        "the resource only the failed revision wanted is removed by the rollback"
    );
    let state = store.load().unwrap();
    let keys: Vec<&String> = state.resources.keys().collect();
    assert_eq!(keys, vec!["b/test.file[/rb2/b]"]);
    assert_eq!(state.module_revisions["b"], RevisionId::new("r0"));
}

#[tokio::test]
async fn rollback_follows_the_stable_revisions_selection() {
    let sys = FakeSystem::new();
    // In the stable revision the module did not apply to this host at all.
    let stable = revision(
        "r0",
        FLEET,
        vec![module(
            "name: b\nwhen: 'host.labels.role == \"other\"'\ntasks: [ { test.file: { path: /rb3/stable-only, content: s, run: rb6 } } ]\n",
        )],
    );
    let r1 = revision(
        "r1",
        FLEET,
        vec![module(
            "name: b\ntasks:\n  - test.file: { path: /rb3/new, content: n, run: rb6 }\n  - test.file: { path: /rb3/boom, run: rb6, fail_times: 99 }\n",
        )],
    );
    let store = Arc::new(MemoryStateStore::default());
    let inputs = inputs_with_store(r1, &sys, store.clone()).with_stable_revision(Some(stable));
    let (outcome, _) = apply(&inputs).await;
    assert_eq!(
        status_of(&outcome, "b"),
        ModuleStatus::RolledBack,
        "{outcome:#?}"
    );
    assert!(
        !sys.exists("/rb3/stable-only"),
        "a module the stable revision does not select is not installed by its rollback"
    );
    assert!(
        !sys.exists("/rb3/new"),
        "what the failed revision added is removed"
    );
    assert!(store.load().unwrap().resources.is_empty());
}

#[tokio::test]
async fn a_deferred_module_whose_planning_fails_is_not_rolled_back() {
    let sys = FakeSystem::new();
    let provider = "name: p\nfacts: { provides: [test.counter] }\ntasks: [ { test.file: { path: /facts/counter, content: \"7\", run: rb7 } } ]\n";
    let stable = revision(
        "r0",
        FLEET,
        vec![
            module(provider),
            module(
                "name: q\nneeds: [p]\ntasks: [ { test.file: { path: /rb4/q, content: stable, run: rb7 } } ]\n",
            ),
        ],
    );
    let r1 = revision(
        "r1",
        FLEET,
        vec![
            module(provider),
            module(
                "name: q\nneeds: [p]\ntasks: [ { test.render: { src: missing.j2, dest: /rb4/q } } ]\n",
            ),
            module("name: z\nneeds: [q]\ntasks: [ { test.file: { path: /rb4/z, run: rb7 } } ]\n"),
        ],
    );
    let mut inputs = inputs(r1, &sys).with_stable_revision(Some(stable));
    inputs.facts_frozen = false;
    inputs.facts = Facts::new();
    let plan = plan(&inputs).await;
    assert_eq!(plan.deferred.len(), 2, "{plan:#?}");
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(!outcome.succeeded);
    assert_eq!(status_of(&outcome, "q"), ModuleStatus::Failed);
    assert!(outcome.rolled_back_modules.is_empty(), "{outcome:#?}");
    assert!(!sys.exists("/rb4/q"), "the stable q never ran");
    assert_eq!(status_of(&outcome, "z"), ModuleStatus::NotStarted);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("planning failed"),
        "{outcome:#?}"
    );
}

#[tokio::test]
async fn without_a_stable_revision_a_failing_module_stays_failed() {
    let sys = FakeSystem::new();
    signal("rb2-release");
    let r1 = revision("r1", FLEET, rollback_modules("rb2", "new", 99));
    let (outcome, _) = apply(&inputs(r1, &sys)).await;
    assert!(!outcome.succeeded);
    assert_eq!(status_of(&outcome, "b"), ModuleStatus::Failed);
    assert!(outcome.rolled_back_modules.is_empty());
    assert_eq!(status_of(&outcome, "c"), ModuleStatus::NotStarted);
    assert!(!sys.exists("/rb/rb2/b"));
}

#[tokio::test]
async fn rekeying_a_line_edit_from_its_line_to_its_regex_does_not_revert_it() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let r1 = revision(
        "r1",
        FLEET,
        vec![module(
            "name: ssh\ntasks: [ { test.line: { path: /lines/sshd, line: 'PasswordAuthentication no' } } ]\n",
        )],
    );
    let (outcome, _) = apply(&inputs_with_store(r1, &sys, store.clone())).await;
    assert!(outcome.succeeded);
    let r2 = revision(
        "r2",
        FLEET,
        vec![module(
            "name: ssh\ntasks: [ { test.line: { path: /lines/sshd, line: 'PasswordAuthentication no', regex: '^#?PasswordAuthentication' } } ]\n",
        )],
    );
    let inputs = inputs_with_store(r2, &sys, store.clone());
    let plan = plan(&inputs).await;
    assert!(plan.removals.is_empty(), "{:#?}", plan.removals);
    assert_eq!(plan.rehomed.len(), 1, "{plan:#?}");
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);
    assert!(!sys.exists("/lines/reverted"));
    let keys: Vec<String> = store.load().unwrap().resources.keys().cloned().collect();
    assert_eq!(
        keys,
        vec!["ssh/test.line[/lines/sshd: ^#?PasswordAuthentication]"]
    );
}

#[tokio::test]
async fn an_apply_that_changes_nothing_keeps_the_artefacts_already_recorded() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let r1 = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /own/a, content: one } } ]\n",
        )],
    );
    apply(&inputs_with_store(r1, &sys, store.clone())).await;
    let r2 = revision(
        "r2",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /own/a, content: two, unchanged: true } } ]\n",
        )],
    );
    let (outcome, _) = apply(&inputs_with_store(r2, &sys, store.clone())).await;
    assert!(outcome.succeeded);
    let record = store
        .load()
        .unwrap()
        .get(&ResourceKey::new("m", "test.file", "/own/a"))
        .cloned()
        .unwrap();
    assert_eq!(
        record.artefacts,
        vec![Artefact::File {
            path: "/own/a".into()
        }]
    );
    assert_eq!(
        record.shadow.as_ref().map(|s| s.get().as_str()),
        Some("one")
    );
}

#[tokio::test]
async fn health_check_fields_marked_secret_keep_their_template_for_the_check() {
    let rev = revision(
        "r1",
        "version: 2\nsecrets: { api_token: { env: API_TOKEN } }\n",
        vec![module(
            "name: m\nhealth:\n  - name: api\n    test.secret_header: { token: 'Bearer {{ secrets.api_token }}' }\n",
        )],
    );
    let mut inputs = inputs(rev, &FakeSystem::new());
    let mut secrets = SecretStore::new();
    secrets.insert(Secret::new(
        "api_token",
        "hunter2-health".into(),
        &InstallKey::from_bytes([3; 32]),
    ));
    inputs.secrets = Arc::new(secrets);
    let report = Engine::health(&inputs, crate::health::HealthScope::Manual, None)
        .await
        .unwrap();
    assert!(report.results[0].status.is_pass(), "{report:#?}");
}

#[tokio::test]
async fn a_task_that_writes_while_planning_fails_loudly_and_changes_nothing() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module("name: bad\ntasks: [ { test.impure: { path: /etc/impure } } ]\n"),
            module(
                "name: scratch\ntasks: [ { test.impure: { path: /tmp/buckle-validate/conf } } ]\n",
            ),
            module("name: state\ntasks: [ { test.impure: { path: /var/lib/buckle/tmp/x } } ]\n"),
        ],
    );
    let sys = FakeSystem::new();
    let inputs = inputs(rev, &sys).with_scratch_dir("/var/lib/buckle/tmp");
    let plan = plan(&inputs).await;
    let bad = indeterminate(&plan, "bad");
    assert!(bad.plan_error);
    assert!(
        bad.reason.contains("must not change the host"),
        "{}",
        bad.reason
    );
    assert!(!sys.exists("/etc/impure"));
    assert_eq!(
        module_names(&plan),
        vec!["scratch", "state"],
        "scratch writes are allowed"
    );
    assert!(sys.exists("/tmp/buckle-validate/conf"));
}

// ---------------------------------------------------------------------------
// Drift after a failed run
// ---------------------------------------------------------------------------

/// `run` names the call log, so tests that run in parallel do not count each
/// other's applies.
fn drift_modules(rev: &str, run: &str) -> Vec<LoadedModule> {
    let content = if rev == "r1" { "one" } else { "two" };
    let fail = if rev == "r1" { 0 } else { 99 };
    let mut modules = vec![
        module(&format!(
            "name: a\ntasks: [ {{ test.file: {{ path: /dr/a, content: {content}, run: {run} }} }} ]\n"
        )),
        module(&format!(
            "name: b\ntasks: [ {{ test.file: {{ path: /dr/b, content: {content}, run: {run}, fail_times: {fail} }} }} ]\n"
        )),
        module(&format!(
            "name: c\nneeds: [b]\ntasks: [ {{ test.file: {{ path: /dr/c, content: c, run: {run} }} }} ]\n"
        )),
    ];
    if rev == "r2" {
        modules.push(module(&format!(
            "name: aa\ntasks: [ {{ test.file: {{ path: /dr/aa, content: new, run: {run} }} }} ]\n"
        )));
    }
    modules
}

#[tokio::test]
async fn drift_after_a_failed_run_neither_undoes_succeeded_modules_nor_retries_the_failed_one() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let r1 = revision("r1", FLEET, drift_modules("r1", "dr-failed"));
    let (outcome, _) =
        apply(&inputs_with_store(r1.clone(), &sys, store.clone()).with_parallelism(1)).await;
    assert!(outcome.succeeded, "{outcome:#?}");

    let r2 = revision("r2", FLEET, drift_modules("r2", "dr-failed"));
    let (outcome, _) = apply(&inputs_with_store(r2, &sys, store.clone()).with_parallelism(1)).await;
    assert!(!outcome.succeeded);
    assert_eq!(status_of(&outcome, "a"), ModuleStatus::Applied);
    assert_eq!(status_of(&outcome, "aa"), ModuleStatus::Applied);
    assert_eq!(status_of(&outcome, "b"), ModuleStatus::Failed);
    let state = store.load().unwrap();
    assert_eq!(state.applied_revision, Some(RevisionId::new("r1")));
    assert_eq!(state.module_revisions["a"], RevisionId::new("r2"));
    assert_eq!(state.module_revisions["b"], RevisionId::new("r1"));
    assert_eq!(state.failed_modules["b"], RevisionId::new("r2"));

    // Without drift mode, re-planning the applied revision would undo `a`
    // and remove what the newer revision added: the bug drift mode avoids.
    let naive = plan(&inputs_with_store(r1.clone(), &sys, store.clone())).await;
    assert!(!naive.removals.is_empty());

    // The agent drifts against its applied revision.
    let inputs = inputs_with_store(r1, &sys, store.clone())
        .with_drift(true)
        .with_removals(false)
        .with_parallelism(1);
    let (plan, _) = Engine::drift(&inputs).await.unwrap();
    assert!(plan.removals.is_empty(), "{:#?}", plan.removals);
    let held: Vec<&str> = plan.held.iter().map(|h| h.module.as_str()).collect();
    assert_eq!(held, vec!["a", "b"]);
    assert!(plan.held[1].reason.contains("failed"), "{:#?}", plan.held);
    assert_eq!(module_names(&plan), vec!["c"]);
    assert!(plan.is_empty(), "{plan:#?}");
    let before = calls("dr-failed")
        .iter()
        .filter(|c| c.starts_with("apply:start"))
        .count();
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(
        calls("dr-failed")
            .iter()
            .filter(|c| c.starts_with("apply:start"))
            .count(),
        before,
        "the failed module is not re-applied and nothing else changes"
    );
    assert_eq!(sys.file_string("/dr/a").as_deref(), Some("two"));
    assert!(sys.exists("/dr/aa"));
    assert_eq!(store.load().unwrap().resources.len(), 4);
}

#[tokio::test]
async fn drift_corrects_modules_applied_at_its_revision() {
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let r1 = revision("r1", FLEET, drift_modules("r1", "dr-corrects"));
    let (outcome, _) = apply(&inputs_with_store(r1.clone(), &sys, store.clone())).await;
    assert!(outcome.succeeded);
    sys.write_atomic(Path::new("/dr/a"), b"hand edit", &FilePerms::mode(0o644))
        .await
        .unwrap();
    let inputs = inputs_with_store(r1, &sys, store).with_drift(true);
    let (plan, _) = Engine::drift(&inputs).await.unwrap();
    assert!(plan.held.is_empty());
    assert_eq!(plan.changes(), 1);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);
    assert_eq!(sys.file_string("/dr/a").as_deref(), Some("one"));
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

fn health_module(run: &str, pass_after: u32) -> LoadedModule {
    module(&format!(
        "name: m\ntasks: [ {{ test.file: {{ path: /health/{run}, run: {run} }} }} ]\nhealth:\n  - name: flaky\n    test.flaky: {{ run: {run}, pass_after: {pass_after} }}\n    interval: 1s\n"
    ))
}

#[tokio::test(start_paused = true)]
async fn health_checks_settle_before_declaring_a_module_healthy() {
    let rev = revision(
        "r1",
        "version: 2\nrollout: { settle: 10s }\n",
        vec![health_module("hs1", 3)],
    );
    let (outcome, events) = apply(&inputs(rev, &FakeSystem::new())).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Applied);
    assert_eq!(
        calls("hs1")
            .iter()
            .filter(|c| c.starts_with("health attempt"))
            .count(),
        3
    );
    let checked: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            ApplyEvent::HealthChecked { passed, .. } => Some(*passed),
            _ => None,
        })
        .collect();
    assert_eq!(checked, vec![false, false, true]);
    let report = outcome.health.unwrap();
    assert_eq!(report.scope, crate::health::HealthScope::Bake);
    assert!(report.healthy(&[]));
    assert_eq!(report.results.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn health_checks_failing_past_settle_make_the_module_unhealthy() {
    let rev = revision(
        "r1",
        "version: 2\nrollout: { settle: 2s }\n",
        vec![health_module("hs2", 100)],
    );
    let (outcome, _) = apply(&inputs(rev, &FakeSystem::new())).await;
    assert!(!outcome.succeeded);
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Unhealthy);
    assert_eq!(outcome.failed_module.as_deref(), Some("m"));
    assert!(
        outcome.error.as_deref().unwrap_or("").contains("flaky"),
        "{outcome:#?}"
    );
    assert_eq!(
        calls("hs2")
            .iter()
            .filter(|c| c.starts_with("health attempt"))
            .count(),
        3,
        "t=0s, 1s, 2s"
    );
    assert!(!outcome.health.unwrap().healthy(&[]));
}

#[tokio::test(start_paused = true)]
async fn health_checks_see_the_facts_their_module_just_refreshed() {
    let rev = revision(
        "r1",
        "version: 2\nrollout: { settle: 5s }\n",
        vec![module(
            r#"
name: nomad
facts: { provides: [test.counter] }
tasks: [ { test.file: { path: /facts/counter, content: eligible, run: hf1 } } ]
health:
  - name: eligible
    test.filter: { expr: 'host.facts.test.counter.value == "eligible"' }
    interval: 1s
"#,
        )],
    );
    let sys = FakeSystem::new();
    let mut inputs = inputs(rev, &sys);
    inputs.facts_frozen = false;
    inputs.facts = Facts::new();
    let (outcome, events) = apply(&inputs).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(status_of(&outcome, "nomad"), ModuleStatus::Applied);
    let checked: Vec<bool> = events
        .iter()
        .filter_map(|e| match e {
            ApplyEvent::HealthChecked { passed, .. } => Some(*passed),
            _ => None,
        })
        .collect();
    assert_eq!(
        checked,
        vec![true],
        "passes at once against the refreshed facts"
    );
    let refreshed = events
        .iter()
        .position(|e| matches!(e, ApplyEvent::FactsRefreshed { .. }))
        .unwrap();
    let checked_at = events
        .iter()
        .position(|e| matches!(e, ApplyEvent::HealthChecked { .. }))
        .unwrap();
    assert!(refreshed < checked_at);
}

#[tokio::test(start_paused = true)]
async fn failing_checks_of_an_unchanged_module_are_reported_but_do_not_gate() {
    let rev = revision(
        "r1",
        "version: 2\nrollout: { settle: 10s }\n",
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /hng/x, content: same, run: hng1 } } ]\nhealth:\n  - name: flaky\n    test.flaky: { run: hng1, pass_after: 100 }\n    interval: 1s\n",
        )],
    );
    let sys = FakeSystem::new().with_file("/hng/x", "same", FilePerms::mode(0o644));
    let stable = revision("r0", FLEET, vec![module("name: m\n")]);
    let inputs = inputs(rev, &sys).with_stable_revision(Some(stable));
    let (outcome, _) = apply(&inputs).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Unchanged);
    assert!(outcome.rolled_back_modules.is_empty());
    assert!(sys.exists("/hng/x"));
    let report = outcome.health.unwrap();
    assert!(!report.healthy(&[]), "the failing check is still reported");
    assert_eq!(
        calls("hng1")
            .iter()
            .filter(|c| c.starts_with("health attempt"))
            .count(),
        1,
        "no settle loop for a module this run did not change"
    );
}

#[tokio::test(start_paused = true)]
async fn waived_checks_do_not_fail_the_module() {
    let rev = revision(
        "r1",
        "version: 2\nrollout: { settle: 2s }\n",
        vec![health_module("hs3", 100)],
    );
    let inputs = inputs(rev, &FakeSystem::new()).with_waived_checks(vec!["m/flaky".into()]);
    let (outcome, _) = apply(&inputs).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(status_of(&outcome, "m"), ModuleStatus::Applied);
    assert_eq!(
        calls("hs3")
            .iter()
            .filter(|c| c.starts_with("health attempt"))
            .count(),
        1
    );
}

#[tokio::test]
async fn engine_health_runs_the_checks_of_selected_modules_once() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: m\ntasks: [ { test.file: { path: /eh/m } } ]\nhealth:\n  - name: present\n    test.file_exists: { path: /eh/m }\n  - test.file_exists: { path: /eh/other }\n    required: false\n",
            ),
            module(
                "name: skipped\nwhen: 'false'\nhealth: [ { test.file_exists: { path: /nope } } ]\n",
            ),
        ],
    );
    let sys = FakeSystem::new();
    let inputs = inputs(rev, &sys);
    let before = Engine::health(&inputs, crate::health::HealthScope::Manual, None)
        .await
        .unwrap();
    assert_eq!(before.scope, crate::health::HealthScope::Manual);
    assert_eq!(before.results.len(), 2, "{before:#?}");
    assert!(!before.healthy(&[]));
    assert_eq!(before.results[0].check, "present");
    assert_eq!(before.results[1].check, "test.file_exists");
    assert!(!before.results[1].required);

    let (outcome, _) = apply(&inputs).await;
    assert!(outcome.succeeded);
    let after = Engine::health(
        &inputs,
        crate::health::HealthScope::Drift,
        Some(&["m".to_string()]),
    )
    .await
    .unwrap();
    assert!(after.healthy(&[]));
    assert!(after.results[0].status.is_pass());

    let err = Engine::health(
        &inputs,
        crate::health::HealthScope::Manual,
        Some(&["ghost".to_string()]),
    )
    .await
    .unwrap_err();
    assert!(err.is(crate::error::Kind::User));
}

// ---------------------------------------------------------------------------
// Digests and secrets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hosts_with_the_same_content_share_a_plan_digest() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /dig/a, content: '{{ host.labels.site }}' } } ]\n",
        )],
    );
    let sys = FakeSystem::new();
    let mut one = inputs(rev.clone(), &sys);
    one.host.id = "pi-1".into();
    one.host.labels = labels(&[("site", serde_json::json!("home"))]);
    let mut two = inputs(rev.clone(), &sys);
    two.host.id = "pi-2".into();
    two.host.labels = labels(&[("site", serde_json::json!("home"))]);
    let mut three = inputs(rev, &sys);
    three.host.id = "oci-1".into();
    three.host.labels = labels(&[("site", serde_json::json!("cloud"))]);
    let (p1, p2, p3) = (plan(&one).await, plan(&two).await, plan(&three).await);
    assert_eq!(p1.digest, p2.digest);
    assert_ne!(p1.host, p2.host);
    assert_ne!(p1.digest, p3.digest);
}

#[tokio::test]
async fn secrets_never_appear_in_plans_events_or_state() {
    const VALUE: &str = "hunter2-leak-test";
    let rev = revision(
        "r1",
        "version: 2\nsecrets: { api_token: { env: API_TOKEN } }\n",
        vec![module_with_files(
            "name: s\ntasks:\n  - test.render: { src: conf.j2, dest: /leak/conf }\n  - test.file: { path: /leak/plain, content: 'present={{ secrets.api_token.present }}' }\n",
            &[(
                "conf.j2",
                "token={{ secrets.api_token }}\nplain={{ vars.x | default('v') }}\n",
            )],
        )],
    );
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let mut inputs = inputs_with_store(rev, &sys, store.clone());
    let mut secrets = SecretStore::new();
    secrets.insert(Secret::new(
        "api_token",
        VALUE.into(),
        &InstallKey::from_bytes([9; 32]),
    ));
    inputs.secrets = Arc::new(secrets);

    let plan = plan(&inputs).await;
    let json = serde_json::to_string(&plan).unwrap();
    assert!(!json.contains(VALUE), "plan leaks the secret: {json}");
    assert!(
        json.contains("secret:api_token@"),
        "plan shows the token: {json}"
    );
    assert!(!format!("{plan:?}").contains(VALUE));
    assert_eq!(preview(step(&plan, "s", "/leak/plain")), "present=True");

    let (outcome, events) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert!(
        sys.file_string("/leak/conf").unwrap().contains(VALUE),
        "the real value reaches the disk through the sink"
    );
    let events_json = serde_json::to_string(&events).unwrap();
    assert!(!events_json.contains(VALUE), "{events_json}");
    let state = store.load().unwrap();
    let state_text = format!("{state:?}");
    assert!(!state_text.contains(VALUE), "{state_text}");
    let record = state
        .get(&ResourceKey::new("s", "test.render", "/leak/conf"))
        .unwrap();
    assert!(
        record
            .shadow
            .as_ref()
            .unwrap()
            .get()
            .contains("secret:api_token@")
    );
    assert_eq!(
        record.content_digest,
        Some(Digest::of(sys.file("/leak/conf").unwrap().as_slice()))
    );

    // Re-planning diffs the new render against the previous shadow (Cx.previous).
    let again = self::plan(&inputs).await;
    assert_eq!(step(&again, "s", "/leak/conf").planned, Planned::NoOp);
}

#[tokio::test]
async fn secret_values_in_plain_task_fields_are_refused_without_leaking() {
    const VALUE: &str = "hunter2-field-test";
    let rev = revision(
        "r1",
        "version: 2\nsecrets: { api_token: { env: API_TOKEN } }\n",
        vec![module(
            "name: s\ntasks: [ { test.file: { path: '/leak/{{ secrets.api_token }}' } } ]\n",
        )],
    );
    let mut inputs = inputs(rev, &FakeSystem::new());
    let mut secrets = SecretStore::new();
    secrets.insert(Secret::new(
        "api_token",
        VALUE.into(),
        &InstallKey::from_bytes([9; 32]),
    ));
    inputs.secrets = Arc::new(secrets);
    let plan = plan(&inputs).await;
    let broken = indeterminate(&plan, "s");
    assert!(broken.plan_error);
    let shown = serde_json::to_string(&plan).unwrap();
    assert!(!shown.contains(VALUE), "{shown}");
    assert!(broken.reason.contains("plain values"), "{}", broken.reason);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validation_reports_every_static_problem() {
    let rev = revision(
        "r1",
        "version: 2\nsecrets: { declared: { env: D } }\nfleet:\n  groups: { g: 'secrets.undeclared_group.present' }\n",
        vec![
            module_with_files(
                "name: a\nneeds: [missing, b]\ntasks:\n  - no.such_kind: { x: 1 }\n  - test.file: { path: '{{ vars.p', content: '{{ secrets.undeclared_task }}' }\n    notify: [nope, b:nope, c:x]\nhandlers:\n  ok: { systemd.restart: { name: x } }\nhealth:\n  - bogus.check: {}\n",
                &[("broken.j2", "{% if %}")],
            ),
            module("name: b\nneeds: [a]\n"),
        ],
    );
    let report = validate(&rev).unwrap();
    assert!(!report.is_ok());
    let messages: Vec<&str> = report.errors.iter().map(|e| e.message.as_str()).collect();
    let expect = |needle: &str| {
        assert!(
            messages.iter().any(|m| m.contains(needle)),
            "missing '{needle}' in {messages:#?}"
        )
    };
    expect("needs the module 'missing'");
    expect("cycle");
    expect("'no.such_kind' is not a task kind");
    expect("template does not compile");
    expect("secret 'undeclared_task'");
    expect("secret 'undeclared_group'");
    expect("declares no handler 'nope'");
    expect("no module 'c'");
    expect("'bogus.check' is not a health check kind");
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.location.contains("broken.j2"))
    );
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.location.contains("tasks[1]"))
    );
    let err = report.into_result().unwrap_err();
    assert!(err.is(crate::error::Kind::User));
    assert!(err.message().contains("validation error"));
}

#[tokio::test]
async fn a_valid_revision_validates_cleanly_and_with_facts() {
    let rev = revision(
        "r1",
        "version: 2\nsecrets: { token: { env: T } }\n",
        vec![module_with_files(
            "name: ok\ntasks:\n  - test.file: { path: /ok, content: '{{ secrets.token.present }}' }\n    notify: [restart]\nhandlers:\n  restart: { systemd.restart: { name: nomad } }\nhealth:\n  - test.file_exists: { path: /ok }\n",
            &[("t.j2", "{{ secrets.token }} {{ vars.x | default(1) }}")],
        )],
    );
    let report = validate(&rev).unwrap();
    assert!(report.is_ok(), "{report}");
    let host = HostIdentity {
        id: "h".into(),
        labels: Labels::new(),
    };
    let with_facts = validate_with_facts(&rev, &host, &default_facts())
        .await
        .unwrap();
    assert!(with_facts.is_ok(), "{with_facts}");
}

#[rstest]
#[case::debian(serde_json::json!({"os": {"family": "debian"}}), true)]
#[case::ubuntu_id(serde_json::json!({"os": {"id": "ubuntu"}}), true)]
#[case::darwin(serde_json::json!({"os": {"family": "darwin"}}), false)]
#[case::windows(serde_json::json!({"os": {"family": "windows"}}), false)]
#[tokio::test]
async fn a_dry_run_checks_kinds_against_the_platform_the_facts_describe(
    #[case] tree: serde_json::Value,
    #[case] linux_ok: bool,
) {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.linux: { path: /plat/linux-only, run: dryplat }\n",
        )],
    );
    let host = HostIdentity {
        id: "h".into(),
        labels: Labels::new(),
    };
    let report = validate_with_facts(&rev, &host, &facts(tree))
        .await
        .unwrap();
    assert_eq!(report.is_ok(), linux_ok, "{report}");
    if !linux_ok {
        assert!(report.to_string().contains("test.linux"), "{report}");
    }
}

#[tokio::test]
async fn validation_with_facts_catches_duplicate_keys_and_bad_var_references() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /dup }\n  - test.file: { path: /dup }\n",
        )],
    );
    let host = HostIdentity {
        id: "h".into(),
        labels: Labels::new(),
    };
    let report = validate_with_facts(&rev, &host, &default_facts())
        .await
        .unwrap();
    assert!(!report.is_ok());
    assert!(
        report.errors[0].message.contains("same resource"),
        "{report}"
    );

    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: '{{ vars.missing }}' } } ]\n",
        )],
    );
    let report = validate_with_facts(&rev, &host, &default_facts())
        .await
        .unwrap();
    assert!(!report.is_ok());
    assert!(
        report.errors[0].message.contains("could not be rendered"),
        "{report}"
    );
}

#[tokio::test]
async fn validation_with_facts_warns_about_modules_a_host_would_defer() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module("name: p\nfacts: { provides: [test.counter] }\n"),
            module("name: q\nneeds: [p]\nvars: { v: '{{ host.facts.test.counter.value }}' }\n"),
            module("name: r\nvars: { v: '{{ vars.nope }}' }\n"),
        ],
    );
    let host = HostIdentity {
        id: "fixture".into(),
        labels: Labels::new(),
    };
    let (report, summary) = crate::engine::validate_host(&rev, &host, &default_facts())
        .await
        .unwrap();
    assert!(
        report.warnings.iter().any(|w| w
            .message
            .contains("module 'q': would be deferred until 'p'")),
        "{report}"
    );
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.message.contains("module 'r'") && e.message.contains("vars.nope")),
        "{report}"
    );
    assert_eq!(report.errors.len(), 1, "{report}");
    assert_eq!(summary.unwrap().selected, vec!["p", "r", "q"]);
}

#[test]
fn secret_reference_scanning_finds_names() {
    let refs = validate::secret_references(
        "{{ secrets.a }} secrets.b.present secrets.c.fingerprint secrets.a",
    );
    assert_eq!(refs.into_iter().collect::<Vec<_>>(), vec!["a", "b", "c"]);
}

#[rstest]
#[case(r#"{{ secrets["db-pass"] }}"#, &["db-pass"])]
#[case("{{ secrets['tok'] }}", &["tok"])]
#[case("{{secrets.a}}", &["a"])]
#[case("secrets.a", &["a"])]
#[case("x = mysecrets.a", &[])]
#[case("host.secrets.a and not_secrets.b", &[])]
#[case("(secrets.a.present) && !secrets . b . present", &["a", "b"])]
fn secret_reference_scanning_respects_word_boundaries(
    #[case] text: &str,
    #[case] expected: &[&str],
) {
    let refs = validate::secret_references(text);
    assert_eq!(
        refs.iter().map(String::as_str).collect::<Vec<_>>(),
        expected
    );
}

// ---------------------------------------------------------------------------
// Per-step outcomes and structured errors
// ---------------------------------------------------------------------------

fn module_outcome<'a>(outcome: &'a ApplyOutcome, module: &str) -> &'a ModuleOutcome {
    outcome
        .module_results
        .iter()
        .find(|m| m.module == module)
        .unwrap_or_else(|| panic!("no result for {module}: {outcome:#?}"))
}

fn step_states(outcome: &ModuleOutcome) -> Vec<(&str, StepState)> {
    outcome
        .steps
        .iter()
        .map(|s| (s.id.as_str(), s.state))
        .collect()
}

#[tokio::test]
async fn a_failing_step_is_failed_with_a_structured_error_and_later_steps_are_not_run() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: m\ntasks:\n  - test.file: { path: /so/a, content: a, run: so1 }\n  - test.file: { path: /so/b, run: so1, fail_times: 9 }\n    name: write b\n  - test.file: { path: /so/c, content: c, run: so1 }\n",
            ),
            module(
                "name: later\nneeds: [m]\ntasks: [ { test.file: { path: /so/later, run: so1 } } ]\n",
            ),
        ],
    );
    let (outcome, events) = apply(&inputs(rev, &FakeSystem::new()).with_parallelism(1)).await;
    let m = module_outcome(&outcome, "m");
    assert_eq!(
        step_states(m),
        vec![
            ("test.file[/so/a]", StepState::Changed),
            ("test.file[/so/b]", StepState::Failed),
            ("test.file[/so/c]", StepState::NotRun),
        ]
    );
    let failed = &m.steps[1];
    assert_eq!(failed.name.as_deref(), Some("write b"));
    assert_eq!(failed.summary, "create /so/b", "the plan's summary");
    assert!(failed.started_at.is_some() && failed.duration_ms.is_some());
    let detail = failed.error_detail.as_ref().expect("a structured error");
    assert_eq!(detail.kind, crate::error::ErrorKind::User);
    assert_eq!(detail.message, "transient failure #1 writing /so/b");
    assert_eq!(detail.advice, vec!["Try again."]);
    assert_eq!(m.steps[0].result.as_deref(), Some("wrote /so/a"));
    assert!(m.steps[2].started_at.is_none());

    let module_error = m.error_detail.as_ref().expect("the module's error");
    assert_eq!(
        module_error.message,
        "m/test.file[/so/b] failed: transient failure #1 writing /so/b"
    );
    assert_eq!(module_error.advice, vec!["Try again."]);
    for line in [m.error.as_deref(), outcome.error.as_deref()] {
        let line = line.expect("an error line");
        assert!(!line.contains("To try and fix this"), "{line}");
        assert!(!line.contains("Try again"), "no advice inline: {line}");
    }
    assert_eq!(
        step_states(module_outcome(&outcome, "later")),
        vec![("test.file[/so/later]", StepState::NotRun)],
        "a module that never started lists its planned steps as not run"
    );

    let failed_event = events
        .iter()
        .find_map(|e| match e {
            ApplyEvent::StepFailed {
                id,
                error,
                error_detail,
                ..
            } => Some((id, error, error_detail)),
            _ => None,
        })
        .expect("a StepFailed event");
    assert_eq!(failed_event.0, "test.file[/so/b]");
    assert_eq!(failed_event.1, "transient failure #1 writing /so/b");
    assert_eq!(failed_event.2.as_ref(), Some(detail));
    assert!(
        events
            .iter()
            .any(|e| e.step() == Some(("m", "test.file[/so/a]"))
                && e.step_state() == Some(StepState::Running))
    );
}

#[tokio::test]
async fn blocked_steps_and_unnotified_handlers_are_reported_as_such() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /bk/tool, requires: /bk/missing, run: bk1 }\n  - test.file: { path: /bk/conf, content: c, run: bk1, unchanged: true }\n    notify: [restart]\nhandlers:\n  restart: { systemd.restart: { name: x, run: bk1 } }\n",
        )],
    );
    let (outcome, events) = apply(&inputs(rev, &FakeSystem::new())).await;
    let m = module_outcome(&outcome, "m");
    assert_eq!(m.status, ModuleStatus::Blocked);
    assert_eq!(
        step_states(m),
        vec![
            ("test.file[/bk/tool]", StepState::Blocked),
            ("test.file[/bk/conf]", StepState::Unchanged),
        ]
    );
    assert_eq!(m.handlers.len(), 1, "{m:#?}");
    assert_eq!(m.handlers[0].id, "m:restart");
    assert_eq!(m.handlers[0].state, StepState::Skipped);
    assert!(events.iter().any(|e| matches!(e, ApplyEvent::StepBlocked { id, reason, .. } if id == "test.file[/bk/tool]" && reason.contains("/bk/missing"))));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ApplyEvent::StepSkipped { id, .. } if id == "m:restart"))
    );
    assert!(
        !events.iter().any(
            |e| matches!(e, ApplyEvent::StepFinished { id, .. } if id == "test.file[/bk/tool]")
        ),
        "a blocked step is not reported as finished"
    );
}

#[tokio::test]
async fn handlers_that_ran_are_listed_under_the_module_with_their_result() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.file: { path: /hr/a, content: a, run: hr1 }\n    notify: [restart]\nhandlers:\n  restart: { systemd.restart: { name: x, run: hr1 } }\n",
        )],
    );
    let (outcome, events) = apply(&inputs(rev, &FakeSystem::new())).await;
    let m = module_outcome(&outcome, "m");
    assert_eq!(m.handlers.len(), 1, "{m:#?}");
    assert_eq!(m.handlers[0].state, StepState::Changed);
    assert!(m.handlers[0].result.is_some());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ApplyEvent::StepStarted { id, .. } if id == "m:restart"))
    );
}

#[tokio::test]
async fn steps_of_a_rolled_back_module_are_rolled_back() {
    let sys = FakeSystem::new();
    let r1 = revision("r1", FLEET, rollback_modules("rb-steps", "new", 99));
    let stable = revision("r0", FLEET, rollback_modules("rb-steps", "stable", 0));
    signal("rb-steps-release");
    let inputs = inputs(r1, &sys)
        .with_stable_revision(Some(stable))
        .with_parallelism(1);
    let (outcome, _) = apply(&inputs).await;
    let b = module_outcome(&outcome, "b");
    assert_eq!(b.status, ModuleStatus::RolledBack);
    assert_eq!(
        step_states(b),
        vec![("test.file[/rb/rb-steps/b]", StepState::Failed)],
        "the failing step stays failed so the reader sees what broke"
    );
    assert!(b.error_detail.is_some());
    assert_eq!(
        step_states(module_outcome(&outcome, "c")),
        vec![("test.file[/rb/rb-steps/c]", StepState::NotRun)]
    );
}

#[tokio::test]
async fn removals_are_reported_run_or_not_run() {
    let mut state = State::default();
    state.record(record_for(
        "gone",
        "test.file",
        "/rm/orphan",
        RemovalPolicy::Cleanup,
        "r0",
    ));
    let sys = FakeSystem::new().with_file("/rm/orphan", "x", FilePerms::mode(0o644));
    let ok = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /rm/a, run: rm1 } } ]\n",
        )],
    );
    let store = Arc::new(MemoryStateStore::with(state.clone()));
    let (outcome, _) = apply(&inputs_with_store(ok, &sys, store)).await;
    assert_eq!(outcome.removals_run.len(), 1, "{outcome:#?}");
    let removal = &outcome.removals_run[0];
    assert_eq!(removal.resource, "gone/test.file[/rm/orphan]");
    assert_eq!(removal.state, StepState::Changed);

    let failing = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /rm/b, run: rm2, fail_times: 9 } } ]\n",
        )],
    );
    let store = Arc::new(MemoryStateStore::with(state));
    let (outcome, _) = apply(&inputs_with_store(failing, &sys, store)).await;
    assert_eq!(
        outcome
            .removals_run
            .iter()
            .map(|r| r.state)
            .collect::<Vec<_>>(),
        vec![StepState::NotRun],
        "a halted run leaves its removals not run"
    );
}

#[tokio::test]
async fn errors_are_redacted_in_every_field() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /red/a, run: red1, fail_times: 9 } } ]\n",
        )],
    );
    let mut inputs = inputs(rev, &FakeSystem::new());
    let mut secrets = SecretStore::new();
    // Values that appear in the failure's message and in its advice.
    for (name, value) in [("pw", "transient failure"), ("tip", "Try again")] {
        secrets.insert(Secret::new(
            name,
            value.into(),
            &InstallKey::from_bytes([4; 32]),
        ));
    }
    inputs.secrets = Arc::new(secrets);
    let (outcome, events) = apply(&inputs).await;
    assert!(
        module_outcome(&outcome, "m").steps[0]
            .error_detail
            .is_some()
    );
    let json = serde_json::to_string(&outcome).unwrap();
    let events = serde_json::to_string(&events).unwrap();
    for text in [&json, &events] {
        assert!(!text.contains("transient failure"), "{text}");
        assert!(!text.contains("Try again"), "{text}");
        assert!(text.contains("error_detail"), "{text}");
    }
}

#[tokio::test]
async fn an_outcome_survives_json_and_old_outcomes_still_parse() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks: [ { test.file: { path: /js/a, run: js1 } } ]\n",
        )],
    );
    let (outcome, _) = apply(&inputs(rev, &FakeSystem::new())).await;
    let back: ApplyOutcome =
        serde_json::from_value(serde_json::to_value(&outcome).unwrap()).unwrap();
    assert_eq!(
        back.module_results[0].steps,
        outcome.module_results[0].steps
    );
    assert!(back.started_at.is_some() && back.finished_at.is_some());
    let old: ApplyOutcome = serde_json::from_value(serde_json::json!({
        "plan_digest": outcome.plan_digest, "succeeded": true, "changes": 0, "removals": 0,
        "duration_ms": 1, "failed_module": null, "rolled_back_modules": [], "not_started": [],
        "health": null, "error": null,
        "module_results": [{"module": "m", "status": "applied", "changes": 1, "duration_ms": 1, "error": null}]
    }))
    .unwrap();
    assert!(old.module_results[0].steps.is_empty());
    let old_event: ApplyEvent = serde_json::from_value(serde_json::json!({
        "event": "step_failed", "module": "m", "task_id": "t", "kind": "k", "error": "boom"
    }))
    .unwrap();
    assert!(
        matches!(old_event, ApplyEvent::StepFailed { id, error_detail: None, .. } if id.is_empty())
    );
}

#[tokio::test]
async fn plan_errors_carry_structured_detail_outside_the_digest() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module("name: bad\ntasks: [ { test.file: { path: '{{ vars.missing }}' } } ]\n"),
            module("name: ok\ntasks: [ { test.file: { path: /pe/ok } } ]\n"),
        ],
    );
    let plan = plan(&inputs(rev, &FakeSystem::new())).await;
    let bad = indeterminate(&plan, "bad");
    assert!(bad.plan_error);
    let detail = bad.error_detail.as_ref().expect("structured detail");
    assert!(
        !detail.message.contains("To try and fix this"),
        "{detail:?}"
    );
    assert!(!detail.advice.is_empty(), "{detail:?}");
    let mut without = plan.clone();
    for i in &mut without.indeterminate {
        i.error_detail = None;
    }
    assert_eq!(without.compute_digest(), plan.digest);
}

#[tokio::test]
async fn a_handler_whose_owner_never_started_is_reported_as_run_under_its_owner() {
    let rev = revision(
        "r1",
        FLEET,
        vec![
            module(
                "name: common\ntasks:\n  - test.file: { path: /lo/common, run: lo1 }\n    notify: [nomad:restart]\n",
            ),
            module(
                "name: breaker\nneeds: [common]\ntasks: [ { test.file: { path: /lo/b, run: lo1, fail_times: 9 } } ]\n",
            ),
            module(
                "name: nomad\nneeds: [common, breaker]\ntasks:\n  - test.file: { path: /lo/nomad, run: lo1 }\n    notify: [restart]\nhandlers:\n  restart: { systemd.restart: { name: nomad, run: lo1 } }\n",
            ),
        ],
    );
    let (outcome, _) = apply(&inputs(rev, &FakeSystem::new()).with_parallelism(1)).await;
    assert_eq!(status_of(&outcome, "nomad"), ModuleStatus::NotStarted);
    assert!(
        handler_calls("lo1").contains(&"handler nomad:systemd.restart nomad".to_string()),
        "the notified handler ran"
    );
    let nomad = module_outcome(&outcome, "nomad");
    assert_eq!(
        nomad
            .handlers
            .iter()
            .map(|h| (h.id.as_str(), h.state))
            .collect::<Vec<_>>(),
        vec![("nomad:restart", StepState::Changed)],
        "{nomad:#?}"
    );
    assert_eq!(
        step_states(nomad),
        vec![("test.file[/lo/nomad]", StepState::NotRun)]
    );
}

// ---------------------------------------------------------------------------
// Reboots
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
struct RebootSpec {
    #[serde(default)]
    run: String,
    #[serde(default)]
    delay_s: u64,
}

/// Asks for a reboot, as `system.reboot` does.
struct TestReboot;

#[async_trait]
impl Task for TestReboot {
    type Spec = RebootSpec;
    const KIND: &'static str = "test.reboot";

    fn key(_spec: &RebootSpec) -> String {
        "reboot".into()
    }

    async fn plan(&self, _cx: &Cx, _spec: &RebootSpec) -> crate::Result<Planned> {
        Ok(Planned::change(Diff::Opaque {
            summary: "reboot".into(),
        }))
    }

    async fn apply(&self, cx: &Cx, spec: &RebootSpec, _p: &Planned) -> crate::Result<Applied> {
        record(&spec.run, format!("reboot requested by {}", cx.module));
        Ok(
            Applied::changed("reboot requested").with_reboot(crate::reboot::RebootRequest::new(
                Duration::from_secs(spec.delay_s),
                Duration::from_secs(300),
            )),
        )
    }
}

crate::register_task!(TestReboot);

#[rstest]
#[case::handler_notified(
    "name: pi\ntasks:\n  - test.file: { path: /rb/a, run: rb1 }\n    notify: [reboot]\nhandlers:\n  reboot: { test.reboot: { run: rb1, delay_s: 7 } }\nhealth:\n  - name: flaky\n    test.flaky: { run: rb1, pass_after: 100 }\n    interval: 1s\n",
    "rb1",
    Some(("pi:reboot", 7))
)]
#[case::task(
    "name: pi\ntasks:\n  - test.reboot: { run: rb2 }\nhealth:\n  - name: flaky\n    test.flaky: { run: rb2, pass_after: 100 }\n    interval: 1s\n",
    "rb2",
    Some(("pi/test.reboot[reboot]", 0))
)]
#[case::handler_not_notified(
    "name: pi\ntasks:\n  - test.file: { path: /rb/c, run: rb3, unchanged: true }\n    notify: [reboot]\nhandlers:\n  reboot: { test.reboot: { run: rb3 } }\n",
    "rb3",
    None
)]
#[tokio::test(start_paused = true)]
async fn a_reboot_is_collected_for_after_the_run_and_defers_health(
    #[case] yaml: &str,
    #[case] run: &str,
    #[case] expected: Option<(&str, u64)>,
) {
    let rev = revision(
        "r1",
        "version: 2\nrollout: { settle: 2s }\n",
        vec![module(yaml)],
    );
    let (outcome, _) = apply(&inputs(rev, &FakeSystem::new())).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    let got = outcome
        .reboot
        .as_ref()
        .map(|r| (r.requested_by.join(","), r.delay.as_secs()));
    assert_eq!(got, expected.map(|(by, d)| (by.to_owned(), d)));
    if expected.is_some() {
        // The check that would fail before the reboot never ran.
        assert!(!calls(run).iter().any(|c| c.starts_with("health attempt")));
        assert!(outcome.health.unwrap().results.is_empty());
    }
}

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

const ACTION_MODULE: &str = r#"
name: web
tasks:
  - test.file: { path: /web/conf, content: v1 }
handlers:
  reload: { systemd.reload: { name: web, run: act1 } }
actions:
  rotate:
    test.file: { path: /web/rotated, content: '{{ vars.stamp }}', run: act1 }
    notify: [reload]
  noop: { test.file: { path: /web/conf, content: v1 } }
  never: { test.file: { path: /web/never }, when: 'host.labels.role == "db"' }
vars: { stamp: s1 }
health:
  - test.file_exists: { path: /web/rotated }
"#;

async fn run_action(inputs: &Inputs, module: &str, action: &str) -> ApplyOutcome {
    let (plan, _) = Engine::plan_action(inputs, module, action)
        .await
        .unwrap_or_else(|e| panic!("plan_action failed: {}", e.message()));
    let (tx, mut rx) = tokio::sync::mpsc::channel(4096);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let outcome = Engine::run_action(inputs, module, action, &plan, tx)
        .await
        .unwrap_or_else(|e| panic!("run_action failed: {}", e.message()));
    drain.await.unwrap();
    outcome
}

#[tokio::test]
async fn an_action_plans_its_step_and_handlers_alone() {
    let rev = revision("r1", FLEET, vec![module(ACTION_MODULE)]);
    let sys = FakeSystem::new();
    let (plan, _) = Engine::plan_action(&inputs(rev, &sys), "web", "rotate")
        .await
        .unwrap();
    assert_eq!(module_names(&plan), vec!["web"]);
    let steps: Vec<&str> = plan.steps().map(|s| s.resource.key.as_str()).collect();
    assert_eq!(
        steps,
        vec!["action:rotate"],
        "the module's tasks are not planned"
    );
    assert_eq!(plan.modules[0].handlers[0].handler, "reload");
    assert!(plan.removals.is_empty());
}

#[tokio::test]
async fn an_action_runs_handlers_and_health_and_records_nothing() {
    let rev = revision("r1", FLEET, vec![module(ACTION_MODULE)]);
    let sys = FakeSystem::new();
    let store = Arc::new(MemoryStateStore::default());
    let inputs = inputs_with_store(rev, &sys, store.clone());
    let outcome = run_action(&inputs, "web", "rotate").await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(outcome.changes, 1);
    assert_eq!(sys.file_string("/web/rotated").as_deref(), Some("s1"));
    assert!(sys.file_string("/web/conf").is_none(), "tasks never run");
    assert!(calls("act1").contains(&"handler web:systemd.reload web".to_owned()));
    let health = outcome.health.unwrap().results;
    assert!(
        !health.is_empty(),
        "the module's checks ran after the change"
    );
    let state = store.load().unwrap();
    assert!(
        state.resources.is_empty(),
        "an action owns nothing: {state:#?}"
    );
    assert!(state.module_revisions.is_empty());
}

#[tokio::test]
async fn an_action_with_nothing_to_do_changes_nothing() {
    let rev = revision("r1", FLEET, vec![module(ACTION_MODULE)]);
    let sys = FakeSystem::new().with_file("/web/conf", b"v1", FilePerms::mode(0o644));
    let outcome = run_action(&inputs(rev, &sys), "web", "noop").await;
    assert!(outcome.succeeded);
    assert_eq!(outcome.changes, 0);
}

#[rstest]
#[case::unknown_action("web", "nope", "declares no action")]
#[case::unknown_module("db", "rotate", "no such module")]
#[case::when_false("web", "never", "not available")]
#[tokio::test]
async fn an_action_that_cannot_run_here_is_a_user_error(
    #[case] module_name: &str,
    #[case] action: &str,
    #[case] needle: &str,
) {
    let rev = revision("r1", FLEET, vec![module(ACTION_MODULE)]);
    let err = Engine::plan_action(&inputs(rev, &FakeSystem::new()), module_name, action)
        .await
        .expect_err("the action cannot be planned");
    assert!(err.is(crate::error::Kind::User));
    assert!(err.message().contains(needle), "{}", err.message());
}

#[tokio::test]
async fn an_action_of_an_unselected_module_is_refused() {
    let yaml = format!("{ACTION_MODULE}when: 'host.labels.role == \"db\"'\n");
    let rev = revision("r1", FLEET, vec![module(&yaml)]);
    let err = Engine::plan_action(&inputs(rev, &FakeSystem::new()), "web", "rotate")
        .await
        .expect_err("not selected");
    assert!(err.message().contains("not selected"), "{}", err.message());
}

#[tokio::test]
async fn a_stale_action_plan_is_refused() {
    let rev = revision("r1", FLEET, vec![module(ACTION_MODULE)]);
    let sys = FakeSystem::new().with_file("/web/conf", b"v1", FilePerms::mode(0o644));
    let inputs = inputs(rev, &sys);
    let (plan, _) = Engine::plan_action(&inputs, "web", "noop").await.unwrap();
    crate::system::System::remove(&sys, Path::new("/web/conf"), false)
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let err = Engine::run_action(&inputs, "web", "noop", &plan, tx)
        .await
        .expect_err("the host changed under the plan");
    assert!(StalePlan::find(&err).is_some());
}
