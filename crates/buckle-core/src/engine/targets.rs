//! Validation of the cluster layer: `fleet.clusters`, `workloads/*` and the
//! `needs` edges that cross between modules and workloads. Nothing here
//! consults a task, health or fact registry, so the server runs the same
//! checks as `buckle validate`.

use super::graph::Graph;
use super::validate::{ValidationReport, secret_references, yaml_strings};
use super::{LoadedWorkload, Revision};
use crate::model::{ClusterDecl, Need};
use crate::selectors::{Namespaces, selector_namespaces};
use crate::template::Templater;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

fn workload_file(name: &str) -> String {
    format!("workloads/{name}/workload.yaml")
}

fn cluster_location(name: &str) -> String {
    format!("buckle.yaml fleet.clusters.{name}")
}

/// The problem with a selector that names both target namespaces, if it does:
/// such a selector is evaluated against one kind of target at a time, so the
/// other half is always null.
pub fn mixed_namespaces(raw: &str) -> Option<String> {
    let Namespaces { host, cluster } = selector_namespaces(raw);
    (host && cluster).then(|| {
        "names both `host.*` and `cluster.*`; a selector is evaluated against one target at a time, so the other namespace is always null here".to_owned()
    })
}

/// Static validation of the cluster layer, merged into [`super::validate`]:
/// cluster declarations (identities, scopes, member selectors), workload
/// names, `when` namespaces, keys, files, templates, secrets, and the `needs`
/// graph across both layers.
pub fn validate_targets(revision: &Revision) -> ValidationReport {
    let mut report = ValidationReport::default();
    let templater = Templater::new();
    let declared_secrets: BTreeSet<&str> =
        revision.fleet.secrets.keys().map(String::as_str).collect();
    let modules: BTreeSet<&str> = revision
        .modules
        .iter()
        .map(|m| m.config.name.as_str())
        .collect();
    let workloads: BTreeSet<&str> = revision
        .workloads
        .iter()
        .map(|w| w.config.name.as_str())
        .collect();

    let check_secrets = |report: &mut ValidationReport,
                         module: Option<&str>,
                         location: &str,
                         text: &str| {
        for name in secret_references(text) {
            if !declared_secrets.contains(name.as_str()) {
                report.error(
                    module,
                    location,
                    format!("references the secret '{name}', which buckle.yaml does not declare under `secrets`"),
                );
            }
        }
    };
    let check_template =
        |report: &mut ValidationReport, module: Option<&str>, location: &str, text: &str| {
            if Templater::needs_render(text)
                && let Err(e) = templater.check(text)
            {
                report.error(
                    module,
                    location,
                    format!("template does not compile: {}", e.message()),
                );
            }
        };

    for (name, decl) in &revision.fleet.fleet.clusters {
        validate_cluster(&mut report, revision, name, decl, &declared_secrets);
    }

    for name in &revision.unloaded_workloads {
        report.warning(
            Some(name),
            format!("workloads/{name}"),
            "the directory has no workload.yaml: nothing is planned for it",
        );
    }

    // A module's `workload:` edges point into this layer.
    for m in &revision.modules {
        for need in m.config.needs() {
            if let Need::Workload(w) = &need
                && !workloads.contains(w.as_str())
            {
                report.error(
                    Some(&m.config.name),
                    format!("modules/{}/module.yaml", m.config.name),
                    format!("needs the workload '{w}', which does not exist"),
                );
            }
        }
    }

    let mut seen: HashMap<&str, usize> = HashMap::new();
    for w in &revision.workloads {
        *seen.entry(w.config.name.as_str()).or_default() += 1;
    }
    for (name, count) in &seen {
        if *count > 1 {
            report.error(
                Some(name),
                workload_file(name),
                format!(
                    "the workload name '{name}' is used {count} times; workload names must be unique"
                ),
            );
        }
    }

    for w in &revision.workloads {
        let cfg = &w.config;
        let name = cfg.name.as_str();
        let file = workload_file(name);

        if let Some(when) = &cfg.when {
            let location = format!("{file} when");
            if selector_namespaces(when.raw()).host {
                report.error(
                    Some(name),
                    &location,
                    "selects on `host.*`, but a workload's `when` is evaluated against cluster targets; select on `cluster.*` (`cluster.kind`, `cluster.labels.*`, `cluster.groups`) instead",
                );
            }
            check_secrets(&mut report, Some(name), &location, when.raw());
        }

        for need in cfg.needs() {
            let exists = match &need {
                Need::Module(n) => modules.contains(n.as_str()),
                Need::Workload(n) => workloads.contains(n.as_str()),
            };
            if !exists {
                let what = match need {
                    Need::Module(_) => "module",
                    Need::Workload(_) => "workload",
                };
                report.error(
                    Some(name),
                    &file,
                    format!("needs the {what} '{}', which does not exist", need.name()),
                );
            } else if need == Need::Workload(name.to_owned()) {
                report.error(Some(name), &file, "a workload cannot need itself");
            }
        }

        match (cfg.uses_nomad_keys(), cfg.uses_kubernetes_keys()) {
            (true, true) => report.error(
                Some(name),
                &file,
                "mixes Nomad keys (job, files, volumes, variables, policies, namespaces, node_pools, resources) with Kubernetes keys (manifests, secrets); a workload runs on one kind of cluster",
            ),
            (false, false) => report.warning(
                Some(name),
                &file,
                "sets none of the keys that make up a workload (a Nomad job, volumes, variables, …; Kubernetes manifests and secrets), so nothing is applied for it",
            ),
            _ => {}
        }

        let present: BTreeSet<&Path> = w.files.iter().map(|p| p.as_path()).collect();
        for (key, referenced) in cfg.referenced_files() {
            if !present.contains(Path::new(referenced)) {
                report.error(
                    Some(name),
                    format!("{file} {key}"),
                    format!(
                        "names the file '{referenced}', which is not under workloads/{name}/files/"
                    ),
                );
            }
        }

        let keys = cfg.keys();
        for key in &cfg.restart_on {
            if !keys.contains(&key.as_str()) {
                report.error(
                    Some(name),
                    format!("{file} restart_on"),
                    format!("names '{key}', which is not a key this workload sets"),
                );
            }
        }

        for (k, v) in &cfg.vars {
            let mut strings = Vec::new();
            yaml_strings(v, &mut strings);
            for s in strings {
                let loc = format!("{file} vars.{k}");
                check_secrets(&mut report, Some(name), &loc, s);
                check_template(&mut report, Some(name), &loc, s);
            }
        }
        for (path, items) in cfg.variables.iter().chain(cfg.secrets.iter()) {
            for (k, v) in items {
                let loc = format!("{file} {path}.{k}");
                check_secrets(&mut report, Some(name), &loc, v);
                check_template(&mut report, Some(name), &loc, v);
            }
        }
        for (i, h) in cfg.health.iter().enumerate() {
            let location = format!("{file} health[{i}] ({})", h.display_name());
            let mut strings = Vec::new();
            yaml_strings(&h.check.spec, &mut strings);
            for s in strings {
                check_secrets(&mut report, Some(name), &location, s);
                check_template(&mut report, Some(name), &location, s);
            }
        }

        // Specs the scheduler stores in clear must not carry secret values;
        // those travel through `variables` (Nomad) and `secrets` (Kubernetes).
        let specs: BTreeSet<&str> = cfg
            .job
            .iter()
            .map(|j| j.file.as_str())
            .chain(cfg.manifests.iter().map(String::as_str))
            .collect();
        for (path, source) in &w.templates {
            let location = format!("workloads/{name}/files/{}", path.display());
            if let Err(e) = templater.check(source) {
                report.error(
                    Some(name),
                    &location,
                    format!("template does not compile: {}", e.message()),
                );
            }
            let referenced = secret_references(source);
            let is_spec = path.to_str().is_some_and(|p| specs.contains(p));
            if is_spec && !referenced.is_empty() {
                report.error(
                    Some(name),
                    &location,
                    format!(
                        "references the secret{} {}; a job spec or manifest is stored by the scheduler in clear, so pass secret values through `variables` (Nomad) or `secrets` (Kubernetes) instead",
                        if referenced.len() == 1 { "" } else { "s" },
                        referenced
                            .iter()
                            .map(|s| format!("'{s}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            } else {
                check_secrets(&mut report, Some(name), &location, source);
            }
        }
    }

    // The `needs` graph across both layers. Module nodes carry only their
    // `workload:` edges (module-to-module cycles are the module graph's to
    // report), so any cycle found here passes through a workload.
    let mut graph = Graph::new();
    for m in &revision.modules {
        let edges: Vec<String> = m
            .config
            .needs()
            .into_iter()
            .filter(|n| matches!(n, Need::Workload(_)))
            .map(|n| n.qualified())
            .collect();
        graph.add(&Need::Module(m.config.name.clone()).qualified(), &edges);
    }
    for w in &revision.workloads {
        let edges: Vec<String> = w.config.needs().iter().map(Need::qualified).collect();
        graph.add(&Need::Workload(w.config.name.clone()).qualified(), &edges);
    }
    if let Some(cycle) = graph.find_cycle() {
        report.error(
            None,
            "workloads",
            format!(
                "the `needs` across modules and workloads form a cycle: {}",
                cycle.join(" -> ")
            ),
        );
    }

    report
}

fn validate_cluster(
    report: &mut ValidationReport,
    revision: &Revision,
    name: &str,
    decl: &ClusterDecl,
    declared_secrets: &BTreeSet<&str>,
) {
    let location = cluster_location(name);
    if let Some(secret) = decl.identity.secret()
        && !declared_secrets.contains(secret)
    {
        report.error(
            None,
            &location,
            format!(
                "uses the identity 'secret://{secret}', but buckle.yaml does not declare the secret '{secret}' under `secrets`"
            ),
        );
    }
    for (key, selector) in [("nodes", &decl.nodes), ("executors", &decl.executors)] {
        if let Some(filter) = selector
            && selector_namespaces(filter.raw()).cluster
        {
            report.error(
                None,
                format!("{location}.{key}"),
                format!(
                    "`{key}` selects on `cluster.*`, but it chooses the hosts that belong to the cluster; select on `host.*` instead"
                ),
            );
        }
    }
    for (key, value) in &decl.labels {
        if !matches!(
            value,
            serde_yaml::Value::String(_)
                | serde_yaml::Value::Bool(_)
                | serde_yaml::Value::Number(_)
        ) {
            report.error(
                None,
                format!("{location}.labels.{key}"),
                "a label must be a string, number or boolean",
            );
        }
    }
    match &decl.cluster {
        None => {
            if !decl.scope.is_empty() {
                report.error(
                    None,
                    format!("{location}.scope"),
                    "sets `scope` without `cluster`; a scope narrows a parent cluster, so name the cluster it is a part of",
                );
            }
            if decl.nodes.is_none() && decl.executors.is_none() {
                report.warning(
                    None,
                    &location,
                    "declares neither `nodes` nor `executors`: no host can speak for this cluster, so its workloads cannot be applied",
                );
            }
        }
        Some(parent) if parent == name => {
            report.error(None, &location, "cannot be scoped to itself");
        }
        Some(parent) => match revision.fleet.fleet.clusters.get(parent) {
            None => report.error(
                None,
                format!("{location}.cluster"),
                format!("is scoped to the cluster '{parent}', which fleet.clusters does not declare"),
            ),
            Some(p) if p.cluster.is_some() => report.error(
                None,
                format!("{location}.cluster"),
                format!("is scoped to '{parent}', which is itself a scoped target; scope the cluster it belongs to instead"),
            ),
            Some(p) if p.kind != decl.kind => report.error(
                None,
                format!("{location}.kind"),
                format!(
                    "is a {} target scoped to '{parent}', which is a {} cluster",
                    decl.kind, p.kind
                ),
            ),
            Some(_) => {}
        },
    }
}

/// The names of the cluster targets a workload selects, given the contexts to
/// decide against. Used by `buckle validate --facts` style dry runs and by
/// tests; the server decides selection for real targets the same way.
pub fn selected_workloads<'a>(
    revision: &'a Revision,
    ctx: &crate::selectors::HostContext,
) -> Vec<&'a LoadedWorkload> {
    revision
        .workloads
        .iter()
        .filter(|w| match &w.config.when {
            None => true,
            Some(when) => ctx.in_module(&w.config.name, &w.config.tags).matches(when),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::RevisionId;
    use rstest::rstest;

    const FLEET: &str = "version: 2
fleet:
  clusters:
    home-nomad:
      kind: nomad
      nodes: 'host.groups contains \"nomad\"'
secrets:
  TOKEN: { env: TOKEN }
";

    fn load(files: &[(&str, &str)]) -> Revision {
        let dir = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let path = dir.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, c).unwrap();
        }
        super::super::loader::load_dir(dir.path(), RevisionId::new("t")).unwrap()
    }

    fn errors(files: &[(&str, &str)]) -> Vec<String> {
        let rev = load(files);
        validate_targets(&rev)
            .errors
            .iter()
            .map(|e| e.to_string())
            .collect()
    }

    #[rstest]
    #[case::host_namespace(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nwhen: 'host.labels.x == 1'\njob: a.hcl\n"), ("workloads/a/files/a.hcl", "job {}")],
        "selects on `host.*`"
    )]
    #[case::missing_module(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nneeds: ['module:nope']\njob: a.hcl\n"), ("workloads/a/files/a.hcl", "job {}")],
        "needs the module 'nope'"
    )]
    #[case::missing_workload(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nneeds: [b]\njob: a.hcl\n"), ("workloads/a/files/a.hcl", "job {}")],
        "needs the workload 'b'"
    )]
    #[case::module_needs_missing_workload(
        &[("buckle.yaml", FLEET), ("modules/m/module.yaml", "name: m\nneeds: ['workload:nope']\n")],
        "needs the workload 'nope'"
    )]
    #[case::mixed_keys(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\njob: a.hcl\nmanifests: [a.yaml]\n"), ("workloads/a/files/a.hcl", ""), ("workloads/a/files/a.yaml", "")],
        "mixes Nomad keys"
    )]
    #[case::missing_file(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\njob: a.hcl\nvolumes: [data.hcl]\n"), ("workloads/a/files/a.hcl", "")],
        "names the file 'data.hcl'"
    )]
    #[case::bad_template(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\njob: a.hcl.j2\n"), ("workloads/a/files/a.hcl.j2", "{% if %}")],
        "does not compile"
    )]
    #[case::secret_in_job_spec(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\njob: a.hcl.j2\n"), ("workloads/a/files/a.hcl.j2", "token = \"{{ secrets.TOKEN }}\"")],
        "stored by the scheduler in clear"
    )]
    #[case::undeclared_secret(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\njob: a.hcl\nvariables:\n  nomad/jobs/a: { token: '{{ secrets.NOPE }}' }\n"), ("workloads/a/files/a.hcl", "")],
        "does not declare under `secrets`"
    )]
    #[case::restart_on_unknown_key(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\njob: a.hcl\nrestart_on: [variables]\n"), ("workloads/a/files/a.hcl", "")],
        "not a key this workload sets"
    )]
    #[case::cross_layer_cycle(
        &[("buckle.yaml", FLEET), ("modules/m/module.yaml", "name: m\nneeds: ['workload:a']\n"), ("workloads/a/workload.yaml", "name: a\nneeds: ['module:m']\njob: a.hcl\n"), ("workloads/a/files/a.hcl", "")],
        "form a cycle"
    )]
    #[case::identity_secret_undeclared(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    c:\n      kind: nomad\n      nodes: 'true'\n      identity: secret://NOPE\n")],
        "does not declare the secret 'NOPE'"
    )]
    #[case::nodes_select_clusters(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    c:\n      kind: nomad\n      nodes: 'cluster.labels.x == 1'\n")],
        "`nodes` selects on `cluster.*`"
    )]
    #[case::scope_without_parent(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    c:\n      kind: kubernetes\n      nodes: 'true'\n      scope: { namespaces: [a] }\n")],
        "sets `scope` without `cluster`"
    )]
    #[case::unknown_parent(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    c:\n      kind: kubernetes\n      cluster: nope\n")],
        "which fleet.clusters does not declare"
    )]
    #[case::parent_kind_mismatch(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    p: { kind: nomad, nodes: 'true' }\n    c: { kind: kubernetes, cluster: p }\n")],
        "which is a nomad cluster"
    )]
    fn invalid_configurations_are_reported(#[case] files: &[(&str, &str)], #[case] needle: &str) {
        let errors = errors(files);
        assert!(
            errors.iter().any(|e| e.contains(needle)),
            "expected {needle:?} in {errors:#?}"
        );
    }

    #[test]
    fn a_valid_cluster_layer_has_no_errors() {
        let rev = load(&[
            (
                "buckle.yaml",
                "version: 2
fleet:
  groups:
    home-cluster: 'cluster.labels.site == \"home\"'
  clusters:
    home-nomad:
      kind: nomad
      nodes: 'host.groups contains \"nomad\"'
      executors: 'host.labels.role == \"server\"'
      identity: secret://NOMAD_TOKEN
      labels: { site: home }
    team-a:
      kind: nomad
      cluster: home-nomad
      scope: { namespaces: [team-a] }
secrets:
  NOMAD_TOKEN: { env: NOMAD_TOKEN }
  DB: { env: DB }
",
            ),
            ("modules/docker/module.yaml", "name: docker\n"),
            (
                "modules/proxy/module.yaml",
                "name: proxy\nneeds: ['workload:paste']\n",
            ),
            (
                "workloads/paste/workload.yaml",
                "name: paste
when: 'cluster.groups contains \"home-cluster\"'
needs: ['module:docker']
job: paste.nomad.hcl.j2
volumes: [paste.volume.hcl]
variables:
  nomad/jobs/paste: { db: '{{ secrets.DB }}' }
restart_on: [variables]
",
            ),
            (
                "workloads/paste/files/paste.nomad.hcl.j2",
                "job \"paste\" {}",
            ),
            ("workloads/paste/files/paste.volume.hcl", "volume {}"),
        ]);
        let report = validate_targets(&rev);
        assert_eq!(report.errors, Vec::new(), "{report}");
        assert_eq!(report.warnings, Vec::new(), "{report}");
    }

    #[test]
    fn empty_workloads_and_bare_directories_warn() {
        let rev = load(&[
            ("buckle.yaml", FLEET),
            ("workloads/a/workload.yaml", "name: a\n"),
            ("workloads/b/README.md", "later"),
        ]);
        let report = validate_targets(&rev);
        assert!(report.is_ok(), "{report}");
        let warnings: Vec<String> = report.warnings.iter().map(|w| w.to_string()).collect();
        assert!(
            warnings.iter().any(|w| w.contains("nothing is applied")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("workloads/b")),
            "{warnings:?}"
        );
    }

    #[test]
    fn workloads_select_clusters_by_their_context() {
        let rev = load(&[
            ("buckle.yaml", FLEET),
            ("workloads/all/workload.yaml", "name: all\njob: a.hcl\n"),
            (
                "workloads/k8s/workload.yaml",
                "name: k8s\nwhen: 'cluster.kind == \"kubernetes\"'\nmanifests: [a.yaml]\n",
            ),
            ("workloads/all/files/a.hcl", ""),
            ("workloads/k8s/files/a.yaml", ""),
        ]);
        let ctx = crate::selectors::HostContext::for_cluster(
            crate::selectors::HostIdentity {
                id: "home-nomad".into(),
                labels: Default::default(),
            },
            crate::model::ClusterKind::Nomad,
        );
        let names: Vec<&str> = selected_workloads(&rev, &ctx)
            .iter()
            .map(|w| w.config.name.as_str())
            .collect();
        assert_eq!(names, vec!["all"]);
    }

    #[test]
    fn mixed_namespaces_are_pointed_out() {
        assert!(mixed_namespaces("host.labels.a == 1 && cluster.kind == \"nomad\"").is_some());
        assert!(mixed_namespaces("host.labels.a == 1").is_none());
        assert!(mixed_namespaces("true").is_none());
    }
}
