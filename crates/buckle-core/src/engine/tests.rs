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
}

async fn plan_file(cx: &Cx, spec: &FileSpec) -> crate::Result<Planned> {
    record(&spec.run, format!("plan {}/{}", cx.module, spec.path));
    if let Some(required) = &spec.requires
        && !cx.system.stat(Path::new(required)).await?.is_some()
    {
        return Ok(Planned::blocked(format!("{required} is not installed")));
    }
    let existing = cx.system.read(Path::new(&spec.path)).await?;
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
        struct $name;

        #[async_trait]
        impl Task for $name {
            type Spec = FileSpec;
            const KIND: &'static str = $kind;
            const LOCKS: &'static [&'static str] = $locks;
            const PLATFORMS: Platforms = $platforms;
            const REMOVAL_CAUTION: bool = $caution;

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
    TestAlways,
    "test.always",
    locks = &[],
    platforms = Platforms::ALL,
    caution = false,
    always = true
);

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

/// Mounts `host.facts.test.counter.value` from the fake file `/facts/counter`.
struct CounterFacts;

#[async_trait]
impl crate::facts::FactProvider for CounterFacts {
    const NAME: &'static str = "test.counter";

    async fn collect(&self, cx: &Cx) -> crate::Result<serde_json::Value> {
        let value = cx
            .system
            .read(Path::new("/facts/counter"))
            .await?
            .map(|b| serde_json::Value::String(String::from_utf8_lossy(&b).into_owned()))
            .unwrap_or(serde_json::Value::Null);
        Ok(serde_json::json!({ "value": value }))
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
    })
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
    let err = plan_err(&inputs(rev, &FakeSystem::new())).await;
    assert!(err.is(crate::error::Kind::User));
    let msg = err.message();
    assert!(msg.contains("task #1"), "{msg}");
    assert!(msg.contains("task #3 'second'"), "{msg}");
    assert!(msg.contains("m/test.file[/dup]"), "{msg}");
    assert!(err.advice().iter().any(|a| a.contains("`id`")));
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
async fn unsupported_platforms_block_steps_and_are_warned_about() {
    let rev = revision(
        "r1",
        FLEET,
        vec![module(
            "name: m\ntasks:\n  - test.linux: { path: /plat/linux-only, run: plat1 }\n  - test.file: { path: /plat/any, run: plat1 }\n",
        )],
    );
    let sys = FakeSystem::new().with_platform(Platform::MacOs);
    let inputs = inputs(rev, &sys);
    let plan = plan(&inputs).await;
    match &step(&plan, "m", "/plat/linux-only").planned {
        Planned::Blocked { reason } => assert!(reason.contains("macos"), "{reason}"),
        other => panic!("expected blocked, got {other:?}"),
    }
    assert!(
        plan.warnings.iter().any(|w| w.contains("blocked")),
        "{:?}",
        plan.warnings
    );
    assert_eq!(plan.changes(), 1);
    let (outcome, _) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded);
    assert!(!sys.exists("/plat/linux-only"));
    assert!(sys.exists("/plat/any"));
    assert!(
        !calls("plat1")
            .iter()
            .any(|c| c.contains("linux-only") && c.starts_with("apply"))
    );
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
  - test.linux: { path: /tool/mac, run: blk1 }
handlers:
  restart: { systemd.restart: { name: tool, run: blk1 } }
"#,
        )],
    );
    let sys = FakeSystem::new().with_platform(Platform::MacOs);
    let inputs = inputs(rev, &sys);
    let plan = plan(&inputs).await;
    assert!(
        matches!(&step(&plan, "m", "/tool/conf").planned, Planned::Blocked { reason } if reason.contains("/tool/bin"))
    );
    assert!(matches!(
        step(&plan, "m", "/tool/mac").planned,
        Planned::Blocked { .. }
    ));
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
    let mac_warning = plan
        .warnings
        .iter()
        .find(|w| w.contains("/tool/mac"))
        .unwrap();
    assert!(!mac_warning.contains("planned again"), "{mac_warning}");

    let (outcome, events) = apply_plan(&inputs, &plan).await;
    assert!(outcome.succeeded, "{outcome:#?}");
    assert_eq!(
        outcome.changes, 2,
        "the blocked step was planned again and applied"
    );
    assert_eq!(sys.file_string("/tool/conf").as_deref(), Some("conf"));
    assert!(
        !sys.exists("/tool/mac"),
        "platform-blocked steps are never planned again"
    );
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
    let (outcome, _) = apply(&inputs(rev, &sys)).await;
    assert!(outcome.succeeded);
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
    let err = plan_err(&inputs).await;
    assert!(err.is(crate::error::Kind::User));
    let shown = format!("{} {:?}", err.message(), err);
    assert!(!shown.contains(VALUE), "{shown}");
    assert!(shown.contains("plain values"), "{shown}");
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

#[test]
fn secret_reference_scanning_finds_names() {
    let refs = validate::secret_references(
        "{{ secrets.a }} secrets.b.present secrets.c.fingerprint secrets.a",
    );
    assert_eq!(refs.into_iter().collect::<Vec<_>>(), vec!["a", "b", "c"]);
}
