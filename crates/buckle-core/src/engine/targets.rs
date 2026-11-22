//! Validation of the cluster layer: `fleet.clusters`, `workloads/*` and the
//! `needs` edges that cross between modules and workloads. Nothing here
//! consults a task, health or fact registry, so the server runs the same
//! checks as `buckle validate`.

use super::graph::Graph;
use super::validate::{ValidationReport, secret_references, yaml_strings};
use super::{LoadedWorkload, Revision};
use crate::model::{AcrossPhases, ClusterDecl, Need, ResourceKind};
use crate::selectors::{HostContext, selector_namespaces};
use crate::template::Templater;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

fn workload_file(name: &str) -> String {
    format!("workloads/{name}/workload.yaml")
}

fn cluster_location(name: &str) -> String {
    format!("buckle.yaml fleet.clusters.{name}")
}

/// The problem with a selector that names `host.*` beside `cluster.*` or
/// `workload.*`, if it does: such a selector is evaluated against one target
/// at a time (a host reads only `host.*`; a placement reads `cluster.*` and
/// `workload.*` together), so the other half is always null.
pub fn mixed_namespaces(raw: &str) -> Option<String> {
    selector_namespaces(raw).mixed().then(|| {
        "names `host.*` beside `cluster.*` or `workload.*`; a selector is evaluated against one target at a time (a host reads only `host.*`, a workload placement `cluster.*` and `workload.*`), so the other namespace is always null here".to_owned()
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

    // A module's `workload:` and `group:` edges point out of its layer.
    for m in &revision.modules {
        let file = format!("modules/{}/module.yaml", m.config.name);
        for need in m.config.needs() {
            if !matches!(need, Need::Module(_)) {
                check_need(&mut report, revision, &m.config.name, &file, &need);
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
            let namespaces = selector_namespaces(when.raw());
            if namespaces.host {
                report.error(
                    Some(name),
                    &location,
                    "selects on `host.*`, but a workload's `when` is evaluated against cluster targets; select on `cluster.*` (`cluster.kind`, `cluster.labels.*`, `cluster.groups`) instead",
                );
            }
            if namespaces.workload {
                report.error(
                    Some(name),
                    &location,
                    "selects on `workload.*`, but a workload's `when` decides which clusters run it and is evaluated before any placement exists; `workload.labels.*` and `workload.name` select placements in phase selectors and groups",
                );
            }
            check_secrets(&mut report, Some(name), &location, when.raw());
        }
        for (key, value) in &cfg.labels {
            if !matches!(
                value,
                serde_yaml::Value::String(_)
                    | serde_yaml::Value::Bool(_)
                    | serde_yaml::Value::Number(_)
            ) {
                report.error(
                    Some(name),
                    format!("{file} labels.{key}"),
                    "a label must be a string, number or boolean",
                );
            }
        }

        for need in cfg.needs() {
            if need == Need::Workload(name.to_owned()) {
                report.error(Some(name), &file, "a workload cannot need itself");
            } else {
                check_need(&mut report, revision, name, &file, &need);
            }
        }

        if !cfg.legacy_keys.is_empty() {
            report.warning(
                Some(name),
                &file,
                format!(
                    "uses the deprecated key{} {}; list the objects under `resources:` instead (they apply in list order)",
                    if cfg.legacy_keys.len() == 1 { "" } else { "s" },
                    cfg.legacy_keys
                        .iter()
                        .map(|k| format!("`{k}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }

        match (cfg.uses_nomad_keys(), cfg.uses_kubernetes_keys()) {
            (true, true) => report.error(
                Some(name),
                format!("{file} resources"),
                "mixes Nomad resources (jobs, volumes, policies, namespaces, node pools, quotas) with Kubernetes resources (manifests); a workload runs on one kind of cluster",
            ),
            (false, false) if cfg.resources.is_empty() => report.warning(
                Some(name),
                &file,
                "lists no resources (a Nomad job, volumes, secrets, …; Kubernetes manifests), so nothing is applied for it",
            ),
            _ => {}
        }
        if let Some(kind) = cfg.cluster_kind() {
            for (cluster, decl) in &revision.fleet.fleet.clusters {
                if decl.kind != kind && selects(revision, w, cluster, decl) {
                    report.error(
                        Some(name),
                        format!("{file} when"),
                        format!(
                            "selects the {} cluster '{cluster}', but its resources are for {kind} clusters; narrow `when` to `cluster.kind == \"{kind}\"`",
                            decl.kind,
                        ),
                    );
                }
            }
        }
        for (cluster, decl) in &revision.fleet.fleet.clusters {
            if selects(revision, w, cluster, decl) {
                validate_bounds(&mut report, revision, w, cluster);
            }
        }
        for duplicate in cfg.duplicates() {
            report.error(
                Some(name),
                format!("{file} resources"),
                format!("lists {duplicate} twice; each object may appear once per workload"),
            );
        }
        for collision in cfg.item_collisions() {
            report.error(Some(name), format!("{file} resources"), collision);
        }
        if let Some(problem) = cfg.ordering_problem() {
            report.warning(Some(name), format!("{file} resources"), problem);
        }

        let present: BTreeSet<&Path> = w.files.iter().map(|p| p.as_path()).collect();
        for (kind, referenced) in cfg.referenced_files() {
            if !present.contains(Path::new(referenced)) {
                report.error(
                    Some(name),
                    format!("{file} resources"),
                    format!(
                        "the {kind} entry names the file '{referenced}', which is not under workloads/{name}/files/"
                    ),
                );
            }
        }

        for entry in &cfg.resources {
            let location = format!("{file} resources ({})", entry.describe());
            for path in &entry.ignore {
                if let Err(e) = crate::fieldpath::FieldPath::parse(path) {
                    report.error(Some(name), &location, format!("ignore: {}", e.message()));
                }
            }
        }

        let ids = cfg.resource_ids();
        for key in &cfg.restart_on {
            if !ids.contains(&key.as_str()) {
                report.error(
                    Some(name),
                    format!("{file} restart_on"),
                    format!(
                        "names '{key}', which is not a resource of this workload (name an entry by its file or secret name)"
                    ),
                );
            }
        }

        for (aname, action) in &cfg.actions {
            let location = format!("{file} actions.{aname} ({})", action.task.kind);
            let task = &action.task;
            if cfg.implicit_actions().iter().any(|i| i.name == aname) {
                report.error(
                    Some(name),
                    &location,
                    format!("'{aname}' is an implicit action of every workload with a job; give the extra another name"),
                );
            }
            if !crate::model::WORKLOAD_ACTION_KINDS.contains(&task.kind.as_str()) {
                report.error(
                    Some(name),
                    &location,
                    format!(
                        "'{}' is not a workload action kind (use one of: {})",
                        task.kind,
                        crate::model::WORKLOAD_ACTION_KINDS.join(", ")
                    ),
                );
            } else if let Err(e) =
                serde_yaml::from_value::<crate::model::DispatchSpec>(task.spec.clone())
            {
                report.error(Some(name), &location, format!("invalid: {e}"));
            }
            let mut unsupported = action.unsupported_keys();
            if !task.notify.is_empty() {
                unsupported.push("notify");
            }
            if task.when.is_some() {
                unsupported.push("when");
            }
            for key in unsupported {
                report.error(
                    Some(name),
                    &location,
                    format!("a workload action does not take `{key}`"),
                );
            }
            let mut strings = Vec::new();
            yaml_strings(&task.spec, &mut strings);
            for s in strings {
                check_secrets(&mut report, Some(name), &location, s);
                check_template(&mut report, Some(name), &location, s);
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
        for entry in cfg.resources.iter().filter(|r| r.kind.is_items()) {
            let path = entry.id();
            for (k, v) in &entry.items {
                let loc = format!("{file} {path}.{k}");
                check_secrets(&mut report, Some(name), &loc, v);
                check_template(&mut report, Some(name), &loc, v);
            }
        }
        // A config is stored in clear (a plain variable, a ConfigMap), so its
        // items and files hold no secret values.
        for entry in cfg.of_kind(ResourceKind::Config) {
            let referenced: Vec<String> = entry
                .items
                .values()
                .map(String::as_str)
                .chain(entry.files.values().filter_map(|f| {
                    w.templates
                        .iter()
                        .find(|(p, _)| p.to_str() == Some(f.as_str()))
                        .map(|(_, s)| s.as_str())
                }))
                .flat_map(secret_references)
                .collect();
            if !referenced.is_empty() {
                report.error(
                    Some(name),
                    format!("{file} resources ({})", entry.describe()),
                    format!(
                        "references the secret{} {}; a config is stored in clear, so put secret values in a `secret` entry instead",
                        if referenced.len() == 1 { "" } else { "s" },
                        referenced
                            .iter()
                            .collect::<BTreeSet<_>>()
                            .iter()
                            .map(|s| format!("'{s}'"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
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
        let specs: BTreeSet<&str> = cfg.spec_files().into_iter().collect();
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
    // workload edges (module-to-module cycles are the module graph's to
    // report), so any cycle found here passes through a workload. A
    // `workload:NAME:CLUSTER` edge counts as one on the workload, except a
    // workload's on itself (its placement on another cluster first); `group:`
    // edges depend on the fleet's facts and are resolved when a rollout
    // starts.
    let mut graph = Graph::new();
    let workload_edge = |from: &Need, need: &Need| -> Option<String> {
        let to = Need::Workload(need.workload()?.to_owned());
        (to != *from).then(|| to.qualified())
    };
    for m in &revision.modules {
        let node = Need::Module(m.config.name.clone());
        let edges: Vec<String> = m
            .config
            .needs()
            .iter()
            .filter_map(|n| workload_edge(&node, n))
            .collect();
        graph.add(&node.qualified(), &edges);
    }
    for w in &revision.workloads {
        let node = Need::Workload(w.config.name.clone());
        let edges: Vec<String> = w
            .config
            .needs()
            .iter()
            .filter_map(|n| match n {
                Need::Module(_) => Some(n.qualified()),
                Need::Group(_) => None,
                _ => workload_edge(&node, n),
            })
            .collect();
        graph.add(&node.qualified(), &edges);
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

    cross_phase_needs(&mut report, revision);

    report
}

/// Reports a `needs` entry of `owner` (declared in `file`) that names
/// something the revision does not have: an unknown module, workload or
/// group is an error; `workload:NAME:CLUSTER` on a cluster `buckle.yaml`
/// does not declare is a warning (clusters can be created through the API),
/// the need being met there only once the cluster runs the workload.
fn check_need(
    report: &mut ValidationReport,
    revision: &Revision,
    owner: &str,
    file: &str,
    need: &Need,
) {
    let (what, exists) = match need {
        Need::Module(n) => ("module", revision.module(n).is_some()),
        Need::Workload(n) | Need::Placement { workload: n, .. } => (
            "workload",
            revision.workloads.iter().any(|w| w.config.name == *n),
        ),
        Need::Group(n) => ("group", revision.fleet.fleet.groups.contains_key(n)),
    };
    if !exists {
        let hint = if matches!(need, Need::Group(_)) {
            " (groups are declared under `fleet.groups` in buckle.yaml)"
        } else {
            ""
        };
        report.error(
            Some(owner),
            file,
            format!(
                "needs the {what} '{}', which does not exist{hint}",
                need.name()
            ),
        );
    } else if let Need::Placement { workload, cluster } = need
        && !revision.fleet.fleet.clusters.contains_key(cluster)
    {
        report.warning(
            Some(owner),
            file,
            format!(
                "needs '{workload}' on the cluster '{cluster}', which buckle.yaml does not declare; the need holds until a cluster of that id runs the workload"
            ),
        );
    }
}

/// A workload placement that needs a workload the same cluster rolls out in
/// a later phase: it cannot be ordered by steps (those lie inside a phase),
/// so the rollout waits for the need to be running (`rollout.dependencies.
/// across_phases: warn`, the default) or the revision is refused (`refuse`).
/// Decided over the clusters `buckle.yaml` declares, with their declared
/// labels.
fn cross_phase_needs(report: &mut ValidationReport, revision: &Revision) {
    let phases = &revision.fleet.rollout.phases;
    if phases.is_empty() {
        return;
    }
    let refuse = revision.fleet.rollout.dependencies.across_phases == AcrossPhases::Refuse;
    for (cluster, decl) in &revision.fleet.fleet.clusters {
        let ctx = declared_cluster_context(revision, cluster, decl);
        let placed: Vec<(&LoadedWorkload, Option<usize>)> = revision
            .workloads
            .iter()
            .filter(|w| selects_in(&ctx, w))
            .map(|w| {
                let pc = super::validate::placement_context(revision, &ctx, &w.config);
                (w, phases.iter().position(|p| pc.matches(&p.select)))
            })
            .collect();
        for (w, phase) in &placed {
            let Some(phase) = phase else { continue };
            for need in w.config.needs() {
                let Need::Workload(needed) = &need else {
                    continue;
                };
                let Some((_, Some(needed_phase))) =
                    placed.iter().find(|(x, _)| x.config.name == *needed)
                else {
                    continue;
                };
                if needed_phase <= phase {
                    continue;
                }
                let name = w.config.name.as_str();
                let message = format!(
                    "needs '{needed}', which the cluster '{cluster}' rolls out in the later phase '{}' (this workload is in '{}'); {}",
                    phases[*needed_phase].name,
                    phases[*phase].name,
                    if refuse {
                        "`rollout.dependencies.across_phases: refuse` does not allow a dependency on a later phase"
                    } else {
                        "it waits for '{needed}' to be running (applied and healthy) before it applies, and otherwise finishes in the stragglers stage"
                    }
                );
                let location = workload_file(name);
                if refuse {
                    report.error(Some(name), location, message);
                } else {
                    report.warning(Some(name), location, message);
                }
            }
        }
    }
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
    if decl.cluster.is_some() && !decl.pools.is_empty() {
        report.error(
            None,
            format!("{location}.pools"),
            "a scoped target has no nodes of its own; declare `pools` on its cluster instead",
        );
    }
    if let Some(trust) = &decl.trust {
        if decl.cluster.is_some() {
            report.error(
                None,
                format!("{location}.trust"),
                "a scoped target is served by its cluster's executors; set `trust` on the cluster instead",
            );
        } else if let Some(problem) = trust.problem() {
            report.error(None, format!("{location}.trust"), problem);
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
            Some(_) => {
                if decl.scope.is_empty() {
                    report.error(
                        None,
                        format!("{location}.scope"),
                        format!("is scoped to '{parent}' but lists no namespaces; give `scope.namespaces` the namespaces it owns"),
                    );
                }
                // Reported once per pair, on the later of the two.
                for (other, sibling) in revision.fleet.fleet.clusters.iter() {
                    if other == name {
                        break;
                    }
                    if sibling.cluster.as_ref() != Some(parent) {
                        continue;
                    }
                    for ns in &decl.scope.namespaces {
                        if sibling.scope.namespaces.contains(ns) {
                            report.error(
                                None,
                                format!("{location}.scope"),
                                format!("claims the namespace '{ns}', which the scoped target '{other}' already owns; a namespace belongs to one target"),
                            );
                        }
                    }
                }
            }
        },
    }
}

/// Reports a workload placed where its objects may not be: outside the
/// namespaces of a scoped target it selects, or, on a cluster, inside the
/// namespaces of one of its scoped targets. Only what `workload.yaml` says is
/// judged here (its `namespace` and the kinds it lists); the executor holds
/// every rendered object to the same bounds when it plans.
fn validate_bounds(
    report: &mut ValidationReport,
    revision: &Revision,
    w: &LoadedWorkload,
    cluster: &str,
) {
    let bounds = revision.fleet.bounds(cluster);
    if !bounds.scoped() && bounds.excluded.is_empty() {
        return;
    }
    let cfg = &w.config;
    let name = cfg.name.as_str();
    let file = workload_file(name);
    let namespace = cfg
        .namespace
        .as_deref()
        .filter(|ns| !Templater::needs_render(ns));
    if !bounds.scoped() {
        if let Some(ns) = namespace
            && let Some(owner) = revision.fleet.fleet.clusters.iter().find_map(|(n, c)| {
                (c.cluster.as_deref() == Some(cluster)
                    && c.scope.namespaces.iter().any(|x| x == ns))
                .then_some(n)
            })
        {
            report.error(
                Some(name),
                format!("{file} namespace"),
                format!(
                    "is placed on '{cluster}' in the namespace '{ns}', which belongs to the scoped target '{owner}'; place it there (`when: 'cluster.id == \"{owner}\"'`)"
                ),
            );
        }
        return;
    }
    let owned = bounds.namespaces.join(", ");
    match (&cfg.namespace, namespace) {
        (None, _) => report.error(
            Some(name),
            format!("{file} namespace"),
            format!(
                "is placed on the scoped target '{cluster}' without a `namespace`; set it to one the target owns ({owned}), or narrow `when` to the cluster the workload belongs on"
            ),
        ),
        (Some(_), Some(ns)) if !bounds.holds(ns) => report.error(
            Some(name),
            format!("{file} namespace"),
            format!(
                "is placed on the scoped target '{cluster}' in the namespace '{ns}', which is outside it (it owns: {owned})"
            ),
        ),
        _ => {}
    }
    for entry in &cfg.resources {
        if matches!(
            entry.kind,
            ResourceKind::Policy
                | ResourceKind::Namespace
                | ResourceKind::NodePool
                | ResourceKind::Quota
        ) {
            let parent = revision
                .fleet
                .fleet
                .clusters
                .get(cluster)
                .and_then(|c| c.cluster.as_deref())
                .unwrap_or_default();
            report.error(
                Some(name),
                format!("{file} resources ({})", entry.describe()),
                format!(
                    "is cluster-wide, but '{cluster}' is a scoped target; manage it from a workload on '{parent}'"
                ),
            );
        }
    }
}

/// The selector context of the declared cluster `name`, with the labels
/// buckle.yaml gives it (labels added through the API are not known here).
fn declared_cluster_context(revision: &Revision, name: &str, decl: &ClusterDecl) -> HostContext {
    let labels = decl
        .labels
        .iter()
        .filter_map(|(k, v)| Some((k.clone(), serde_json::to_value(v).ok()?)))
        .collect();
    super::validate::cluster_context(
        revision,
        crate::selectors::HostIdentity {
            id: name.to_owned(),
            labels,
        },
        decl.kind,
    )
}

/// Whether `w`'s `when` selects the declared cluster `name`.
fn selects(revision: &Revision, w: &LoadedWorkload, name: &str, decl: &ClusterDecl) -> bool {
    selects_in(&declared_cluster_context(revision, name, decl), w)
}

/// Whether `w`'s `when` selects the cluster `ctx` describes.
fn selects_in(ctx: &HostContext, w: &LoadedWorkload) -> bool {
    match &w.config.when {
        None => true,
        Some(when) => ctx.in_module(&w.config.name, &w.config.tags).matches(when),
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

    const MIXED_FLEET: &str = "version: 2
fleet:
  clusters:
    home-nomad: { kind: nomad, nodes: 'true', labels: { site: home } }
    lab: { kind: kubernetes, nodes: 'true', labels: { site: lab } }
";

    const SCOPED_FLEET: &str = "version: 2
fleet:
  clusters:
    nomad: { kind: nomad, api: 'http://nomad:4646', nodes: 'true', executors: 'host.id == \"z2\"', identity: local }
    public: { kind: nomad, cluster: nomad, scope: { namespaces: [edge, public] } }
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
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nwhen: 'host.labels.x == 1'\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "selects on `host.*`"
    )]
    #[case::workload_namespace(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nwhen: 'workload.name == \"a\"'\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "selects on `workload.*`"
    )]
    #[case::label_not_scalar(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nlabels: { tier: [canary] }\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "labels.tier"
    )]
    #[case::missing_module(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nneeds: ['module:nope']\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "needs the module 'nope'"
    )]
    #[case::missing_workload(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nneeds: [b]\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "needs the workload 'b'"
    )]
    #[case::module_needs_missing_workload(
        &[("buckle.yaml", FLEET), ("modules/m/module.yaml", "name: m\nneeds: ['workload:nope']\n")],
        "needs the workload 'nope'"
    )]
    #[case::module_needs_missing_group(
        &[("buckle.yaml", FLEET), ("modules/m/module.yaml", "name: m\nneeds: ['group:nope']\n")],
        "needs the group 'nope'"
    )]
    #[case::workload_needs_missing_group(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nneeds: ['group:nope']\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "needs the group 'nope'"
    )]
    #[case::module_needs_missing_placement(
        &[("buckle.yaml", FLEET), ("modules/m/module.yaml", "name: m\nneeds: ['workload:nope:home-nomad']\n")],
        "needs the workload 'nope'"
    )]
    #[case::mixed_kinds(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl, a.yaml]\n"), ("workloads/a/files/a.nomad.hcl", ""), ("workloads/a/files/a.yaml", "")],
        "mixes Nomad resources"
    )]
    #[case::missing_file(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [data.volume.hcl, a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "names the file 'data.volume.hcl'"
    )]
    #[case::duplicate_entry(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl, a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "lists job a.nomad.hcl twice"
    )]
    #[case::bad_ignore_path(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [{ file: a.nomad.hcl, ignore: ['Meta['] }]\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "ignore: The field path"
    )]
    #[case::bad_template(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl.j2]\n"), ("workloads/a/files/a.nomad.hcl.j2", "{% if %}")],
        "does not compile"
    )]
    #[case::secret_in_job_spec(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl.j2]\n"), ("workloads/a/files/a.nomad.hcl.j2", "token = \"{{ secrets.TOKEN }}\"")],
        "stored by the scheduler in clear"
    )]
    #[case::undeclared_secret(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources:\n  - { secret: nomad/jobs/a, items: { token: '{{ secrets.NOPE }}' } }\n  - a.nomad.hcl\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "does not declare under `secrets`"
    )]
    #[case::config_holding_a_secret(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources:\n  - { config: nomad/jobs/a, items: { token: '{{ secrets.TOKEN }}' } }\n  - a.nomad.hcl\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "a config is stored in clear"
    )]
    #[case::config_file_holding_a_secret(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources:\n  - { config: nomad/jobs/a, files: [c.toml.j2] }\n  - a.nomad.hcl\n"), ("workloads/a/files/a.nomad.hcl", ""), ("workloads/a/files/c.toml.j2", "token = \"{{ secrets.TOKEN }}\"")],
        "a config is stored in clear"
    )]
    #[case::config_item_twice(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources:\n  - { config: c, files: [c.toml] }\n  - { config: c, items: { c.toml: x } }\n  - a.nomad.hcl\n"), ("workloads/a/files/a.nomad.hcl", ""), ("workloads/a/files/c.toml", "")],
        "sets the item 'c.toml' of config c twice"
    )]
    #[case::restart_on_unknown_resource(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl]\nrestart_on: [variables]\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "not a resource of this workload"
    )]
    #[case::workload_action_unknown_kind(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl]\nactions:\n  backfill: { systemd.restart: { name: x } }\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "not a workload action kind"
    )]
    #[case::workload_action_shadows_implicit(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl]\nactions:\n  restart: { dispatch: { job: a } }\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "implicit action"
    )]
    #[case::workload_action_bad_spec(
        &[("buckle.yaml", FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl]\nactions:\n  backfill: { dispatch: { meta: { a: b } } }\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "invalid"
    )]
    #[case::cross_layer_cycle(
        &[("buckle.yaml", FLEET), ("modules/m/module.yaml", "name: m\nneeds: ['workload:a']\n"), ("workloads/a/workload.yaml", "name: a\nneeds: ['module:m']\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "")],
        "form a cycle"
    )]
    #[case::identity_secret_undeclared(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    c:\n      kind: nomad\n      nodes: 'true'\n      identity: secret://NOPE\n")],
        "does not declare the secret 'NOPE'"
    )]
    #[case::trust_without_keys(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    c:\n      kind: nomad\n      nodes: 'true'\n      trust: { subjects: [default/x] }\n")],
        "needs `jwks_url` or `keys`"
    )]
    #[case::trust_keys_not_a_jwks(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    c:\n      kind: nomad\n      nodes: 'true'\n      trust: { keys: { kid: a } }\n")],
        "must be a JWKS document"
    )]
    #[case::trust_on_a_scoped_target(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    p: { kind: nomad, nodes: 'true' }\n    c: { kind: nomad, cluster: p, trust: { jwks_url: 'http://p:4646/.well-known/jwks.json' } }\n")],
        "set `trust` on the cluster instead"
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
    #[case::nomad_workload_on_kubernetes(
        &[("buckle.yaml", MIXED_FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "selects the kubernetes cluster 'lab'"
    )]
    #[case::kubernetes_workload_by_label(
        &[("buckle.yaml", MIXED_FLEET), ("workloads/a/workload.yaml", "name: a\nwhen: 'cluster.labels.site == \"home\"'\nresources: [a.yaml]\n"), ("workloads/a/files/a.yaml", "")],
        "selects the nomad cluster 'home-nomad'"
    )]
    #[case::pools_on_a_scoped_target(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    p: { kind: nomad, nodes: 'true' }\n    c: { kind: nomad, cluster: p, scope: { namespaces: [edge] }, pools: [edge] }\n")],
        "declare `pools` on its cluster"
    )]
    #[case::scoped_without_namespaces(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    p: { kind: nomad, nodes: 'true' }\n    c: { kind: nomad, cluster: p }\n")],
        "lists no namespaces"
    )]
    #[case::scopes_overlap(
        &[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    p: { kind: nomad, nodes: 'true' }\n    a: { kind: nomad, cluster: p, scope: { namespaces: [edge, public] } }\n    b: { kind: nomad, cluster: p, scope: { namespaces: [public] } }\n")],
        "which the scoped target 'a' already owns"
    )]
    #[case::unplaced_workload_reaches_a_scoped_target(
        &[("buckle.yaml", SCOPED_FLEET), ("workloads/a/workload.yaml", "name: a\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "on the scoped target 'public' without a `namespace`"
    )]
    #[case::namespace_outside_the_scope(
        &[("buckle.yaml", SCOPED_FLEET), ("workloads/a/workload.yaml", "name: a\nwhen: 'cluster.id == \"public\"'\nnamespace: internal\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "which is outside it"
    )]
    #[case::cluster_wide_object_on_a_scoped_target(
        &[("buckle.yaml", SCOPED_FLEET), ("workloads/a/workload.yaml", "name: a\nwhen: 'cluster.id == \"public\"'\nnamespace: edge\nresources: [edge.namespace.hcl]\n"), ("workloads/a/files/edge.namespace.hcl", "namespace {}")],
        "manage it from a workload on 'nomad'"
    )]
    #[case::cluster_workload_inside_a_scope(
        &[("buckle.yaml", SCOPED_FLEET), ("workloads/a/workload.yaml", "name: a\nwhen: 'cluster.id == \"nomad\"'\nnamespace: edge\nresources: [a.nomad.hcl]\n"), ("workloads/a/files/a.nomad.hcl", "job {}")],
        "which belongs to the scoped target 'public'"
    )]
    fn invalid_configurations_are_reported(#[case] files: &[(&str, &str)], #[case] needle: &str) {
        let errors = errors(files);
        assert!(
            errors.iter().any(|e| e.contains(needle)),
            "expected {needle:?} in {errors:#?}"
        );
    }

    #[test]
    fn a_scoped_target_takes_what_it_leaves_out_from_its_cluster() {
        let rev = load(&[
            ("buckle.yaml", SCOPED_FLEET),
            (
                "workloads/platform/workload.yaml",
                "name: platform\nwhen: 'cluster.id == \"nomad\"'\nresources: [edge.namespace.hcl]\n",
            ),
            (
                "workloads/platform/files/edge.namespace.hcl",
                "namespace {}",
            ),
            (
                "workloads/site/workload.yaml",
                "name: site\nwhen: 'cluster.id == \"public\"'\nnamespace: edge\nneeds: ['workload:platform:nomad']\nresources: [site.nomad.hcl]\n",
            ),
            ("workloads/site/files/site.nomad.hcl", "job {}"),
        ]);
        let report = validate_targets(&rev);
        assert!(report.errors.is_empty(), "{:#?}", report.errors);

        let (parent, scoped) = (
            &rev.fleet.fleet.clusters["nomad"],
            &rev.fleet.fleet.clusters["public"],
        );
        assert_eq!(scoped.api, parent.api);
        assert_eq!(scoped.identity, parent.identity);
        assert_eq!(
            scoped.executors().map(|f| f.raw()),
            parent.executors().map(|f| f.raw())
        );
        assert_eq!(rev.fleet.bounds("public").namespaces, ["edge", "public"]);
        assert_eq!(rev.fleet.bounds("nomad").excluded, ["edge", "public"]);
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
resources:
  - paste.volume.hcl
  - { secret: nomad/jobs/paste, items: { db: '{{ secrets.DB }}' } }
  - { file: paste.nomad.hcl.j2, live_vars: [version], ignore: ['Meta[autoupdate.*]'] }
restart_on: [nomad/jobs/paste]
actions:
  backfill: { dispatch: { job: paste-maintenance, meta: { task: backfill } }, confirm: true }
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

    #[rstest]
    #[case::by_kind("name: a\nwhen: 'cluster.kind == \"nomad\"'\nresources: [a.nomad.hcl]\n")]
    #[case::by_label(
        "name: a\nwhen: 'cluster.labels.site == \"home\"'\nresources: [a.nomad.hcl]\n"
    )]
    #[case::secrets_alone("name: a\nresources: [{ secret: s, items: { A: x } }]\n")]
    fn workloads_narrowed_to_their_scheduler_are_valid(#[case] workload: &str) {
        let errors = errors(&[
            ("buckle.yaml", MIXED_FLEET),
            ("workloads/a/workload.yaml", workload),
            ("workloads/a/files/a.nomad.hcl", "job {}"),
        ]);
        assert_eq!(errors, Vec::<String>::new());
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
    fn the_first_schemas_keys_and_a_job_before_its_entities_warn() {
        let rev = load(&[
            ("buckle.yaml", FLEET),
            (
                "workloads/a/workload.yaml",
                "name: a\njob: a.nomad.hcl\nvolumes: [a.volume.hcl]\n",
            ),
            ("workloads/a/files/a.nomad.hcl", ""),
            ("workloads/a/files/a.volume.hcl", ""),
            (
                "workloads/b/workload.yaml",
                "name: b\nresources: [b.nomad.hcl, b.volume.hcl]\n",
            ),
            ("workloads/b/files/b.nomad.hcl", ""),
            ("workloads/b/files/b.volume.hcl", ""),
        ]);
        let report = validate_targets(&rev);
        assert!(report.is_ok(), "{report}");
        let warnings: Vec<String> = report.warnings.iter().map(|w| w.to_string()).collect();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("deprecated keys `volumes`, `job`")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("job b.nomad.hcl before volume b.volume.hcl")),
            "{warnings:?}"
        );
    }

    #[test]
    fn workloads_select_clusters_by_their_context() {
        let rev = load(&[
            ("buckle.yaml", FLEET),
            (
                "workloads/all/workload.yaml",
                "name: all\nresources: [a.nomad.hcl]\n",
            ),
            (
                "workloads/k8s/workload.yaml",
                "name: k8s\nwhen: 'cluster.kind == \"kubernetes\"'\nresources: [a.yaml]\n",
            ),
            ("workloads/all/files/a.nomad.hcl", ""),
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

    /// A placement needing a workload its cluster rolls out in a later phase
    /// is a warning (it waits for the need to be running) or, under
    /// `dependencies.across_phases: refuse`, an error.
    #[test]
    fn needs_reach_groups_and_single_placements() {
        let fleet = "version: 2
fleet:
  groups:
    servers: 'host.labels.role == \"server\"'
  clusters:
    home-nomad: { kind: nomad, nodes: 'true' }
";
        let rev = load(&[
            ("buckle.yaml", fleet),
            (
                "modules/agent/module.yaml",
                "name: agent\nneeds: ['group:servers', 'workload:a:home-nomad', 'workload:a:elsewhere']\nparallel: 50%\nmax_failures: 1\n",
            ),
            (
                "workloads/a/workload.yaml",
                "name: a\nneeds: ['group:servers']\nparallel: 1\nresources: [a.nomad.hcl]\n",
            ),
            ("workloads/a/files/a.nomad.hcl", "job {}"),
        ]);
        let report = validate_targets(&rev);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        // The one cluster buckle.yaml does not declare is pointed out.
        let warnings: Vec<String> = report.warnings.iter().map(|w| w.to_string()).collect();
        assert!(
            warnings.iter().any(|w| w.contains("'elsewhere'")),
            "{warnings:?}"
        );
        assert!(
            !warnings.iter().any(|w| w.contains("'home-nomad'")),
            "{warnings:?}"
        );
        let agent = &rev.module("agent").unwrap().config;
        assert_eq!(agent.parallel, Some(crate::model::Parallelism::Percent(50)));
        assert_eq!(
            agent.max_failures,
            Some(crate::model::Parallelism::Count(1))
        );
        assert_eq!(
            rev.workload("a").unwrap().config.parallel,
            Some(crate::model::Parallelism::Count(1))
        );
    }

    #[rstest]
    #[case::warn("", false)]
    #[case::refuse("  dependencies: { across_phases: refuse }\n", true)]
    fn a_need_on_a_later_phase_warns_or_is_refused(#[case] policy: &str, #[case] refused: bool) {
        let fleet = format!(
            "version: 2\nfleet:\n  clusters:\n    home-nomad: {{ kind: nomad }}\nrollout:\n  phases:\n    - {{ name: canary, select: 'workload.labels.tier == \"canary\"' }}\n    - {{ name: all, select: 'true' }}\n{policy}"
        );
        let rev = load(&[
            ("buckle.yaml", fleet.as_str()),
            (
                "workloads/app/workload.yaml",
                "name: app\nlabels: { tier: canary }\nneeds: [postgres]\nresources: [app.nomad.hcl]\n",
            ),
            ("workloads/app/files/app.nomad.hcl", "job {}"),
            (
                "workloads/postgres/workload.yaml",
                "name: postgres\nresources: [postgres.nomad.hcl]\n",
            ),
            ("workloads/postgres/files/postgres.nomad.hcl", "job {}"),
        ]);
        let report = validate_targets(&rev);
        let (found, other) = if refused {
            (&report.errors, &report.warnings)
        } else {
            (&report.warnings, &report.errors)
        };
        assert!(
            found
                .iter()
                .any(|i| i.to_string().contains("later phase 'all'")),
            "{report}"
        );
        assert!(
            !other.iter().any(|i| i.to_string().contains("later phase")),
            "{report}"
        );
    }
}
