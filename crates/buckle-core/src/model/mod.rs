//! The configuration model: `buckle.yaml` (fleet), `module.yaml` (module)
//! and `workload.yaml` (workload).
//!
//! Everything here is plain data with serde derives. Selectors are
//! [`filt_rs::Filter`]s; string fields that may contain `{{ }}` are kept as raw
//! strings and rendered by the engine with the host's context.

mod action_decl;
mod cluster;
mod task_decl;
mod workload;

pub use action_decl::ActionDecl;
pub use cluster::{
    ClusterDecl, ClusterIdentity, ClusterKind, ClusterScope, ClusterTrust, trust_problem,
    yaml_scalar_text,
};
pub use task_decl::{RetryPolicy, TaskDecl};
pub use workload::{
    DispatchSpec, IMPLICIT_ACTIONS, ImplicitAction, Items, Layer, Need, PolicyAttach,
    ResourceEntry, ResourceKind, WORKLOAD_ACTION_KINDS, WorkloadConfig, WorkloadRemoval,
    WorkloadTimeouts,
};

use crate::Filter;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// `buckle.yaml` at the root of a configuration repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetConfig {
    /// Configuration schema version. v2 is the only supported value.
    pub version: u32,
    #[serde(default)]
    pub fleet: FleetSection,
    /// Variable overlays applied in order; later entries win. An omitted list is
    /// the empty set.
    #[serde(default)]
    pub vars: Vec<VarsOverlay>,
    /// Secret references by name. Values are resolved by a provider, never stored here.
    #[serde(default)]
    pub secrets: IndexMap<String, SecretRef>,
    #[serde(default)]
    pub rollout: RolloutPolicy,
}

impl FleetConfig {
    pub fn parse(yaml: &str) -> crate::Result<Self> {
        use crate::error::HumanizableResult;
        let config: FleetConfig = serde_yaml::from_str(yaml).to_human_error()?;
        if config.version != 2 {
            return Err(crate::error::user(
                format!(
                    "This configuration declares version {}, but this Buckle only understands version 2.",
                    config.version
                ),
                &[
                    "Set `version: 2` at the top of buckle.yaml.",
                    "Use `buckle migrate` to convert a Buckle 1.x package directory.",
                ],
            ));
        }
        Ok(config)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetSection {
    /// Derived groups: name → selector over a target's labels and facts
    /// (`host.*` for hosts, `cluster.*` for clusters).
    #[serde(default)]
    pub groups: IndexMap<String, Filter>,
    /// The clusters this repository declares as targets, by name.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub clusters: IndexMap<String, ClusterDecl>,
}

/// One entry in the `vars` list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VarsOverlay {
    /// Applies to every host when absent.
    #[serde(default)]
    pub when: Option<Filter>,
    /// A YAML file of values, relative to the repository root.
    #[serde(default)]
    pub file: Option<PathBuf>,
    /// Inline values.
    #[serde(default)]
    pub values: IndexMap<String, serde_yaml::Value>,
}

/// Where a secret's value comes from. Written as a one-key map in YAML:
/// `{ env: NAME }`, `{ file: /path }` or `{ exec: [prog, args…] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum SecretRef {
    /// An environment variable of the resolving process.
    Env { env: String },
    /// A file whose contents are the value (one trailing newline trimmed).
    File { file: PathBuf },
    /// A command whose stdout is the value. Allowed only where server config lists the program.
    Exec { exec: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RolloutPolicy {
    #[serde(default)]
    pub trigger: RolloutTrigger,
    #[serde(default)]
    pub supersede: SupersedePolicy,
    #[serde(default)]
    pub on_failure: FailurePolicy,
    /// Flap tolerance: a required check must fail continuously this long before it counts.
    #[serde(default = "RolloutPolicy::default_settle", with = "humantime_serde")]
    pub settle: Duration,
    #[serde(default)]
    pub health: HealthPolicy,
    #[serde(default)]
    pub local_rollback: LocalRollbackPolicy,
    #[serde(default)]
    pub phases: Vec<PhasePolicy>,
    #[serde(default)]
    pub stragglers: StragglersPolicy,
}

impl RolloutPolicy {
    fn default_settle() -> Duration {
        Duration::from_secs(120)
    }
}

impl Default for RolloutPolicy {
    fn default() -> Self {
        Self {
            trigger: RolloutTrigger::default(),
            supersede: SupersedePolicy::default(),
            on_failure: FailurePolicy::default(),
            settle: Self::default_settle(),
            health: HealthPolicy::default(),
            local_rollback: LocalRollbackPolicy::default(),
            phases: Vec::new(),
            stragglers: StragglersPolicy::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RolloutTrigger {
    #[default]
    Auto,
    Manual,
}

/// What happens to a revision that arrives while a rollout is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SupersedePolicy {
    /// Finish the current rollout, then roll out only the newest queued
    /// revision; the queued ones in between are skipped. (`queue` is the
    /// older name.)
    #[default]
    #[serde(alias = "queue")]
    Latest,
    /// Finish the current rollout, then roll out every queued revision in
    /// the order it arrived.
    Sequential,
    /// Abort the current rollout and start the new one at once.
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailurePolicy {
    #[default]
    Pause,
    Rollback,
    Continue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthPolicy {
    /// Default bake duration per phase.
    #[serde(default = "HealthPolicy::default_bake", with = "humantime_serde")]
    pub bake: Duration,
    #[serde(default)]
    pub baseline: BaselinePolicy,
}

impl HealthPolicy {
    fn default_bake() -> Duration {
        Duration::from_secs(300)
    }
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            bake: Self::default_bake(),
            baseline: BaselinePolicy::default(),
        }
    }
}

/// What to do with a host whose pre-apply baseline health is failing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BaselinePolicy {
    /// Wait for an operator to force or skip the host.
    #[default]
    Hold,
    /// Exclude the host from this rollout.
    Skip,
    /// Apply anyway.
    Ignore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalRollbackPolicy {
    #[serde(default = "LocalRollbackPolicy::default_enabled")]
    pub enabled: bool,
    /// Connected before apply, no session for this long after it → roll back.
    #[serde(
        default = "LocalRollbackPolicy::default_after",
        with = "humantime_serde"
    )]
    pub after: Duration,
    /// Required checks failing past `settle` while disconnected also trigger it.
    #[serde(default = "LocalRollbackPolicy::default_enabled")]
    pub on_health: bool,
}

impl LocalRollbackPolicy {
    fn default_enabled() -> bool {
        true
    }
    fn default_after() -> Duration {
        Duration::from_secs(300)
    }
}

impl Default for LocalRollbackPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            after: Self::default_after(),
            on_health: true,
        }
    }
}

/// Parallelism within a phase: an absolute count or a percentage of the phase's hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parallelism {
    Count(u32),
    Percent(u8),
}

impl Parallelism {
    /// The number of hosts to run at once for a phase of `total` hosts; never zero for a non-empty phase.
    pub fn resolve(self, total: usize) -> usize {
        match self {
            Parallelism::Count(n) => (n as usize).max(1).min(total.max(1)),
            Parallelism::Percent(p) => {
                let n = (total as f64 * p as f64 / 100.0).ceil() as usize;
                n.max(1).min(total.max(1))
            }
        }
    }
}

impl Default for Parallelism {
    fn default() -> Self {
        Parallelism::Count(1)
    }
}

impl Serialize for Parallelism {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Parallelism::Count(n) => s.serialize_u32(*n),
            Parallelism::Percent(p) => s.serialize_str(&format!("{p}%")),
        }
    }
}

impl<'de> Deserialize<'de> for Parallelism {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Count(u32),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Count(n) => Ok(Parallelism::Count(n)),
            Raw::Text(t) => {
                let t = t.trim();
                if let Some(p) = t.strip_suffix('%') {
                    let p: u8 = p.trim().parse().map_err(|_| {
                        serde::de::Error::custom(format!(
                            "'{t}' is not a valid percentage (expected e.g. 50%)"
                        ))
                    })?;
                    if p == 0 || p > 100 {
                        return Err(serde::de::Error::custom(
                            "a percentage must be between 1% and 100%",
                        ));
                    }
                    Ok(Parallelism::Percent(p))
                } else {
                    t.parse::<u32>().map(Parallelism::Count).map_err(|_| {
                        serde::de::Error::custom(format!(
                            "'{t}' is not a valid parallelism (expected a count or a percentage)"
                        ))
                    })
                }
            }
        }
    }
}

/// How many failures a phase tolerates before pausing.
pub type FailureBudget = Parallelism;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    #[default]
    Auto,
    Required,
    /// Require approval only when the plan contains removals or other destructive steps.
    IfDestructive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhasePolicy {
    pub name: String,
    /// Hosts matching this selector (and no earlier phase) belong to the phase.
    pub select: Filter,
    #[serde(default)]
    pub parallel: Parallelism,
    #[serde(default)]
    pub approval: ApprovalPolicy,
    /// Overrides `rollout.health.bake` for this phase.
    #[serde(default, with = "humantime_serde")]
    pub bake: Option<Duration>,
    /// Failures tolerated before the phase pauses. Default: zero.
    #[serde(default = "PhasePolicy::default_max_failures")]
    pub max_failures: FailureBudget,
}

impl PhasePolicy {
    fn default_max_failures() -> FailureBudget {
        Parallelism::Count(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StragglersPolicy {
    #[serde(default = "StragglersPolicy::default_parallel")]
    pub parallel: Parallelism,
    #[serde(default)]
    pub approval: ApprovalPolicy,
}

impl StragglersPolicy {
    fn default_parallel() -> Parallelism {
        Parallelism::Count(1)
    }
}

impl Default for StragglersPolicy {
    fn default() -> Self {
        Self {
            parallel: Self::default_parallel(),
            approval: ApprovalPolicy::Auto,
        }
    }
}

/// `module.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Selects the hosts this module applies to. Absent means every host.
    #[serde(default)]
    pub when: Option<Filter>,
    /// Modules that must apply before this one (ordering and fact
    /// dependency), and `workload:NAME` for a workload on the cluster this
    /// host is a node of (resolved by the rollout controller, not here).
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Module defaults; fleet vars override.
    #[serde(default)]
    pub vars: IndexMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub tasks: Vec<TaskDecl>,
    /// Handlers are tasks run once at the end of the module when notified.
    #[serde(default)]
    pub handlers: IndexMap<String, TaskDecl>,
    /// Named tasks an operator runs on demand (from the UI or the API, or
    /// across many hosts as an action rollout), in the order the UI lists
    /// them: the first is the one on the Actions button's face.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub actions: IndexMap<String, ActionDecl>,
    #[serde(default)]
    pub health: Vec<HealthDecl>,
    /// How long a rollout watches a host after a run that changed this
    /// module. The phase's `bake` is the minimum; a run that changed nothing
    /// does not bake.
    #[serde(default, with = "humantime_serde")]
    pub bake: Option<Duration>,
    #[serde(default)]
    pub facts: ModuleFacts,
    #[serde(default)]
    pub removal: RemovalConfig,
}

impl ModuleConfig {
    pub fn parse(yaml: &str) -> crate::Result<Self> {
        use crate::error::HumanizableResult;
        serde_yaml::from_str(yaml).to_human_error()
    }

    /// The `needs` edges, parsed.
    pub fn needs(&self) -> Vec<Need> {
        self.needs
            .iter()
            .map(|n| Need::parse(n, Layer::Module))
            .collect()
    }

    /// The modules this module needs: its `needs` without the `workload:`
    /// edges, which order it against a cluster's workloads rather than
    /// against other modules on the host.
    pub fn module_needs(&self) -> Vec<String> {
        self.needs()
            .into_iter()
            .filter_map(|n| match n {
                Need::Module(name) => Some(name),
                Need::Workload(_) => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleFacts {
    /// Fact providers to re-run after this module applies; dependents are planned afterwards.
    #[serde(default)]
    pub provides: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemovalConfig {
    /// The module's default for its tasks. Precedence: a task's own `removal`,
    /// then this, then the task kind's default. Absent means "use the kind's
    /// default" (cleanup for every kind except handler-style kinds).
    #[serde(default)]
    pub policy: Option<RemovalPolicy>,
}

/// What happens to a resource when it leaves the desired set.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum RemovalPolicy {
    /// Run the task kind's `remove`.
    #[default]
    Cleanup,
    /// Forget the resource without touching the host.
    Leave,
}

/// A health check declaration. The check kind is the single non-reserved key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthDecl {
    #[serde(default)]
    pub name: Option<String>,
    /// How often the check repeats inside the bake window.
    #[serde(default = "HealthDecl::default_interval", with = "humantime_serde")]
    pub interval: Duration,
    #[serde(default = "HealthDecl::default_timeout", with = "humantime_serde")]
    pub timeout: Duration,
    /// A failing required check fails the run; an informational one only reports.
    #[serde(default = "HealthDecl::default_required")]
    pub required: bool,
    /// The check kind and its spec, e.g. `http: { url: ... }`.
    #[serde(flatten)]
    pub check: KindSpec,
}

impl HealthDecl {
    fn default_interval() -> Duration {
        Duration::from_secs(30)
    }
    fn default_timeout() -> Duration {
        Duration::from_secs(10)
    }
    fn default_required() -> bool {
        true
    }
    /// The display name: explicit `name`, else the kind.
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.check.kind)
    }
}

/// A `kind: spec` pair captured from a map that has exactly one non-reserved key.
#[derive(Debug, Clone, PartialEq)]
pub struct KindSpec {
    pub kind: String,
    pub spec: serde_yaml::Value,
}

impl Serialize for KindSpec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(1))?;
        map.serialize_entry(&self.kind, &self.spec)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for KindSpec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let map: IndexMap<String, serde_yaml::Value> = IndexMap::deserialize(d)?;
        let mut iter = map.into_iter();
        match (iter.next(), iter.next()) {
            (Some((kind, spec)), None) => Ok(KindSpec { kind, spec }),
            (None, _) => Err(serde::de::Error::custom(
                "expected exactly one kind key (e.g. `http: {...}`), found none",
            )),
            (Some((a, _)), Some((b, _))) => Err(serde::de::Error::custom(format!(
                "expected exactly one kind key, found both '{a}' and '{b}'"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const FLEET: &str = r#"
version: 2
fleet:
  groups:
    nomad: 'host.labels.role == "worker"'
    home-cluster: 'cluster.labels.cluster == "home"'
  clusters:
    home-nomad:
      kind: nomad
      api: http://127.0.0.1:4646
      nodes: 'host.groups contains "nomad"'
      labels: { cluster: home }
vars:
  - file: vars/defaults.yaml
  - when: 'host.groups contains "home"'
    values: { swap_mb: 1024 }
secrets:
  admin_password: { file: /secrets/admin_password }
  rustic_password: { env: RUSTIC_PASSWORD }
rollout:
  phases:
    - { name: canary, select: 'host.labels.canary == true', parallel: 1, approval: required, bake: 15m }
    - { name: rest, select: 'true', parallel: 50%, max_failures: 10% }
  stragglers: { parallel: 2 }
"#;

    #[test]
    fn fleet_config_parses_the_documented_shape() {
        let cfg = FleetConfig::parse(FLEET).expect("fleet config parses");
        assert_eq!(cfg.fleet.groups.len(), 2);
        let cluster = &cfg.fleet.clusters["home-nomad"];
        assert_eq!(cluster.kind, ClusterKind::Nomad);
        assert_eq!(cluster.api.as_deref(), Some("http://127.0.0.1:4646"));
        assert_eq!(cluster.identity, ClusterIdentity::BuckleJwt);
        assert_eq!(cluster.text_labels()["cluster"], "home");
        assert_eq!(cfg.vars.len(), 2);
        assert!(
            cfg.vars[0].when.is_none(),
            "an overlay without `when` applies everywhere"
        );
        assert_eq!(
            cfg.secrets["rustic_password"],
            SecretRef::Env {
                env: "RUSTIC_PASSWORD".into()
            }
        );
        assert_eq!(
            cfg.secrets["admin_password"],
            SecretRef::File {
                file: "/secrets/admin_password".into()
            }
        );
        assert_eq!(cfg.rollout.phases[0].approval, ApprovalPolicy::Required);
        assert_eq!(cfg.rollout.phases[0].bake, Some(Duration::from_secs(900)));
        assert_eq!(cfg.rollout.phases[1].parallel, Parallelism::Percent(50));
        assert_eq!(cfg.rollout.phases[1].max_failures, Parallelism::Percent(10));
        assert_eq!(cfg.rollout.settle, Duration::from_secs(120));
        assert_eq!(cfg.rollout.stragglers.parallel, Parallelism::Count(2));
    }

    #[test]
    fn fleet_config_rejects_other_versions() {
        let err = FleetConfig::parse("version: 1\n").expect_err("v1 is rejected");
        assert!(err.is(crate::error::Kind::User));
    }

    #[test]
    fn fleet_config_rejects_unknown_fields() {
        assert!(FleetConfig::parse("version: 2\nbogus: 1\n").is_err());
    }

    #[rstest]
    #[case(Parallelism::Count(1), 10, 1)]
    #[case(Parallelism::Count(0), 10, 1)]
    #[case(Parallelism::Count(50), 10, 10)]
    #[case(Parallelism::Percent(50), 5, 3)]
    #[case(Parallelism::Percent(10), 3, 1)]
    #[case(Parallelism::Percent(100), 7, 7)]
    #[case(Parallelism::Percent(50), 0, 1)]
    fn parallelism_resolves_to_at_least_one(
        #[case] p: Parallelism,
        #[case] total: usize,
        #[case] expected: usize,
    ) {
        assert_eq!(p.resolve(total), expected);
    }

    #[rstest]
    #[case("0%")]
    #[case("150%")]
    #[case("many")]
    fn parallelism_rejects_nonsense(#[case] raw: &str) {
        let yaml = format!("parallel: '{raw}'\nname: x\nselect: 'true'");
        assert!(serde_yaml::from_str::<PhasePolicy>(&yaml).is_err());
    }

    const MODULE: &str = r#"
name: nomad
when: 'host.groups contains "nomad"'
needs: [tailscale, docker]
vars: { nomad_version: "1.9.3" }
tasks:
  - apt.package: { name: nomad }
  - file.template:
      src: nomad.hcl.j2
      dest: /etc/nomad.d/nomad.hcl
    notify: [nomad]
    when: 'vars.render_config == true'
  - command.run: { run: ["true"] }
    removal: leave
    id: eligibility
    retry: { limit: 3, delay: 5s }
handlers:
  nomad: { systemd.restart: { name: nomad } }
actions:
  restart: { systemd.restart: { name: nomad }, description: Restart the Nomad agent }
  drain: { command.run: { run: [nomad, node, drain, -self, -enable] }, confirm: true }
health:
  - name: api
    http: { url: 'http://127.0.0.1:4646/v1/agent/health' }
    interval: 30s
  - systemd: { unit: nomad }
    required: false
facts: { provides: [nomad] }
removal: { policy: cleanup }
"#;

    #[test]
    fn module_config_parses_tasks_handlers_and_health() {
        let m = ModuleConfig::parse(MODULE).expect("module parses");
        assert_eq!(m.needs, vec!["tailscale", "docker"]);
        assert_eq!(m.module_needs(), vec!["tailscale", "docker"]);
        assert_eq!(m.tasks.len(), 3);
        assert_eq!(m.tasks[0].kind, "apt.package");
        assert_eq!(m.tasks[1].notify, vec!["nomad"]);
        assert!(m.tasks[1].when.is_some());
        assert_eq!(m.tasks[2].id.as_deref(), Some("eligibility"));
        assert_eq!(m.tasks[2].removal, Some(RemovalPolicy::Leave));
        assert_eq!(m.tasks[2].retry.limit, 3);
        assert_eq!(m.handlers["nomad"].kind, "systemd.restart");
        assert_eq!(
            m.actions.keys().collect::<Vec<_>>(),
            vec!["restart", "drain"],
            "actions keep their declared order"
        );
        assert!(m.actions["drain"].confirm && !m.actions["restart"].confirm);
        assert_eq!(m.health[0].display_name(), "api");
        assert_eq!(m.health[0].check.kind, "http");
        assert_eq!(m.health[1].display_name(), "systemd");
        assert!(!m.health[1].required);
        assert_eq!(m.facts.provides, vec!["nomad"]);
    }

    #[test]
    fn a_module_needs_only_names_modules_on_the_host() {
        let m = ModuleConfig::parse(
            "name: backup\nneeds: [docker, workload:postgresql, module:nomad]\n",
        )
        .unwrap();
        assert_eq!(m.module_needs(), vec!["docker", "nomad"]);
        assert_eq!(
            m.needs(),
            vec![
                Need::Module("docker".into()),
                Need::Workload("postgresql".into()),
                Need::Module("nomad".into())
            ]
        );
    }

    #[test]
    fn module_round_trips_through_serde() {
        let m = ModuleConfig::parse(MODULE).unwrap();
        let yaml = serde_yaml::to_string(&m).unwrap();
        let again = ModuleConfig::parse(&yaml).unwrap();
        assert_eq!(again.tasks[1].kind, "file.template");
        assert_eq!(again.tasks[1].notify, vec!["nomad"]);
        assert_eq!(again.health[0].check.kind, "http");
    }

    #[test]
    fn health_decl_requires_exactly_one_kind() {
        assert!(ModuleConfig::parse("name: x\nhealth:\n  - name: a\n").is_err());
        assert!(ModuleConfig::parse("name: x\nhealth:\n  - http: {}\n    tcp: {}\n").is_err());
    }
}
