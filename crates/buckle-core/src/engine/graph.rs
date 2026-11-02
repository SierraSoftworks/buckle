//! The module dependency graph: deterministic topological order and cycle
//! detection over `needs` edges.

use std::collections::{BTreeMap, BTreeSet};

/// A dependency graph over named nodes. Edges point from a node to the nodes
/// it needs; edges to unknown nodes are ignored (validation reports them).
#[derive(Debug, Default, Clone)]
pub struct Graph {
    needs: BTreeMap<String, Vec<String>>,
}

impl Graph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, name: &str, needs: &[String]) {
        self.needs.insert(name.to_owned(), needs.to_vec());
    }

    pub fn needs(&self, name: &str) -> &[String] {
        self.needs.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The direct dependencies that are nodes of this graph.
    fn known_needs<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a String> + 'a {
        self.needs(name)
            .iter()
            .filter(move |n| self.needs.contains_key(n.as_str()))
    }

    /// A topological order (dependencies first) that is deterministic: within a
    /// level (nodes whose dependencies are all in earlier levels) nodes are
    /// sorted by name. Returns the cycle when the graph has one.
    pub fn topo_order(&self) -> Result<Vec<String>, Vec<String>> {
        let mut remaining: BTreeSet<&str> = self.needs.keys().map(String::as_str).collect();
        let mut done: BTreeSet<&str> = BTreeSet::new();
        let mut order = Vec::with_capacity(remaining.len());
        while !remaining.is_empty() {
            let level: Vec<&str> = remaining
                .iter()
                .copied()
                .filter(|n| self.known_needs(n).all(|d| done.contains(d.as_str())))
                .collect();
            if level.is_empty() {
                return Err(self.find_cycle().unwrap_or_default());
            }
            for n in level {
                remaining.remove(n);
                done.insert(n);
                order.push(n.to_owned());
            }
        }
        Ok(order)
    }

    /// Finds one cycle, as the list of nodes along it (first node repeated at the end).
    pub fn find_cycle(&self) -> Option<Vec<String>> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Visiting,
            Done,
        }
        let mut marks: BTreeMap<&str, Mark> = BTreeMap::new();
        let mut stack: Vec<&str> = Vec::new();

        fn visit<'a>(
            g: &'a Graph,
            node: &'a str,
            marks: &mut BTreeMap<&'a str, Mark>,
            stack: &mut Vec<&'a str>,
        ) -> Option<Vec<String>> {
            match marks.get(node) {
                Some(Mark::Done) => return None,
                Some(Mark::Visiting) => {
                    let start = stack.iter().position(|n| *n == node).unwrap_or(0);
                    let mut cycle: Vec<String> =
                        stack[start..].iter().map(|s| s.to_string()).collect();
                    cycle.push(node.to_owned());
                    return Some(cycle);
                }
                None => {}
            }
            marks.insert(node, Mark::Visiting);
            stack.push(node);
            for dep in g.known_needs(node) {
                if let Some(cycle) = visit(g, dep.as_str(), marks, stack) {
                    return Some(cycle);
                }
            }
            stack.pop();
            marks.insert(node, Mark::Done);
            None
        }

        for node in self.needs.keys() {
            if let Some(cycle) = visit(self, node.as_str(), &mut marks, &mut stack) {
                return Some(cycle);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(&str, &[&str])]) -> Graph {
        let mut g = Graph::new();
        for (n, needs) in edges {
            g.add(n, &needs.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        }
        g
    }

    #[test]
    fn topo_order_is_dependency_first_and_name_sorted_within_levels() {
        let g = graph(&[
            ("nomad", &["tailscale", "docker"]),
            ("docker", &[]),
            ("tailscale", &["common"]),
            ("common", &[]),
            ("zeta", &[]),
        ]);
        assert_eq!(
            g.topo_order().unwrap(),
            vec!["common", "docker", "zeta", "tailscale", "nomad"]
        );
    }

    #[test]
    fn cycles_are_reported_with_their_path() {
        let g = graph(&[("a", &["b"]), ("b", &["c"]), ("c", &["a"]), ("d", &[])]);
        let cycle = g.topo_order().unwrap_err();
        assert_eq!(cycle.first(), cycle.last());
        assert_eq!(cycle.len(), 4);
        assert!(cycle.contains(&"a".to_string()) && cycle.contains(&"c".to_string()));
    }

    #[test]
    fn unknown_dependencies_are_ignored_for_ordering() {
        let g = graph(&[("a", &["missing"]), ("b", &["a"])]);
        assert_eq!(g.topo_order().unwrap(), vec!["a", "b"]);
    }
}
