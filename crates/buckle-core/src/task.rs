//! The task contract and its registry.
//!
//! A task kind is a unit struct implementing [`Task`] and registered with
//! [`crate::register_task!`]. The engine never sees the typed `Spec`; it
//! works through the object-safe [`DynTask`], which is blanket-implemented.

use crate::model::RemovalPolicy;
use crate::plan::{Planned, ResourceKey};
use crate::secrets::SecretStore;
use crate::selectors::HostContext;
use crate::state::ResourceRecord;
use crate::system::System;
use crate::template::Templater;
use crate::{Platform, Platforms};
use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

/// Whether the engine is planning (read-only) or applying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Plan,
    Apply,
}

/// Read access to the module's files in the revision tree.
#[async_trait]
pub trait Source: Send + Sync {
    /// Reads `path` relative to the module's `files/` directory. `Ok(None)` when absent.
    async fn read(&self, path: &Path) -> crate::Result<Option<Vec<u8>>>;

    /// Lists files under `path` (relative), recursively, as relative paths.
    async fn list(&self, path: &Path) -> crate::Result<Vec<PathBuf>>;
}

/// A [`Source`] over a directory on disk.
#[derive(Debug, Clone)]
pub struct DirSource {
    pub root: PathBuf,
}

impl DirSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn resolve(&self, path: &Path) -> crate::Result<PathBuf> {
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(crate::error::user(
                format!(
                    "The source path '{}' must be relative and may not contain '..'.",
                    path.display()
                ),
                &["Reference files relative to the module's files/ directory."],
            ));
        }
        Ok(self.root.join(path))
    }
}

#[async_trait]
impl Source for DirSource {
    async fn read(&self, path: &Path) -> crate::Result<Option<Vec<u8>>> {
        use crate::error::HumanizableResult;
        match tokio::fs::read(self.resolve(path)?).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).to_human_error(),
        }
    }

    async fn list(&self, path: &Path) -> crate::Result<Vec<PathBuf>> {
        let base = self.resolve(path)?;
        let mut out = Vec::new();
        let mut stack = vec![base.clone()];
        while let Some(dir) = stack.pop() {
            let mut rd = match tokio::fs::read_dir(&dir).await {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(crate::error::HumanizableError::to_human_error(e)),
            };
            while let Some(entry) = rd
                .next_entry()
                .await
                .map_err(crate::error::HumanizableError::to_human_error)?
            {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if let Ok(rel) = p.strip_prefix(&base) {
                    out.push(rel.to_path_buf());
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

/// Everything a task needs while planning or applying.
#[derive(Clone)]
pub struct Cx {
    pub mode: Mode,
    pub system: Arc<dyn System>,
    /// The host context scoped to the current module and task.
    pub host: Arc<HostContext>,
    pub templater: Arc<Templater>,
    /// The current module's files.
    pub source: Arc<dyn Source>,
    pub module: String,
    /// Secrets available to this host (also reachable via `host.secrets`).
    pub secrets: Arc<SecretStore>,
    /// Checks waived for this run (module/check names), for `continue` after a failure.
    pub waived_checks: Arc<Vec<String>>,
}

impl Cx {
    pub fn platform(&self) -> Platform {
        self.system.platform()
    }

    /// Renders a template string that must not reference secrets.
    pub fn render_plain(&self, source: &str) -> crate::Result<String> {
        if Templater::needs_render(source) {
            self.templater.render_plain(source, &self.host)
        } else {
            Ok(source.to_owned())
        }
    }
}

/// A typed, idempotent operation on the host.
#[async_trait]
pub trait Task: Send + Sync + 'static {
    /// Deserialised from the YAML under the kind key, after the engine has
    /// rendered string scalars with [`Templater::render_plain`].
    type Spec: DeserializeOwned + Serialize + schemars::JsonSchema + Send + Sync + 'static;

    /// `file.template`, `apt.package`, …
    const KIND: &'static str;

    /// Resource locks the executor serialises on across modules.
    const LOCKS: &'static [&'static str] = &[];

    const PLATFORMS: Platforms = Platforms::ALL;

    /// Default removal policy for the kind. Cleanup for every kind; `command.run`
    /// has nothing to infer and reports "no cleanup defined" unless given `remove:`.
    const REMOVAL_DEFAULT: RemovalPolicy = RemovalPolicy::Cleanup;

    /// Removal of this kind can take unrelated things down (packages, users, groups);
    /// plans flag it with a caution.
    const REMOVAL_CAUTION: bool = false;

    /// One-line description of the kind for the catalog.
    const DESCRIPTION: &'static str = "";

    /// Stable identity for state tracking: a path, a package name, a unit name.
    fn key(spec: &Self::Spec) -> String;

    /// A human summary of the desired state, for plans: "nomad present", "/etc/x (0644 root:root)".
    fn describe(spec: &Self::Spec) -> String {
        Self::key(spec)
    }

    /// Inspect the host. Pure: must not change anything.
    async fn plan(&self, cx: &Cx, spec: &Self::Spec) -> crate::Result<Planned>;

    /// Make it so. Only called when `plan` returned a change. Returns what to record.
    async fn apply(&self, cx: &Cx, spec: &Self::Spec, planned: &Planned) -> crate::Result<Applied>;

    /// Describe what `remove` would do for a recorded resource (shown as a plan warning).
    fn describe_removal(&self, record: &ResourceRecord) -> String {
        format!("remove {}", record.key)
    }

    /// Undo a previously applied resource.
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> crate::Result<Applied> {
        let _ = (cx, record);
        Ok(Applied::skipped("this task kind does not define a cleanup"))
    }
}

/// What an apply or remove did, for state and telemetry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    pub changed: bool,
    pub summary: String,
    /// Things created on the host that removal should undo.
    #[serde(default)]
    pub artefacts: Vec<Artefact>,
    /// Redacted render of written content, kept in state for future diffs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<crate::secrets::Redacted<String>>,
    /// Digest of the real content written, for drift detection without reading secrets back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_digest: Option<crate::Digest>,
}

impl Applied {
    pub fn changed(summary: impl Into<String>) -> Self {
        Self {
            changed: true,
            summary: summary.into(),
            ..Default::default()
        }
    }

    pub fn unchanged(summary: impl Into<String>) -> Self {
        Self {
            changed: false,
            summary: summary.into(),
            ..Default::default()
        }
    }

    pub fn skipped(reason: impl Into<String>) -> Self {
        Self::unchanged(reason)
    }

    pub fn with_artefact(mut self, artefact: Artefact) -> Self {
        self.artefacts.push(artefact);
        self
    }

    pub fn with_shadow(
        mut self,
        shadow: crate::secrets::Redacted<String>,
        digest: crate::Digest,
    ) -> Self {
        self.shadow = Some(shadow);
        self.content_digest = Some(digest);
        self
    }
}

/// Something a task created that removal should undo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Artefact {
    File {
        path: PathBuf,
    },
    Directory {
        path: PathBuf,
    },
    Symlink {
        path: PathBuf,
    },
    Package {
        manager: String,
        name: String,
    },
    Repository {
        manager: String,
        name: String,
    },
    Unit {
        name: String,
        enabled: bool,
        started: bool,
        unit_file: Option<PathBuf>,
    },
    User {
        name: String,
    },
    Group {
        name: String,
    },
    Line {
        path: PathBuf,
        marker: String,
    },
    Rule {
        subsystem: String,
        id: String,
    },
    Other {
        description: String,
    },
}

/// Type-erased spec: a boxed `Task::Spec` plus the raw JSON it came from.
pub struct ErasedSpec {
    inner: Box<dyn Any + Send + Sync>,
    pub json: serde_json::Value,
}

impl ErasedSpec {
    pub fn downcast<S: 'static>(&self) -> Option<&S> {
        self.inner.downcast_ref::<S>()
    }
}

impl std::fmt::Debug for ErasedSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ErasedSpec({})", self.json)
    }
}

/// Object-safe view of a task kind used by the engine.
#[async_trait]
pub trait DynTask: Send + Sync {
    fn kind(&self) -> &'static str;
    fn locks(&self) -> &'static [&'static str];
    fn platforms(&self) -> Platforms;
    fn removal_default(&self) -> RemovalPolicy;
    fn removal_caution(&self) -> bool;
    fn description(&self) -> &'static str;
    fn schema(&self) -> schemars::Schema;
    /// Parses a (string-rendered) spec. Errors name the kind and the field.
    fn parse(&self, raw: serde_json::Value) -> crate::Result<ErasedSpec>;
    fn key(&self, spec: &ErasedSpec) -> String;
    fn describe(&self, spec: &ErasedSpec) -> String;
    async fn plan(&self, cx: &Cx, spec: &ErasedSpec) -> crate::Result<Planned>;
    async fn apply(&self, cx: &Cx, spec: &ErasedSpec, planned: &Planned) -> crate::Result<Applied>;
    fn describe_removal(&self, record: &ResourceRecord) -> String;
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> crate::Result<Applied>;
}

impl std::fmt::Debug for dyn DynTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DynTask({})", self.kind())
    }
}

fn wrong_spec(kind: &str) -> crate::Error {
    crate::error::system(
        format!("A spec of the wrong type was passed to the '{kind}' task."),
        &["This is a bug in the engine; please report it."],
    )
}

#[async_trait]
impl<T: Task> DynTask for T {
    fn kind(&self) -> &'static str {
        T::KIND
    }
    fn locks(&self) -> &'static [&'static str] {
        T::LOCKS
    }
    fn platforms(&self) -> Platforms {
        T::PLATFORMS
    }
    fn removal_default(&self) -> RemovalPolicy {
        T::REMOVAL_DEFAULT
    }
    fn removal_caution(&self) -> bool {
        T::REMOVAL_CAUTION
    }
    fn description(&self) -> &'static str {
        T::DESCRIPTION
    }
    fn schema(&self) -> schemars::Schema {
        schemars::schema_for!(T::Spec)
    }
    fn parse(&self, raw: serde_json::Value) -> crate::Result<ErasedSpec> {
        let spec: T::Spec = serde_json::from_value(raw.clone()).map_err(|e| {
            crate::error::wrap_user(
                e,
                format!("The '{}' task has an invalid spec.", T::KIND),
                &["Compare the task's fields against the kind's documentation (`buckle catalog`)."],
            )
        })?;
        Ok(ErasedSpec {
            inner: Box::new(spec),
            json: raw,
        })
    }
    fn key(&self, spec: &ErasedSpec) -> String {
        spec.downcast::<T::Spec>().map(T::key).unwrap_or_default()
    }
    fn describe(&self, spec: &ErasedSpec) -> String {
        spec.downcast::<T::Spec>()
            .map(T::describe)
            .unwrap_or_default()
    }
    async fn plan(&self, cx: &Cx, spec: &ErasedSpec) -> crate::Result<Planned> {
        let spec = spec
            .downcast::<T::Spec>()
            .ok_or_else(|| wrong_spec(T::KIND))?;
        Task::plan(self, cx, spec).await
    }
    async fn apply(&self, cx: &Cx, spec: &ErasedSpec, planned: &Planned) -> crate::Result<Applied> {
        let spec = spec
            .downcast::<T::Spec>()
            .ok_or_else(|| wrong_spec(T::KIND))?;
        Task::apply(self, cx, spec, planned).await
    }
    fn describe_removal(&self, record: &ResourceRecord) -> String {
        Task::describe_removal(self, record)
    }
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> crate::Result<Applied> {
        Task::remove(self, cx, record).await
    }
}

/// An `inventory` entry: a `'static` task kind.
pub struct TaskRegistration(pub &'static dyn DynTask);

impl TaskRegistration {
    pub const fn new<T: DynTask>(task: &'static T) -> Self {
        Self(task)
    }
}

inventory::collect!(TaskRegistration);

/// All registered task kinds by name. Built once; duplicate kinds are a startup panic.
pub fn registry() -> &'static HashMap<&'static str, &'static dyn DynTask> {
    static REGISTRY: LazyLock<HashMap<&'static str, &'static dyn DynTask>> = LazyLock::new(|| {
        let mut map = HashMap::new();
        for reg in inventory::iter::<TaskRegistration> {
            if map.insert(reg.0.kind(), reg.0).is_some() {
                panic!("two task kinds are registered as '{}'", reg.0.kind());
            }
        }
        map
    });
    &REGISTRY
}

/// Looks up a task kind, with a helpful error listing the known kinds.
pub fn lookup(kind: &str) -> crate::Result<&'static dyn DynTask> {
    registry().get(kind).copied().ok_or_else(|| {
        let mut known: Vec<&str> = registry().keys().copied().collect();
        known.sort_unstable();
        crate::error::user(
            format!(
                "'{kind}' is not a task kind this Buckle knows about. Known kinds: {}.",
                known.join(", ")
            ),
            &[
                "Check the spelling of the task kind in module.yaml.",
                "Run `buckle catalog` to list the available kinds.",
            ],
        )
    })
}

/// Derives the resource key for a task declaration: explicit `id` wins over the kind's key.
pub fn resource_key(
    module: &str,
    task: &'static dyn DynTask,
    spec: &ErasedSpec,
    explicit_id: Option<&str>,
) -> ResourceKey {
    ResourceKey::new(
        module,
        task.kind(),
        explicit_id
            .map(str::to_owned)
            .unwrap_or_else(|| task.key(spec)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial kind used to exercise the erasure layer.
    struct Touch;

    #[derive(Serialize, Deserialize, schemars::JsonSchema)]
    struct TouchSpec {
        path: String,
    }

    #[async_trait]
    impl Task for Touch {
        type Spec = TouchSpec;
        const KIND: &'static str = "test.touch";
        const LOCKS: &'static [&'static str] = &["fs"];
        fn key(spec: &TouchSpec) -> String {
            spec.path.clone()
        }
        async fn plan(&self, _cx: &Cx, spec: &TouchSpec) -> crate::Result<Planned> {
            Ok(Planned::change(crate::plan::Diff::Create {
                summary: format!("touch {}", spec.path),
                preview: None,
            }))
        }
        async fn apply(
            &self,
            _cx: &Cx,
            spec: &TouchSpec,
            _planned: &Planned,
        ) -> crate::Result<Applied> {
            Ok(Applied::changed(format!("touched {}", spec.path)))
        }
    }
    crate::register_task!(Touch);

    #[test]
    fn registry_finds_registered_kinds_and_rejects_unknown_ones() {
        let task = lookup("test.touch").expect("registered via register_task!");
        assert_eq!(task.locks(), &["fs"]);
        assert_eq!(task.removal_default(), RemovalPolicy::Cleanup);
        let err = lookup("nope.kind").unwrap_err();
        assert!(err.description().contains("test.touch"));
    }

    #[test]
    fn parse_and_key_go_through_the_typed_spec() {
        let task = lookup("test.touch").unwrap();
        let spec = task.parse(serde_json::json!({"path": "/tmp/x"})).unwrap();
        assert_eq!(task.key(&spec), "/tmp/x");
        assert_eq!(
            resource_key("m", task, &spec, None).to_string(),
            "m/test.touch[/tmp/x]"
        );
        assert_eq!(resource_key("m", task, &spec, Some("custom")).key, "custom");
        let err = task.parse(serde_json::json!({"nope": 1})).unwrap_err();
        assert!(err.is(crate::error::Kind::User));
        assert!(err.description().contains("test.touch"));
    }

    #[test]
    fn schema_is_generated_from_the_spec_type() {
        let schema = lookup("test.touch").unwrap().schema();
        let json = serde_json::to_value(schema).unwrap();
        assert!(json["properties"]["path"].is_object());
    }

    #[tokio::test]
    async fn dir_source_reads_relative_paths_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("etc/sub")).unwrap();
        std::fs::write(dir.path().join("etc/sub/a.conf"), "a").unwrap();
        let src = DirSource::new(dir.path());
        assert_eq!(
            src.read(Path::new("etc/sub/a.conf"))
                .await
                .unwrap()
                .unwrap(),
            b"a"
        );
        assert!(src.read(Path::new("etc/missing")).await.unwrap().is_none());
        assert!(src.read(Path::new("../escape")).await.is_err());
        assert!(src.read(Path::new("/abs")).await.is_err());
        assert_eq!(
            src.list(Path::new("etc")).await.unwrap(),
            vec![PathBuf::from("sub/a.conf")]
        );
        assert!(src.list(Path::new("nothing")).await.unwrap().is_empty());
    }
}
