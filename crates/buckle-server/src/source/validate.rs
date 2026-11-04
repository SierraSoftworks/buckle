//! Validation of a loaded revision beyond what loading itself checks:
//! module graph, rollout phase names, secret references, template syntax and
//! the cluster layer (`fleet.clusters`, workloads, cross-layer `needs`).

use crate::secrets::ServerSecrets;
use buckle_core::engine::Revision;
use std::collections::{BTreeMap, BTreeSet};

/// Every problem found; empty means valid.
pub fn validate(revision: &Revision) -> Vec<String> {
    let mut errors = Vec::new();
    let mut names = BTreeSet::new();
    for m in &revision.modules {
        if !names.insert(m.config.name.clone()) {
            errors.push(format!(
                "Module '{}' is defined more than once.",
                m.config.name
            ));
        }
    }
    let graph: BTreeMap<&str, Vec<String>> = revision
        .modules
        .iter()
        .map(|m| (m.config.name.as_str(), m.config.module_needs()))
        .collect();
    for (module, needs) in &graph {
        for n in needs.iter() {
            if !graph.contains_key(n.as_str()) {
                errors.push(format!(
                    "Module '{module}' needs '{n}', which does not exist."
                ));
            }
        }
    }
    if let Some(cycle) = find_cycle(&graph) {
        errors.push(format!(
            "The module graph has a cycle: {}.",
            cycle.join(" → ")
        ));
    }
    let mut phases = BTreeSet::new();
    for phase in &revision.fleet.rollout.phases {
        let name = phase.name.trim();
        if name.is_empty() {
            errors.push("A rollout phase has no name.".to_owned());
        } else if name.eq_ignore_ascii_case(crate::rollout::model::STRAGGLERS) {
            errors.push(format!(
                "The rollout phase name '{name}' is reserved for the stage that converges hosts which missed their phase."
            ));
        } else if !phases.insert(name.to_ascii_lowercase()) {
            errors.push(format!(
                "The rollout phase '{name}' is defined more than once; phase names must be unique."
            ));
        }
    }
    let declared: BTreeSet<&str> = revision.fleet.secrets.keys().map(String::as_str).collect();
    for m in &revision.modules {
        let mut text = serde_yaml::to_string(&m.config).unwrap_or_default();
        for (_, t) in &m.templates {
            text.push_str(t);
        }
        for name in ServerSecrets::references(&text) {
            if !declared.contains(name.as_str()) {
                errors.push(format!(
                    "Module '{}' references secrets.{name}, which buckle.yaml does not declare.",
                    m.config.name
                ));
            }
        }
        let env = minijinja::Environment::new();
        for (path, source) in &m.templates {
            if let Err(e) = env.template_from_str(source) {
                errors.push(format!(
                    "The template {}/files/{} does not parse: {e}",
                    m.config.name,
                    path.display()
                ));
            }
        }
    }
    let targets = buckle_core::engine::validate_targets(revision);
    errors.extend(targets.errors.iter().map(|e| format!("{e}.")));
    errors
}

fn find_cycle(graph: &BTreeMap<&str, Vec<String>>) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    fn visit<'a>(
        node: &'a str,
        graph: &'a BTreeMap<&'a str, Vec<String>>,
        marks: &mut BTreeMap<&'a str, Mark>,
        path: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match marks.get(node) {
            Some(Mark::Done) => return None,
            Some(Mark::Visiting) => {
                let start = path.iter().position(|n| *n == node).unwrap_or(0);
                let mut cycle: Vec<String> = path[start..].iter().map(|s| s.to_string()).collect();
                cycle.push(node.to_owned());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(node, Mark::Visiting);
        path.push(node);
        if let Some(needs) = graph.get(node) {
            for n in needs.iter() {
                if graph.contains_key(n.as_str())
                    && let Some(c) = visit(n.as_str(), graph, marks, path)
                {
                    return Some(c);
                }
            }
        }
        path.pop();
        marks.insert(node, Mark::Done);
        None
    }
    let mut marks = BTreeMap::new();
    for node in graph.keys() {
        if let Some(c) = visit(node, graph, &mut marks, &mut Vec::new()) {
            return Some(c);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_core::plan::RevisionId;
    use rstest::rstest;

    fn load(files: &[(&str, &str)]) -> Revision {
        let dir = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let path = dir.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, c).unwrap();
        }
        buckle_core::engine::loader::load_dir(dir.path(), RevisionId::new("t")).unwrap()
    }

    #[rstest]
    #[case(&[("buckle.yaml", "version: 2\n"), ("modules/a/module.yaml", "name: a\nneeds: [b]\n"), ("modules/b/module.yaml", "name: b\nneeds: [a]\n")], "cycle")]
    #[case(&[("buckle.yaml", "version: 2\n"), ("modules/a/module.yaml", "name: a\nneeds: [zzz]\n")], "does not exist")]
    #[case(&[("buckle.yaml", "version: 2\n"), ("modules/a/module.yaml", "name: a\n"), ("modules/a/files/x.j2", "{{ secrets.nope }}")], "does not declare")]
    #[case(&[("buckle.yaml", "version: 2\n"), ("modules/a/module.yaml", "name: a\n"), ("modules/a/files/x.j2", "{% if %}")], "does not parse")]
    #[case(&[("buckle.yaml", "version: 2\nrollout:\n  phases:\n    - { name: canary, select: 'true' }\n    - { name: Canary, select: 'true' }\n")], "more than once")]
    #[case(&[("buckle.yaml", "version: 2\nrollout:\n  phases:\n    - { name: stragglers, select: 'true' }\n")], "reserved")]
    #[case(&[("buckle.yaml", "version: 2\n"), ("modules/a/module.yaml", "name: a\n"), ("modules/a/files/x.j2", "{{ secrets[\"nope\"] }}")], "does not declare")]
    #[case(&[("buckle.yaml", "version: 2\n"), ("modules/a/module.yaml", "name: a\nneeds: ['workload:nope']\n")], "needs the workload 'nope'")]
    #[case(&[("buckle.yaml", "version: 2\n"), ("workloads/w/workload.yaml", "name: w\nwhen: 'host.labels.x == 1'\njob: w.hcl\n"), ("workloads/w/files/w.hcl", "")], "selects on `host.*`")]
    #[case(&[("buckle.yaml", "version: 2\nfleet:\n  clusters:\n    c:\n      kind: nomad\n      nodes: 'true'\n      identity: secret://NOPE\n")], "does not declare the secret 'NOPE'")]
    fn invalid_revisions_are_reported(#[case] files: &[(&str, &str)], #[case] needle: &str) {
        let errors = validate(&load(files));
        assert!(errors.iter().any(|e| e.contains(needle)), "{errors:?}");
    }

    #[test]
    fn a_valid_revision_has_no_errors() {
        let rev = load(&[
            (
                "buckle.yaml",
                "version: 2\nfleet:\n  clusters:\n    c: { kind: nomad, nodes: 'host.labels.nomad == true' }\nsecrets:\n  tok: { env: TOK }\n",
            ),
            (
                "modules/a/module.yaml",
                "name: a\nneeds: [b, 'workload:w']\n",
            ),
            ("modules/b/module.yaml", "name: b\n"),
            ("modules/a/files/x.j2", "{{ secrets.tok }}"),
            (
                "workloads/w/workload.yaml",
                "name: w\nneeds: ['module:b']\njob: w.hcl\n",
            ),
            ("workloads/w/files/w.hcl", "job {}"),
        ]);
        assert_eq!(validate(&rev), Vec::<String>::new());
    }
}
