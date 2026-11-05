//! `workloads/<name>/workload.yaml`: what a cluster target runs (design:
//! cluster workloads, D4, D5, D5b; resource lists, 2026-11-04). A workload
//! shares `name`, `description`, `when`, `needs`, `vars`, `health` and
//! `removal` with a module, but has no tasks, handlers or facts: it is an
//! ordered list of `resources` (jobs, volumes, variables and the other
//! cluster entities; on Kubernetes manifests and secrets) applied top to
//! bottom and removed bottom to top. Its `when` selects over `cluster.*`,
//! never `host.*`.

use super::{ClusterKind, HealthDecl};
use crate::Filter;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// A `needs` entry, as a module or a workload writes it: a bare name is the
/// same layer, `module:NAME` and `workload:NAME` cross the layers (D2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Need {
    Module(String),
    Workload(String),
}

impl Need {
    /// Parses a `needs` entry written in a file of `layer`.
    pub fn parse(raw: &str, layer: Layer) -> Need {
        let raw = raw.trim();
        if let Some(name) = raw.strip_prefix("module:") {
            Need::Module(name.trim().to_owned())
        } else if let Some(name) = raw.strip_prefix("workload:") {
            Need::Workload(name.trim().to_owned())
        } else {
            match layer {
                Layer::Module => Need::Module(raw.to_owned()),
                Layer::Workload => Need::Workload(raw.to_owned()),
            }
        }
    }

    /// The layer this edge points into.
    pub fn layer(&self) -> Layer {
        match self {
            Need::Module(_) => Layer::Module,
            Need::Workload(_) => Layer::Workload,
        }
    }

    /// The needed module's or workload's name.
    pub fn name(&self) -> &str {
        match self {
            Need::Module(n) | Need::Workload(n) => n,
        }
    }

    /// `module:NAME` / `workload:NAME`: the unambiguous written form.
    pub fn qualified(&self) -> String {
        match self {
            Need::Module(n) => format!("module:{n}"),
            Need::Workload(n) => format!("workload:{n}"),
        }
    }
}

impl std::fmt::Display for Need {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.qualified())
    }
}

/// The two layers of a configuration: modules run on hosts, workloads on
/// clusters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    Module,
    Workload,
}

/// What happens to a workload's objects when it leaves the configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkloadRemoval {
    /// Stop the job (delete the manifests); keep the data: volumes,
    /// variables, namespaces and other entities stay.
    #[default]
    Stop,
    /// Also delete the entities that hold state (volumes, variables,
    /// secrets); namespaces are kept. Destructive: `if_destructive` gates it.
    Purge,
    /// Leave everything in the cluster; only forget it.
    Keep,
}

/// Where a Nomad ACL policy attaches: a job, or a group or task within it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyAttach {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub job: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

/// The items of a variable or a Secret: name to value, where a value may
/// reference `secrets.*` (it then reaches the cluster through the API sink
/// and is planned by fingerprint).
pub type Items = IndexMap<String, String>;

/// What one `resources` entry is. The kind decides which scheduler the
/// workload runs on, which fields the entry may carry and what removal does
/// with the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    // ---- Nomad
    Job,
    Volume,
    Policy,
    Namespace,
    NodePool,
    Quota,
    // ---- Kubernetes
    Manifest,
    // ---- both
    /// Inline items the executor writes to the cluster's own secret store:
    /// a Nomad variable at the path, or a Kubernetes Secret with the name.
    Secret,
    /// Plain items, inline or read from files under `files/`, written to a
    /// Nomad variable at the path or a Kubernetes ConfigMap with the name;
    /// jobs read them at run time (`nomadVar`, a mounted ConfigMap).
    Config,
}

impl ResourceKind {
    pub const ALL: &'static [ResourceKind] = &[
        ResourceKind::Job,
        ResourceKind::Volume,
        ResourceKind::Policy,
        ResourceKind::Namespace,
        ResourceKind::NodePool,
        ResourceKind::Quota,
        ResourceKind::Manifest,
        ResourceKind::Secret,
        ResourceKind::Config,
    ];

    /// The file suffixes that imply each kind (after a trailing `.j2` is
    /// dropped). `*.hcl` without one of these needs `kind:`.
    pub const SUFFIXES: &'static [(&'static str, ResourceKind)] = &[
        (".nomad.hcl", ResourceKind::Job),
        (".job.hcl", ResourceKind::Job),
        (".volume.hcl", ResourceKind::Volume),
        (".policy.hcl", ResourceKind::Policy),
        (".namespace.hcl", ResourceKind::Namespace),
        (".pool.hcl", ResourceKind::NodePool),
        (".node_pool.hcl", ResourceKind::NodePool),
        (".quota.hcl", ResourceKind::Quota),
        (".yaml", ResourceKind::Manifest),
        (".yml", ResourceKind::Manifest),
        (".json", ResourceKind::Manifest),
    ];

    /// The kind a file name implies, if any.
    pub fn from_file(file: &str) -> Option<ResourceKind> {
        let file = file.strip_suffix(".j2").unwrap_or(file);
        Self::SUFFIXES
            .iter()
            .find(|(suffix, _)| file.ends_with(suffix))
            .map(|(_, kind)| *kind)
    }

    /// The kind as written in `kind:` (`node_pool`).
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Job => "job",
            ResourceKind::Volume => "volume",
            ResourceKind::Policy => "policy",
            ResourceKind::Namespace => "namespace",
            ResourceKind::NodePool => "node_pool",
            ResourceKind::Quota => "quota",
            ResourceKind::Manifest => "manifest",
            ResourceKind::Secret => "secret",
            ResourceKind::Config => "config",
        }
    }

    /// The kind in prose (`node pool`).
    pub fn label(self) -> &'static str {
        match self {
            ResourceKind::NodePool => "node pool",
            other => other.as_str(),
        }
    }

    pub fn parse(text: &str) -> Option<ResourceKind> {
        Self::ALL.iter().copied().find(|k| k.as_str() == text)
    }

    /// Which scheduler applies this kind of object; `None` for a kind both
    /// have (a secret, a config).
    pub fn scheduler(self) -> Option<ClusterKind> {
        match self {
            ResourceKind::Manifest => Some(ClusterKind::Kubernetes),
            ResourceKind::Secret | ResourceKind::Config => None,
            _ => Some(ClusterKind::Nomad),
        }
    }

    /// Whether the entry names a file under `files/` (as against inline
    /// items).
    pub fn is_file(self) -> bool {
        !self.is_items()
    }

    /// Whether the entry is a named set of items (a secret, a config): on
    /// Nomad both are variables, and two entries may share one path.
    pub fn is_items(self) -> bool {
        matches!(self, ResourceKind::Secret | ResourceKind::Config)
    }

    /// Whether the executor diffs this object field by field, so that
    /// `ignore` has fields to leave alone. Volumes are created once and never
    /// updated; a policy's rules are one text.
    pub fn supports_ignore(self) -> bool {
        matches!(
            self,
            ResourceKind::Job
                | ResourceKind::Namespace
                | ResourceKind::NodePool
                | ResourceKind::Quota
                | ResourceKind::Manifest
        )
    }

    /// Whether this is something that runs (a job, a manifest), as against
    /// an entity it uses. The ordering lint expects entities first.
    pub fn runs(self) -> bool {
        matches!(self, ResourceKind::Job | ResourceKind::Manifest)
    }
}

impl std::fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// One entry of `resources`, written as a bare file name, as
/// `{ file, kind?, … }`, or inline as `{ secret: NAME, items }` or
/// `{ config: NAME, items, files }`. Applied in list order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceEntry {
    pub kind: ResourceKind,
    /// The spec under `files/` for file-backed kinds.
    pub file: Option<String>,
    /// A secret's or config's name (a Nomad variable path, a Kubernetes
    /// Secret or ConfigMap name), or a policy's name (defaults to the file
    /// stem).
    pub name: Option<String>,
    /// The inline items of a secret or config.
    pub items: Items,
    /// Field paths of the live object that may differ from the spec without
    /// counting as a change or drift; the live value is kept when applying
    /// (`TaskGroups[*].Tasks[*].Config.image`, `spec.replicas`).
    pub ignore: Vec<String>,
    /// Jobs: variables another tool owns once the job runs (an updater's
    /// pinned digest). The live submission's value is the input to the next
    /// plan, unless the workload's own value changed since Buckle last wrote
    /// it.
    pub live_vars: Vec<String>,
    /// Jobs: files passed to this job as string variables, by variable name,
    /// over the workload's `files`. Configs: items read from files, by item
    /// name (a bare list names each item after its file, less `.j2`).
    pub files: IndexMap<String, String>,
    /// Policies: the job, group or task the policy attaches to.
    pub attach: Option<PolicyAttach>,
    /// Secrets: the Secret's `type` (default `Opaque`).
    pub secret_type: Option<String>,
}

impl ResourceEntry {
    fn new(kind: ResourceKind) -> Self {
        Self {
            kind,
            file: None,
            name: None,
            items: Items::new(),
            ignore: Vec::new(),
            live_vars: Vec::new(),
            files: IndexMap::new(),
            attach: None,
            secret_type: None,
        }
    }

    /// A file-backed entry of `kind`.
    pub fn of(kind: ResourceKind, file: impl Into<String>) -> Self {
        Self {
            file: Some(file.into()),
            ..Self::new(kind)
        }
    }

    /// A bare file name: the kind comes from its suffix.
    pub fn file(file: impl Into<String>) -> Result<Self, String> {
        let file = file.into();
        match ResourceKind::from_file(&file) {
            Some(kind) => Ok(Self::of(kind, file)),
            None => Err(unknown_kind(&file)),
        }
    }

    /// A secret called `name`: a Nomad variable at that path, a Kubernetes
    /// Secret with that name.
    pub fn secret(name: impl Into<String>, items: Items) -> Self {
        Self {
            name: Some(name.into()),
            items,
            ..Self::new(ResourceKind::Secret)
        }
    }

    /// A config called `name`: a Nomad variable at that path, a Kubernetes
    /// ConfigMap with that name, holding `items` and the contents of `files`.
    pub fn config(name: impl Into<String>, items: Items, files: IndexMap<String, String>) -> Self {
        Self {
            name: Some(name.into()),
            items,
            files,
            ..Self::new(ResourceKind::Config)
        }
    }

    /// What identifies the entry in the workload: its file, or the secret's
    /// or config's name. `restart_on` names entries by this.
    pub fn id(&self) -> &str {
        self.file
            .as_deref()
            .or(self.name.as_deref())
            .unwrap_or_default()
    }

    /// Every item name the entry holds: inline items, then file items.
    pub fn item_names(&self) -> impl Iterator<Item = &str> {
        self.items
            .keys()
            .chain(self.files.keys())
            .map(String::as_str)
    }

    /// `job paste.nomad.hcl`, `secret nomad/jobs/paste`.
    pub fn describe(&self) -> String {
        format!("{} {}", self.kind, self.id())
    }
}

fn unknown_kind(file: &str) -> String {
    let suffixes: Vec<String> = ResourceKind::SUFFIXES
        .iter()
        .map(|(s, _)| format!("`*{s}`"))
        .collect();
    format!(
        "the file '{file}' does not say what it is: name it with one of the suffixes {}, or set `kind:` ({}) on the entry",
        suffixes.join(", "),
        ResourceKind::ALL
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// `resources` as written: the bare-string and map forms.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawEntry {
    File(String),
    Full(Box<RawEntryMap>),
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawEntryMap {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    /// `secret: NAME`: inline items for the cluster's secret store (a Nomad
    /// variable path, a Kubernetes Secret name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
    /// `config: NAME`: plain items and files (a Nomad variable path, a
    /// Kubernetes ConfigMap name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    items: Items,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    ignore: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    live_vars: Vec<String>,
    #[serde(default, skip_serializing_if = "RawFiles::is_empty")]
    files: RawFiles,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attach: Option<PolicyAttach>,
    /// A Secret's type; on a file entry without `kind`, the entity type of
    /// the former `resources: [{ type, file }]` form.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    ty: Option<String>,
}

/// `files` as written: `{ name: file }`, or a bare list of files.
#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum RawFiles {
    Map(IndexMap<String, String>),
    List(Vec<String>),
}

impl Default for RawFiles {
    fn default() -> Self {
        RawFiles::Map(IndexMap::new())
    }
}

impl RawFiles {
    fn is_empty(&self) -> bool {
        match self {
            RawFiles::Map(m) => m.is_empty(),
            RawFiles::List(l) => l.is_empty(),
        }
    }
}

impl RawEntryMap {
    fn resolve(self) -> Result<ResourceEntry, String> {
        let named = match (&self.secret, &self.config) {
            (Some(_), Some(_)) => {
                return Err("a resource entry is a `secret` or a `config`, not both".to_owned());
            }
            (Some(_), None) => Some(ResourceKind::Secret),
            (None, Some(_)) => Some(ResourceKind::Config),
            (None, None) => None,
        };
        let kind = match (&self.kind, named, &self.file) {
            (Some(k), ..) => ResourceKind::parse(k).ok_or_else(|| {
                format!(
                    "'{k}' is not a resource kind; use one of {}",
                    ResourceKind::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?,
            (None, Some(kind), None) => kind,
            (None, None, Some(file)) => match (&self.ty, ResourceKind::from_file(file)) {
                (Some(ty), _) => ResourceKind::parse(ty)
                    .filter(|k| k.is_file())
                    .ok_or_else(|| format!("'{ty}' is not a resource type Buckle can apply"))?,
                (None, Some(kind)) => kind,
                (None, None) => return Err(unknown_kind(file)),
            },
            (None, None, None) => {
                return Err("a resource entry names a `file`, a `secret` or a `config`".to_owned());
            }
            (None, Some(_), Some(_)) => {
                return Err(
                    "a resource entry is a `file`, a `secret` or a `config`, not one and another"
                        .to_owned(),
                );
            }
        };
        let mut entry = ResourceEntry::new(kind);
        let fail = |field: &str| format!("`{field}` does not apply to a {}", kind.label());
        if kind.is_file() {
            entry.file = Some(
                self.file
                    .ok_or_else(|| format!("a {} entry names its `file`", kind.label()))?,
            );
            if self.secret.is_some() {
                return Err(fail("secret"));
            }
            if self.config.is_some() {
                return Err(fail("config"));
            }
            if !self.items.is_empty() {
                return Err(fail("items"));
            }
        } else {
            if self.file.is_some() {
                return Err(fail("file"));
            }
            let written = match kind {
                ResourceKind::Secret => self.secret,
                _ => self.config,
            };
            entry.name = Some(written.or(self.name.clone()).ok_or_else(|| {
                format!(
                    "a {k} entry names its {k}: `{k}: nomad/jobs/NAME` (a Nomad variable path) or `{k}: NAME` (a Kubernetes {})",
                    if kind == ResourceKind::Secret { "Secret" } else { "ConfigMap" },
                    k = kind.as_str(),
                )
            })?);
            entry.items = self.items;
        }
        if kind == ResourceKind::Policy {
            entry.name = self.name;
        } else if self.name.is_some() && kind.is_file() {
            return Err(fail("name"));
        }
        if !self.ignore.is_empty() {
            if !kind.supports_ignore() {
                return Err(format!(
                    "`ignore` does not apply to a {}: only jobs, namespaces, node pools, quotas and manifests are diffed field by field",
                    kind.label()
                ));
            }
            entry.ignore = self.ignore;
        }
        if !self.live_vars.is_empty() {
            if kind != ResourceKind::Job {
                return Err(fail("live_vars"));
            }
            entry.live_vars = self.live_vars;
        }
        if !self.files.is_empty() {
            entry.files = match (kind, self.files) {
                (ResourceKind::Job | ResourceKind::Config, RawFiles::Map(map)) => map,
                (ResourceKind::Config, RawFiles::List(list)) => list
                    .into_iter()
                    .map(|file| {
                        let name = file.strip_suffix(".j2").unwrap_or(&file).to_owned();
                        (name, file)
                    })
                    .collect(),
                (ResourceKind::Job, RawFiles::List(_)) => {
                    return Err("a job's `files` map variable names to files: `files: { app_config: config.yaml }`, read as `var.app_config`".to_owned());
                }
                _ => return Err(fail("files")),
            };
            if let Some(name) = entry.files.keys().find(|k| entry.items.contains_key(*k)) {
                return Err(format!(
                    "the item '{name}' is both in `items` and in `files`"
                ));
            }
        }
        if self.attach.is_some() {
            if kind != ResourceKind::Policy {
                return Err(fail("attach"));
            }
            entry.attach = self.attach;
        }
        if kind == ResourceKind::Secret {
            entry.secret_type = self.ty;
        }
        Ok(entry)
    }
}

impl<'de> Deserialize<'de> for ResourceEntry {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let entry = match RawEntry::deserialize(d)? {
            RawEntry::File(file) => ResourceEntry::file(file),
            RawEntry::Full(map) => map.resolve(),
        };
        entry.map_err(serde::de::Error::custom)
    }
}

impl Serialize for ResourceEntry {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut raw = RawEntryMap {
            kind: Some(self.kind.as_str().to_owned()),
            file: self.file.clone(),
            items: self.items.clone(),
            ignore: self.ignore.clone(),
            live_vars: self.live_vars.clone(),
            files: RawFiles::Map(self.files.clone()),
            attach: self.attach.clone(),
            ty: self.secret_type.clone(),
            ..RawEntryMap::default()
        };
        match self.kind {
            ResourceKind::Secret => raw.secret = self.name.clone(),
            ResourceKind::Config => raw.config = self.name.clone(),
            _ => raw.name = self.name.clone(),
        }
        raw.serialize(s)
    }
}

/// How long an apply waits on the scheduler for a workload's objects.
/// Scheduler-neutral: each executor maps them onto what its scheduler
/// reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadTimeouts {
    /// For a placement the scheduler cannot make yet (Nomad's blocked
    /// evaluation, say a static port the old allocation still holds),
    /// before the apply fails.
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub placement: Option<std::time::Duration>,
    /// For a rollout to finish (Nomad's deployment) once placed. The
    /// scheduler's own deadlines (`progress_deadline`) usually end it first.
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub rollout: Option<std::time::Duration>,
}

/// The task kinds a workload's `actions:` may use.
pub const WORKLOAD_ACTION_KINDS: &[&str] = &["dispatch"];

/// An action every workload with a job offers without declaring it. The
/// names are scheduler-neutral: on Nomad, `restart` restarts the jobs'
/// allocations, `stop` stops the jobs and `revert` re-registers each job's
/// last stable version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImplicitAction {
    pub name: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub confirm: bool,
}

/// The implicit actions, in the order the UI lists them.
pub const IMPLICIT_ACTIONS: &[ImplicitAction] = &[
    ImplicitAction {
        name: "restart",
        label: "Restart",
        description: "Restart the running allocations of the workload's jobs in place.",
        confirm: false,
    },
    ImplicitAction {
        name: "stop",
        label: "Stop",
        description: "Stop the workload's jobs; their volumes and variables stay. The next rollout that changes the workload starts them again.",
        confirm: true,
    },
    ImplicitAction {
        name: "revert",
        label: "Roll back",
        description: "Re-register each job's newest stable version older than the running one.",
        confirm: true,
    },
];

/// `dispatch:`, the workload action kind that dispatches a parameterized
/// job with fixed meta (and payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispatchSpec {
    /// The parameterized job's id.
    pub job: String,
    /// The job's namespace; absent: the workload's, else `default`.
    #[serde(default)]
    pub namespace: Option<String>,
    #[serde(default)]
    pub meta: IndexMap<String, String>,
    /// The dispatch payload, as text.
    #[serde(default)]
    pub payload: Option<String>,
}

/// `workload.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawWorkload")]
pub struct WorkloadConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Selects the cluster targets this workload applies to, over `cluster.*`.
    /// Absent means every cluster.
    #[serde(default)]
    pub when: Option<Filter>,
    /// Workloads (bare, or `workload:NAME`) and modules (`module:NAME`) that
    /// must apply before this one.
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Labels of the workload's placements (`workload.labels.*` in phase
    /// selectors and groups): scalars, kept in the repository only.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub labels: IndexMap<String, serde_yaml::Value>,
    /// Defaults; fleet vars override. On Nomad they become the job's HCL2
    /// variables.
    #[serde(default)]
    pub vars: IndexMap<String, serde_yaml::Value>,
    /// Files passed to every job as string variables, by variable name.
    #[serde(default)]
    pub files: IndexMap<String, String>,
    /// The objects the workload consists of, in the order they apply.
    #[serde(default)]
    pub resources: Vec<ResourceEntry>,
    /// The namespace the workload's entities live in; created if missing.
    /// On Nomad it is the namespace of the variables (and of a job whose
    /// spec names none); on Kubernetes the one objects without a namespace
    /// land in. Absent: the scheduler's default namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Extra checks beside the implicit ones (each job's deployment and
    /// allocations; a Deployment's rollout and pods).
    #[serde(default)]
    pub health: Vec<HealthDecl>,
    /// How long a rollout watches the cluster after a run that changed this
    /// workload; the phase's `bake` is the minimum.
    #[serde(default, with = "humantime_serde")]
    pub bake: Option<std::time::Duration>,
    /// How long an apply waits on the scheduler; absent: the executor's
    /// defaults.
    #[serde(default)]
    pub timeouts: WorkloadTimeouts,
    /// Entries (by file or secret name) whose change restarts the jobs even
    /// when their specs did not change.
    #[serde(default)]
    pub restart_on: Vec<String>,
    #[serde(default)]
    pub removal: WorkloadRemoval,
    /// Extra actions beside the implicit ones (Restart, Stop and Roll back
    /// on every job), in a host module's shape; their kinds are the
    /// workload action kinds (`dispatch`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub actions: IndexMap<String, super::ActionDecl>,
    /// The former per-kind keys the file used (`job`, `volumes`, …), for the
    /// deprecation warning. They are folded into `resources`.
    #[serde(skip)]
    pub legacy_keys: Vec<&'static str>,
}

/// `workload.yaml` as written, with the per-kind keys of the first schema
/// (`job`, `volumes`, `variables`, `policies`, `namespaces`, `node_pools`,
/// `manifests`, `secrets`) accepted for one release and folded into
/// `resources` in the order the executor used to apply them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkload {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    when: Option<Filter>,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    labels: IndexMap<String, serde_yaml::Value>,
    #[serde(default)]
    vars: IndexMap<String, serde_yaml::Value>,
    #[serde(default)]
    files: IndexMap<String, String>,
    #[serde(default)]
    resources: Vec<ResourceEntry>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    health: Vec<HealthDecl>,
    #[serde(default, with = "humantime_serde")]
    bake: Option<std::time::Duration>,
    #[serde(default)]
    timeouts: WorkloadTimeouts,
    #[serde(default)]
    restart_on: Vec<String>,
    #[serde(default)]
    removal: WorkloadRemoval,
    #[serde(default)]
    actions: IndexMap<String, super::ActionDecl>,

    // ---- the first schema's keys
    #[serde(default)]
    job: Option<LegacyJob>,
    #[serde(default)]
    volumes: Vec<String>,
    #[serde(default)]
    variables: IndexMap<String, Items>,
    #[serde(default)]
    policies: Vec<LegacyPolicy>,
    #[serde(default)]
    namespaces: Vec<String>,
    #[serde(default)]
    node_pools: Vec<String>,
    #[serde(default)]
    manifests: Vec<String>,
    #[serde(default)]
    secrets: IndexMap<String, Items>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyJob {
    File(String),
    Full {
        file: String,
        #[serde(default)]
        ignore: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyPolicy {
    File(String),
    Full {
        file: String,
        #[serde(default)]
        attach: Option<PolicyAttach>,
    },
}

impl TryFrom<RawWorkload> for WorkloadConfig {
    type Error = String;

    fn try_from(raw: RawWorkload) -> Result<Self, String> {
        let mut legacy_keys = Vec::new();
        let mut resources = Vec::new();
        let mut legacy = |key: &'static str, entries: Vec<ResourceEntry>| {
            if !entries.is_empty() {
                legacy_keys.push(key);
                resources.extend(entries);
            }
        };
        let files = |kind: ResourceKind, files: Vec<String>| -> Vec<ResourceEntry> {
            files
                .into_iter()
                .map(|f| ResourceEntry::of(kind, f))
                .collect()
        };
        // The order the first executor applied the keys in.
        legacy("namespaces", files(ResourceKind::Namespace, raw.namespaces));
        legacy("node_pools", files(ResourceKind::NodePool, raw.node_pools));
        legacy("volumes", files(ResourceKind::Volume, raw.volumes));
        legacy(
            "variables",
            raw.variables
                .into_iter()
                .map(|(path, items)| ResourceEntry::secret(path, items))
                .collect(),
        );
        legacy(
            "policies",
            raw.policies
                .into_iter()
                .map(|p| match p {
                    LegacyPolicy::File(file) => ResourceEntry::of(ResourceKind::Policy, file),
                    LegacyPolicy::Full { file, attach } => ResourceEntry {
                        attach,
                        ..ResourceEntry::of(ResourceKind::Policy, file)
                    },
                })
                .collect(),
        );
        legacy(
            "job",
            raw.job
                .into_iter()
                .map(|j| match j {
                    LegacyJob::File(file) => ResourceEntry::of(ResourceKind::Job, file),
                    LegacyJob::Full { file, ignore } => ResourceEntry {
                        ignore,
                        ..ResourceEntry::of(ResourceKind::Job, file)
                    },
                })
                .collect(),
        );
        legacy(
            "secrets",
            raw.secrets
                .into_iter()
                .map(|(name, items)| ResourceEntry::secret(name, items))
                .collect(),
        );
        legacy("manifests", files(ResourceKind::Manifest, raw.manifests));
        resources.extend(raw.resources);
        Ok(WorkloadConfig {
            name: raw.name,
            description: raw.description,
            when: raw.when,
            needs: raw.needs,
            tags: raw.tags,
            labels: raw.labels,
            vars: raw.vars,
            files: raw.files,
            resources,
            namespace: raw.namespace,
            health: raw.health,
            bake: raw.bake,
            timeouts: raw.timeouts,
            restart_on: raw.restart_on,
            removal: raw.removal,
            actions: raw.actions,
            legacy_keys,
        })
    }
}

impl WorkloadConfig {
    /// The placement labels as selector values (`workload.labels.*`).
    pub fn selector_labels(&self) -> crate::selectors::Labels {
        self.labels
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), serde_json::to_value(v).ok()?)))
            .collect()
    }

    pub fn parse(yaml: &str) -> crate::Result<Self> {
        use crate::error::HumanizableResult;
        serde_yaml::from_str(yaml).to_human_error()
    }

    /// The `needs` edges, parsed.
    pub fn needs(&self) -> Vec<Need> {
        self.needs
            .iter()
            .map(|n| Need::parse(n, Layer::Workload))
            .collect()
    }

    /// The actions the workload offers without declaring them: none for a
    /// workload without a job.
    pub fn implicit_actions(&self) -> &'static [ImplicitAction] {
        if self.jobs().next().is_some() {
            IMPLICIT_ACTIONS
        } else {
            &[]
        }
    }

    /// The job entries, in order.
    pub fn jobs(&self) -> impl Iterator<Item = &ResourceEntry> {
        self.resources
            .iter()
            .filter(|r| r.kind == ResourceKind::Job)
    }

    /// The entries of `kind`, in order.
    pub fn of_kind(&self, kind: ResourceKind) -> impl Iterator<Item = &ResourceEntry> {
        self.resources.iter().filter(move |r| r.kind == kind)
    }

    /// What `restart_on` may name: each entry's id.
    pub fn resource_ids(&self) -> Vec<&str> {
        self.resources.iter().map(ResourceEntry::id).collect()
    }

    /// Which scheduler the workload's resources belong to: `Some(kind)` when
    /// one of them belongs to one scheduler only, `None` when none does (an
    /// empty workload, or secrets alone, which either applies) or they mix
    /// the two (validation rejects that).
    pub fn cluster_kind(&self) -> Option<ClusterKind> {
        match (self.uses_nomad_keys(), self.uses_kubernetes_keys()) {
            (true, false) => Some(ClusterKind::Nomad),
            (false, true) => Some(ClusterKind::Kubernetes),
            _ => None,
        }
    }

    /// Whether the workload lists a resource only Nomad has.
    pub fn uses_nomad_keys(&self) -> bool {
        self.resources
            .iter()
            .any(|r| r.kind.scheduler() == Some(ClusterKind::Nomad))
    }

    /// Whether the workload lists a resource only Kubernetes has.
    pub fn uses_kubernetes_keys(&self) -> bool {
        self.resources
            .iter()
            .any(|r| r.kind.scheduler() == Some(ClusterKind::Kubernetes))
    }

    /// Every file under `files/` the workload references, with the kind that
    /// names it.
    pub fn referenced_files(&self) -> Vec<(ResourceKind, &str)> {
        let mut out: Vec<(ResourceKind, &str)> = Vec::new();
        for r in &self.resources {
            if let Some(file) = &r.file {
                out.push((r.kind, file.as_str()));
            }
            out.extend(r.files.values().map(|f| (r.kind, f.as_str())));
        }
        out.extend(self.files.values().map(|f| (ResourceKind::Job, f.as_str())));
        out
    }

    /// The files the scheduler stores in clear (job specs, manifests), which
    /// must not carry secret values.
    pub fn spec_files(&self) -> Vec<&str> {
        self.resources
            .iter()
            .filter(|r| r.kind.runs())
            .filter_map(|r| r.file.as_deref())
            .collect()
    }

    /// Entries that are declared twice (same kind and id).
    pub fn duplicates(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for r in &self.resources {
            if !seen.insert((r.kind, r.id().to_owned())) {
                out.push(r.describe());
            }
        }
        out
    }

    /// Items set twice within the entries of one kind and name (two
    /// `config` entries for one ConfigMap, say). A `secret` and a `config`
    /// of the same name are different objects on Kubernetes and one Nomad
    /// variable, so the Nomad executor checks their items against each other
    /// when it merges them.
    pub fn item_collisions(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for r in self.resources.iter().filter(|r| r.kind.is_items()) {
            for item in r.item_names() {
                if !seen.insert((r.kind, r.id(), item)) {
                    out.push(format!("sets the item '{item}' of {} twice", r.describe()));
                }
            }
        }
        out
    }

    /// The first job or manifest that precedes an entity of the same
    /// workload, if any: almost always the other way round was meant.
    pub fn ordering_problem(&self) -> Option<String> {
        let first_run = self.resources.iter().position(|r| r.kind.runs())?;
        let later_entity = self.resources[first_run + 1..]
            .iter()
            .find(|r| !r.kind.runs())?;
        Some(format!(
            "lists {} before {}; resources apply in list order, so the {} should usually come first",
            self.resources[first_run].describe(),
            later_entity.describe(),
            later_entity.kind,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::time::Duration;

    const NOMAD: &str = r#"
name: atuin
description: Atuin shell-history sync server.
when: 'cluster.groups contains "home-cluster"'
needs: [postgresql, module:nomad]
vars:
  atuin_version: "18.6.1"
resources:
  - secret: nomad/jobs/atuin
    items:
      DATABASE_URL: 'postgres://atuin:{{ secrets.ATUIN_DB_PASSWORD }}@db/atuin'
      ATUIN_OPEN_REGISTRATION: "false"
  - { file: atuin.policy.hcl, attach: { job: atuin } }
  - anonymous.policy.hcl
  - { file: atuin.nomad.hcl, live_vars: [atuin_version], ignore: ['Meta[autoupdate.*]'], files: { config: config.toml } }
  - atuin-backup.job.hcl
health:
  - name: api
    http: { url: 'http://atuin.example/', expect: { status: 200 } }
    required: false
"#;

    #[test]
    fn a_nomad_workload_parses_in_list_order() {
        let w = WorkloadConfig::parse(NOMAD).expect("workload parses");
        assert_eq!(
            w.needs(),
            vec![
                Need::Workload("postgresql".into()),
                Need::Module("nomad".into())
            ]
        );
        let kinds: Vec<ResourceKind> = w.resources.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ResourceKind::Secret,
                ResourceKind::Policy,
                ResourceKind::Policy,
                ResourceKind::Job,
                ResourceKind::Job
            ]
        );
        assert_eq!(w.resources[0].id(), "nomad/jobs/atuin");
        assert_eq!(w.resources[0].items.len(), 2);
        assert_eq!(w.resources[1].attach.as_ref().unwrap().job, "atuin");
        assert!(w.resources[2].attach.is_none());
        let job = &w.resources[3];
        assert_eq!(job.live_vars, vec!["atuin_version"]);
        assert_eq!(job.ignore, vec!["Meta[autoupdate.*]"]);
        assert_eq!(job.files["config"], "config.toml");
        assert_eq!(w.jobs().count(), 2);
        assert_eq!(w.cluster_kind(), Some(ClusterKind::Nomad));
        assert_eq!(w.health[0].display_name(), "api");
        assert!(!w.health[0].required);
        assert_eq!(w.removal, WorkloadRemoval::Stop);
        assert!(w.legacy_keys.is_empty());
        let files: Vec<&str> = w.referenced_files().into_iter().map(|(_, f)| f).collect();
        assert_eq!(
            files,
            vec![
                "atuin.policy.hcl",
                "anonymous.policy.hcl",
                "atuin.nomad.hcl",
                "config.toml",
                "atuin-backup.job.hcl"
            ]
        );
        assert_eq!(
            w.spec_files(),
            vec!["atuin.nomad.hcl", "atuin-backup.job.hcl"]
        );
        assert!(w.ordering_problem().is_none());
        assert!(w.duplicates().is_empty());
    }

    #[test]
    fn a_kubernetes_workload_parses() {
        let w = WorkloadConfig::parse(
            r#"
name: grafana
when: 'cluster.kind == "kubernetes"'
namespace: monitoring
resources:
  - secret: grafana-admin
    type: Opaque
    items: { admin-password: '{{ secrets.GRAFANA_ADMIN_PASSWORD }}' }
  - grafana.yaml.j2
  - { file: web.json, ignore: [spec.replicas] }
removal: purge
"#,
        )
        .unwrap();
        let kinds: Vec<ResourceKind> = w.resources.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ResourceKind::Secret,
                ResourceKind::Manifest,
                ResourceKind::Manifest
            ]
        );
        assert_eq!(w.resources[0].secret_type.as_deref(), Some("Opaque"));
        assert_eq!(w.cluster_kind(), Some(ClusterKind::Kubernetes));
        assert_eq!(w.removal, WorkloadRemoval::Purge);
    }

    #[test]
    fn the_first_schemas_keys_fold_into_resources_in_the_old_apply_order() {
        let w = WorkloadConfig::parse(
            "name: paste\njob: { file: paste.nomad.hcl, ignore: ['Meta[autoupdate.*]'] }\nvolumes: [paste.volume.hcl]\nvariables:\n  nomad/jobs/paste: { A: '1' }\npolicies:\n  - { file: paste.policy.hcl, attach: { job: paste } }\nnamespaces: [apps.namespace.hcl]\nresources:\n  - { type: quota, file: team.quota.hcl }\n",
        )
        .unwrap();
        let described: Vec<String> = w.resources.iter().map(ResourceEntry::describe).collect();
        assert_eq!(
            described,
            vec![
                "namespace apps.namespace.hcl",
                "volume paste.volume.hcl",
                "secret nomad/jobs/paste",
                "policy paste.policy.hcl",
                "job paste.nomad.hcl",
                "quota team.quota.hcl",
            ]
        );
        assert_eq!(w.resources[4].ignore, vec!["Meta[autoupdate.*]"]);
        assert_eq!(w.resources[3].attach.as_ref().unwrap().job, "paste");
        assert_eq!(
            w.legacy_keys,
            vec!["namespaces", "volumes", "variables", "policies", "job"]
        );
    }

    #[test]
    fn an_entity_only_workload_parses_with_explicit_kinds() {
        let w = WorkloadConfig::parse(
            "name: platform\nresources:\n  - team-a.namespace.hcl\n  - pis.pool.hcl\n  - { kind: quota, file: team-a.hcl }\nremoval: keep\n",
        )
        .unwrap();
        let kinds: Vec<ResourceKind> = w.resources.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                ResourceKind::Namespace,
                ResourceKind::NodePool,
                ResourceKind::Quota
            ]
        );
        assert_eq!(w.removal, WorkloadRemoval::Keep);
    }

    #[test]
    fn mixed_or_empty_workloads_have_no_single_kind() {
        let mixed = WorkloadConfig::parse("name: x\nresources: [x.nomad.hcl, x.yaml]\n").unwrap();
        assert_eq!(mixed.cluster_kind(), None);
        assert!(mixed.uses_nomad_keys() && mixed.uses_kubernetes_keys());
        let empty = WorkloadConfig::parse("name: x\n").unwrap();
        assert_eq!(empty.cluster_kind(), None);
        assert!(empty.resources.is_empty());
        // Secrets are written to either scheduler's store.
        let secrets =
            WorkloadConfig::parse("name: x\nresources: [{ secret: s, items: { a: b } }]\n")
                .unwrap();
        assert_eq!(secrets.cluster_kind(), None);
        assert!(!secrets.uses_nomad_keys() && !secrets.uses_kubernetes_keys());
        assert_eq!(secrets.resources[0].kind, ResourceKind::Secret);
    }

    #[test]
    fn a_config_lists_its_files_as_items_and_may_share_a_secrets_name() {
        let w = WorkloadConfig::parse(
            "name: x\nresources:\n  - { secret: nomad/jobs/x, items: { token: '{{ secrets.T }}' } }\n  - { config: nomad/jobs/x, files: [config.yaml, extra.toml.j2], items: { mode: fast } }\n  - { config: other, files: { app.conf: app.conf.j2 } }\n  - x.nomad.hcl\n",
        )
        .unwrap();
        let config = &w.resources[1];
        assert_eq!(config.kind, ResourceKind::Config);
        assert_eq!(config.id(), "nomad/jobs/x");
        assert_eq!(config.describe(), "config nomad/jobs/x");
        assert_eq!(
            config.item_names().collect::<Vec<_>>(),
            vec!["mode", "config.yaml", "extra.toml"]
        );
        assert_eq!(config.files["extra.toml"], "extra.toml.j2");
        assert_eq!(w.resources[2].files["app.conf"], "app.conf.j2");
        assert!(w.duplicates().is_empty());
        assert!(w.item_collisions().is_empty());
        assert_eq!(w.cluster_kind(), Some(ClusterKind::Nomad));
        let files: Vec<(ResourceKind, &str)> = w.referenced_files();
        assert!(files.contains(&(ResourceKind::Config, "config.yaml")));
        assert!(files.contains(&(ResourceKind::Config, "app.conf.j2")));
        assert!(w.resource_ids().contains(&"other"));
        // Serialising keeps the entry a config.
        let yaml = serde_yaml::to_string(&w).unwrap();
        assert!(yaml.contains("config: nomad/jobs/x"), "{yaml}");
        let again = WorkloadConfig::parse(&yaml).unwrap();
        assert_eq!(again.resources, w.resources);
    }

    #[test]
    fn an_item_set_twice_within_one_config_is_a_collision() {
        let w = WorkloadConfig::parse(
            "name: x\nresources:\n  - { config: c, files: [a.yaml] }\n  - { config: c, items: { a.yaml: text } }\n  - { secret: c, items: { a.yaml: 's' } }\n",
        )
        .unwrap();
        assert_eq!(
            w.item_collisions(),
            vec!["sets the item 'a.yaml' of config c twice"]
        );
    }

    #[test]
    fn ordering_and_duplicates_are_pointed_out() {
        let w =
            WorkloadConfig::parse("name: x\nresources: [x.nomad.hcl, x.volume.hcl, x.nomad.hcl]\n")
                .unwrap();
        let problem = w.ordering_problem().unwrap();
        assert!(
            problem.contains("job x.nomad.hcl before volume x.volume.hcl"),
            "{problem}"
        );
        assert_eq!(w.duplicates(), vec!["job x.nomad.hcl"]);
        // A policy attached to a job may follow it.
        let w = WorkloadConfig::parse("name: x\nresources: [x.volume.hcl, x.nomad.hcl]\n").unwrap();
        assert!(w.ordering_problem().is_none());
    }

    #[rstest]
    #[case::tasks("name: x\ntasks: []\n", "unknown field")]
    #[case::handlers("name: x\nhandlers: {}\n", "unknown field")]
    #[case::facts("name: x\nfacts: { provides: [a] }\n", "unknown field")]
    #[case::removal("name: x\nremoval: delete\n", "unknown variant")]
    #[case::unknown_suffix("name: x\nresources: [x.hcl]\n", "does not say what it is")]
    #[case::unknown_kind(
        "name: x\nresources: [{ kind: deployment, file: x.yaml }]\n",
        "not a resource kind"
    )]
    #[case::attach_on_a_job(
        "name: x\nresources: [{ file: x.nomad.hcl, attach: { job: a } }]\n",
        "does not apply to a job"
    )]
    #[case::job_files_as_a_list(
        "name: x\nresources: [{ file: x.nomad.hcl, files: [config.yaml] }]\n",
        "map variable names to files"
    )]
    #[case::config_and_secret("name: x\nresources: [{ config: c, secret: s }]\n", "not both")]
    #[case::config_item_twice(
        "name: x\nresources: [{ config: c, files: [a.yaml], items: { a.yaml: x } }]\n",
        "both in `items` and in `files`"
    )]
    #[case::config_with_a_file(
        "name: x\nresources: [{ config: c, file: a.yaml }]\n",
        "not one and another"
    )]
    #[case::files_on_a_volume(
        "name: x\nresources: [{ file: x.volume.hcl, files: { a: a.yaml } }]\n",
        "does not apply to a volume"
    )]
    #[case::live_vars_on_a_volume(
        "name: x\nresources: [{ file: x.volume.hcl, live_vars: [v] }]\n",
        "does not apply to a volume"
    )]
    #[case::ignore_on_a_volume(
        "name: x\nresources: [{ file: x.volume.hcl, ignore: [Name] }]\n",
        "does not apply to a volume"
    )]
    #[case::items_on_a_file(
        "name: x\nresources: [{ file: x.nomad.hcl, items: { a: b } }]\n",
        "does not apply to a job"
    )]
    #[case::nothing_named("name: x\nresources: [{ kind: job }]\n", "names its `file`")]
    #[case::two_things(
        "name: x\nresources: [{ file: x.nomad.hcl, secret: p }]\n",
        "not one and another"
    )]
    #[case::empty_map(
        "name: x\nresources: [{ ignore: [a] }]\n",
        "names a `file`, a `secret` or a `config`"
    )]
    #[case::legacy_policy_without_file("name: x\npolicies: [{ attach: { job: a } }]\n", "")]
    fn bad_workloads_are_rejected(#[case] yaml: &str, #[case] needle: &str) {
        let err = WorkloadConfig::parse(yaml).expect_err(yaml);
        let text = format!("{} {}", err.message(), err.description());
        assert!(text.contains(needle), "{yaml}: {text}");
    }

    #[test]
    fn workloads_round_trip_through_serde() {
        let w = WorkloadConfig::parse(NOMAD).unwrap();
        let yaml = serde_yaml::to_string(&w).unwrap();
        let again = WorkloadConfig::parse(&yaml).unwrap();
        assert_eq!(again.resources, w.resources);
        assert_eq!(again.needs(), w.needs());
    }

    #[rstest]
    #[case::absent("", WorkloadTimeouts::default())]
    #[case::placement(
        "timeouts: { placement: 10m }\n",
        WorkloadTimeouts { placement: Some(Duration::from_secs(600)), rollout: None }
    )]
    #[case::both(
        "timeouts: { placement: 30s, rollout: 2h }\n",
        WorkloadTimeouts {
            placement: Some(Duration::from_secs(30)),
            rollout: Some(Duration::from_secs(7200)),
        }
    )]
    fn timeouts_are_optional_durations(#[case] yaml: &str, #[case] expected: WorkloadTimeouts) {
        let w = WorkloadConfig::parse(&format!("name: paste\n{yaml}")).unwrap();
        assert_eq!(w.timeouts, expected);
    }

    #[test]
    fn unknown_timeouts_are_refused() {
        assert!(WorkloadConfig::parse("name: paste\ntimeouts: { plaecment: 10m }\n").is_err());
    }

    #[rstest]
    #[case("paste.nomad.hcl", Some(ResourceKind::Job))]
    #[case("paste.job.hcl.j2", Some(ResourceKind::Job))]
    #[case("data.volume.hcl", Some(ResourceKind::Volume))]
    #[case("pis.pool.hcl", Some(ResourceKind::NodePool))]
    #[case("grafana.yaml.j2", Some(ResourceKind::Manifest))]
    #[case("grafana.yml", Some(ResourceKind::Manifest))]
    #[case("paste.hcl", None)]
    #[case("config.toml", None)]
    fn kinds_come_from_file_suffixes(#[case] file: &str, #[case] kind: Option<ResourceKind>) {
        assert_eq!(ResourceKind::from_file(file), kind);
    }

    #[rstest]
    #[case("nomad", Layer::Workload, Need::Workload("nomad".into()))]
    #[case("module:nomad", Layer::Workload, Need::Module("nomad".into()))]
    #[case("workload: postgresql", Layer::Module, Need::Workload("postgresql".into()))]
    #[case("docker", Layer::Module, Need::Module("docker".into()))]
    fn needs_resolve_their_layer(#[case] raw: &str, #[case] layer: Layer, #[case] expected: Need) {
        let need = Need::parse(raw, layer);
        assert_eq!(need, expected);
        assert_eq!(Need::parse(&need.qualified(), layer), expected);
    }
}
