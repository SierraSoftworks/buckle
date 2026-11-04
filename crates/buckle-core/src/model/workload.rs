//! `workloads/<name>/workload.yaml`: what a cluster target runs (design:
//! cluster workloads, D4, D5, D5b). A workload shares `name`, `description`,
//! `when`, `needs`, `vars`, `health` and `removal` with a module, but has no
//! tasks, handlers or facts: its keys say what it consists of (a job, volumes,
//! variables, policies and the other cluster entities; on Kubernetes
//! manifests and secrets; on both the namespace they live in) and the
//! executor knows the order. Its
//! `when` selects over `cluster.*`, never `host.*`.

use super::HealthDecl;
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

/// A job spec: the file under `files/` and what to ignore when diffing it.
/// Written as a bare file name or as `{ file, ignore }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobDecl {
    pub file: String,
    /// Field paths of the live job that may differ from the spec without
    /// counting as a change or drift (`Meta.autoupdate.*`,
    /// `TaskGroups[*].Tasks[*].Config.image`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
}

impl JobDecl {
    /// A plain file reference.
    pub fn file(file: impl Into<String>) -> Self {
        Self {
            file: file.into(),
            ignore: Vec::new(),
        }
    }
}

fn job_from_yaml<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<JobDecl>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        File(String),
        Full(JobDecl),
    }
    Ok(match Option::<Raw>::deserialize(d)? {
        None => None,
        Some(Raw::File(file)) => Some(JobDecl::file(file)),
        Some(Raw::Full(decl)) => Some(decl),
    })
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

/// One ACL policy: its rules file and, optionally, what it attaches to.
/// Written as a bare file name or `{ file, attach }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDecl {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach: Option<PolicyAttach>,
}

fn policies_from_yaml<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<PolicyDecl>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        File(String),
        Full(PolicyDecl),
    }
    Ok(Vec::<Raw>::deserialize(d)?
        .into_iter()
        .map(|r| match r {
            Raw::File(file) => PolicyDecl { file, attach: None },
            Raw::Full(decl) => decl,
        })
        .collect())
}

/// A cluster entity the other keys do not name (`quota`, `acl_role`,
/// `csi_volume`, …): its API type and its spec file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceDecl {
    #[serde(rename = "type")]
    pub kind: String,
    pub file: String,
}

/// The items of a variable or a Secret: name to value, where a value may
/// reference `secrets.*` (it then reaches the cluster through the API sink
/// and is planned by fingerprint).
pub type Items = IndexMap<String, String>;

/// `workload.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Defaults; fleet vars override. On Nomad they become the job's HCL2
    /// variables.
    #[serde(default)]
    pub vars: IndexMap<String, serde_yaml::Value>,

    // ---- Nomad
    /// The job spec, HCL2, sent verbatim.
    #[serde(default, deserialize_with = "job_from_yaml")]
    pub job: Option<JobDecl>,
    /// Files passed to the job as string variables, by variable name.
    #[serde(default)]
    pub files: IndexMap<String, String>,
    /// Dynamic host volume specs, created once by name.
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Nomad variables by path, each a map of items.
    #[serde(default)]
    pub variables: IndexMap<String, Items>,
    /// ACL policies, each optionally attached to a job, group or task.
    #[serde(default, deserialize_with = "policies_from_yaml")]
    pub policies: Vec<PolicyDecl>,
    #[serde(default)]
    pub namespaces: Vec<String>,
    #[serde(default)]
    pub node_pools: Vec<String>,
    /// Every other entity the API names.
    #[serde(default)]
    pub resources: Vec<ResourceDecl>,

    // ---- Kubernetes
    /// Manifests applied as they are (`.yaml`) or rendered first (`.j2`).
    #[serde(default)]
    pub manifests: Vec<String>,
    /// Secrets filled through the API sink, by name.
    #[serde(default)]
    pub secrets: IndexMap<String, Items>,

    // ---- shared
    /// The namespace the workload's entities live in; created if missing.
    /// On Nomad it is the namespace of the `variables` (and of the job when
    /// the spec names none); on Kubernetes the one objects without a
    /// namespace land in. Absent: the scheduler's default namespace.
    #[serde(default)]
    pub namespace: Option<String>,
    /// Extra checks beside the implicit ones (the job's deployment and
    /// allocations; a Deployment's rollout and pods).
    #[serde(default)]
    pub health: Vec<HealthDecl>,
    /// Keys whose change restarts the job even when its spec did not change
    /// (`[variables]` for a job without a `template` stanza).
    #[serde(default)]
    pub restart_on: Vec<String>,
    #[serde(default)]
    pub removal: WorkloadRemoval,
}

impl WorkloadConfig {
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

    /// The keys this workload sets, in the order the executor applies them.
    pub fn keys(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.namespace.is_some() {
            out.push("namespace");
        }
        if !self.namespaces.is_empty() {
            out.push("namespaces");
        }
        if !self.node_pools.is_empty() {
            out.push("node_pools");
        }
        if !self.volumes.is_empty() {
            out.push("volumes");
        }
        if !self.variables.is_empty() {
            out.push("variables");
        }
        if !self.policies.is_empty() {
            out.push("policies");
        }
        if !self.resources.is_empty() {
            out.push("resources");
        }
        if self.job.is_some() {
            out.push("job");
        }
        if !self.secrets.is_empty() {
            out.push("secrets");
        }
        if !self.manifests.is_empty() {
            out.push("manifests");
        }
        out
    }

    /// Which scheduler's keys the workload uses: `Some(kind)` when it uses
    /// one scheduler's keys only, `None` when it names no scheduler-specific
    /// key (an empty workload) or mixes the two (validation rejects that).
    pub fn cluster_kind(&self) -> Option<super::ClusterKind> {
        match (self.uses_nomad_keys(), self.uses_kubernetes_keys()) {
            (true, false) => Some(super::ClusterKind::Nomad),
            (false, true) => Some(super::ClusterKind::Kubernetes),
            _ => None,
        }
    }

    pub fn uses_nomad_keys(&self) -> bool {
        self.job.is_some()
            || !self.files.is_empty()
            || !self.volumes.is_empty()
            || !self.variables.is_empty()
            || !self.policies.is_empty()
            || !self.namespaces.is_empty()
            || !self.node_pools.is_empty()
            || !self.resources.is_empty()
    }

    pub fn uses_kubernetes_keys(&self) -> bool {
        !self.manifests.is_empty() || !self.secrets.is_empty()
    }

    /// Every file under `files/` the workload references, with the key that
    /// names it.
    pub fn referenced_files(&self) -> Vec<(&'static str, &str)> {
        let mut out: Vec<(&'static str, &str)> = Vec::new();
        if let Some(job) = &self.job {
            out.push(("job", &job.file));
        }
        out.extend(self.files.values().map(|f| ("files", f.as_str())));
        out.extend(self.volumes.iter().map(|f| ("volumes", f.as_str())));
        out.extend(self.policies.iter().map(|p| ("policies", p.file.as_str())));
        out.extend(self.namespaces.iter().map(|f| ("namespaces", f.as_str())));
        out.extend(self.node_pools.iter().map(|f| ("node_pools", f.as_str())));
        out.extend(
            self.resources
                .iter()
                .map(|r| ("resources", r.file.as_str())),
        );
        out.extend(self.manifests.iter().map(|f| ("manifests", f.as_str())));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const NOMAD: &str = r#"
name: atuin
description: Atuin shell-history sync server.
when: 'cluster.groups contains "home-cluster"'
needs: [postgresql, module:nomad]
vars:
  atuin_version: "18.6.1"
job: atuin.nomad.hcl
variables:
  nomad/jobs/atuin:
    DATABASE_URL: 'postgres://atuin:{{ secrets.ATUIN_DB_PASSWORD }}@db/atuin'
    ATUIN_OPEN_REGISTRATION: "false"
policies:
  - { file: atuin.policy.hcl, attach: { job: atuin } }
  - anonymous.policy.hcl
health:
  - name: api
    http: { url: 'http://atuin.example/', expect: { status: 200 } }
    required: false
"#;

    #[test]
    fn a_nomad_workload_parses_with_its_keys_in_apply_order() {
        let w = WorkloadConfig::parse(NOMAD).expect("workload parses");
        assert_eq!(
            w.needs(),
            vec![
                Need::Workload("postgresql".into()),
                Need::Module("nomad".into())
            ]
        );
        assert_eq!(w.job.as_ref().unwrap().file, "atuin.nomad.hcl");
        assert_eq!(w.keys(), vec!["variables", "policies", "job"]);
        assert_eq!(w.cluster_kind(), Some(super::super::ClusterKind::Nomad));
        assert_eq!(w.policies[0].attach.as_ref().unwrap().job, "atuin");
        assert!(w.policies[1].attach.is_none());
        assert_eq!(w.health[0].display_name(), "api");
        assert!(!w.health[0].required);
        assert_eq!(w.removal, WorkloadRemoval::Stop);
        let files: Vec<&str> = w.referenced_files().into_iter().map(|(_, f)| f).collect();
        assert_eq!(
            files,
            vec![
                "atuin.nomad.hcl",
                "atuin.policy.hcl",
                "anonymous.policy.hcl"
            ]
        );
    }

    #[test]
    fn a_kubernetes_workload_parses() {
        let w = WorkloadConfig::parse(
            r#"
name: grafana
when: 'cluster.kind == "kubernetes"'
namespace: monitoring
manifests: [grafana.yaml.j2]
secrets:
  grafana-admin:
    admin-password: '{{ secrets.GRAFANA_ADMIN_PASSWORD }}'
removal: purge
"#,
        )
        .unwrap();
        assert_eq!(w.keys(), vec!["namespace", "secrets", "manifests"]);
        assert_eq!(
            w.cluster_kind(),
            Some(super::super::ClusterKind::Kubernetes)
        );
        assert_eq!(w.removal, WorkloadRemoval::Purge);
    }

    #[test]
    fn an_entity_only_workload_and_a_job_with_ignores_parse() {
        let w = WorkloadConfig::parse(
            "name: platform\nnamespaces: [team-a.namespace.hcl]\nnode_pools: [pis.pool.hcl]\nresources:\n  - { type: quota, file: team-a.quota.hcl }\nremoval: keep\n",
        )
        .unwrap();
        assert_eq!(w.keys(), vec!["namespaces", "node_pools", "resources"]);
        assert_eq!(w.resources[0].kind, "quota");
        assert_eq!(w.removal, WorkloadRemoval::Keep);
        let w = WorkloadConfig::parse(
            "name: paste\njob: { file: paste.nomad.hcl, ignore: ['Meta.autoupdate.*'] }\n",
        )
        .unwrap();
        assert_eq!(w.job.as_ref().unwrap().ignore, vec!["Meta.autoupdate.*"]);
    }

    #[test]
    fn mixed_or_empty_workloads_have_no_single_kind() {
        let mixed = WorkloadConfig::parse("name: x\njob: x.hcl\nmanifests: [x.yaml]\n").unwrap();
        assert_eq!(mixed.cluster_kind(), None);
        assert!(mixed.uses_nomad_keys() && mixed.uses_kubernetes_keys());
        let empty = WorkloadConfig::parse("name: x\n").unwrap();
        assert_eq!(empty.cluster_kind(), None);
        assert!(empty.keys().is_empty());
    }

    #[rstest]
    #[case("name: x\ntasks: []\n")]
    #[case("name: x\nhandlers: {}\n")]
    #[case("name: x\nfacts: { provides: [a] }\n")]
    #[case("name: x\nremoval: delete\n")]
    #[case("name: x\npolicies: [{ attach: { job: a } }]\n")]
    fn module_only_and_unknown_keys_are_rejected(#[case] yaml: &str) {
        assert!(WorkloadConfig::parse(yaml).is_err(), "{yaml}");
    }

    #[test]
    fn workloads_round_trip_through_serde() {
        let w = WorkloadConfig::parse(NOMAD).unwrap();
        let yaml = serde_yaml::to_string(&w).unwrap();
        let again = WorkloadConfig::parse(&yaml).unwrap();
        assert_eq!(again.keys(), w.keys());
        assert_eq!(again.needs(), w.needs());
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
