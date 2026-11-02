//! Reading a rollout's fleet-wide plan: which hosts a line applies to.

use super::format::count;

/// Whether `hosts` covers every one of `all`, and how to say who it covers:
/// `all 5 hosts`, a short list of names, or `3 of 5 hosts`.
pub fn scope_label(all: &[String], hosts: &[String]) -> (bool, String) {
    let fleet_wide = !all.is_empty() && all.iter().all(|h| hosts.contains(h));
    let label = match (fleet_wide, all.len(), hosts.len()) {
        (true, 1, _) => all[0].clone(),
        (true, n, _) => format!("all {n} hosts"),
        (false, _, 1 | 2) => hosts.join(", "),
        (false, n, k) => format!("{k} of {}", count(n, "host", "hosts")),
    };
    (fleet_wide, label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn scope_reads_as_all_hosts_or_names_the_subset() {
        let all = names(&["a", "b", "c", "d"]);
        assert_eq!(scope_label(&all, &all), (true, "all 4 hosts".into()));
        assert_eq!(
            scope_label(&all, &names(&["a", "c"])),
            (false, "a, c".into())
        );
        assert_eq!(
            scope_label(&all, &names(&["a", "b", "c"])),
            (false, "3 of 4 hosts".into())
        );
        assert!(scope_label(&names(&["a"]), &names(&["a"])).0);
    }
}
