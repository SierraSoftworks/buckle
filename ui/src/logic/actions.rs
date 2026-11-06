//! How a target's actions (`HostDetail.actions`) are offered: which ones a
//! module head shows, how each is labelled, and what a click does.

use buckle_api::{ActionInfo, FleetAction};

/// What clicking an action does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Click {
    /// Runs it at once.
    Run,
    /// Opens a confirmation that asks for a note for the audit log.
    Confirm,
}

pub fn click(a: &ActionInfo) -> Click {
    if a.confirm {
        Click::Confirm
    } else {
        Click::Run
    }
}

/// The action's label on a button or menu item: an ellipsis when it asks
/// for confirmation. Where a list spans modules, the module follows as the
/// item's muted aside.
pub fn label(a: &ActionInfo) -> String {
    match click(a) {
        Click::Confirm => format!("{}…", a.label),
        Click::Run => a.label.clone(),
    }
}

/// The actions of `module`, in declaration order.
pub fn of_module(actions: &[ActionInfo], module: &str) -> Vec<ActionInfo> {
    actions
        .iter()
        .filter(|a| a.module == module)
        .cloned()
        .collect()
}

/// Whether a list names more than one module (its labels say which).
pub fn spans_modules(actions: &[ActionInfo]) -> bool {
    actions
        .first()
        .is_some_and(|first| actions.iter().any(|a| a.module != first.module))
}

/// Names a fleet action in the Run action picker: `module/name`.
pub fn fleet_key(f: &FleetAction) -> String {
    format!("{}/{}", f.action.module, f.action.name)
}

/// The Run action dialog's button: what starts, on how many hosts (or, for
/// a workload's action, clusters).
pub fn rollout_label(f: &FleetAction) -> String {
    let n = f.targets.len();
    let (one, many) = if f.workload {
        ("cluster", "clusters")
    } else {
        ("host", "hosts")
    };
    format!(
        "Run {} on {n} {}",
        f.action.label,
        if n == 1 { one } else { many }
    )
}

/// A workload-wide action's label: it always opens a dialog (it starts a
/// rollout across the workload's placements).
pub fn rollout_item_label(a: &ActionInfo) -> String {
    format!("{}…", a.label)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(module: &str, label: &str, confirm: bool) -> ActionInfo {
        ActionInfo {
            module: module.into(),
            name: label.to_lowercase(),
            label: label.into(),
            kind: "command.run".into(),
            description: None,
            confirm,
            implicit: false,
        }
    }

    #[test]
    fn actions_are_labelled_by_what_a_click_does() {
        for (confirm, expected, does) in [
            (false, "Restart", Click::Run),
            (true, "Restart…", Click::Confirm),
        ] {
            let a = action("web", "Restart", confirm);
            assert_eq!(label(&a), expected);
            assert_eq!(click(&a), does);
        }
    }

    #[test]
    fn module_heads_show_their_own_actions_and_lists_say_when_they_span_modules() {
        let all = vec![
            action("web", "Restart", false),
            action("db", "Vacuum", true),
            action("web", "Drain", true),
        ];
        let web: Vec<String> = of_module(&all, "web")
            .into_iter()
            .map(|a| a.label)
            .collect();
        assert_eq!(web, vec!["Restart", "Drain"]);
        assert!(spans_modules(&all));
        assert!(!spans_modules(&of_module(&all, "web")));
        assert!(!spans_modules(&[]));
    }

    #[test]
    fn a_fleet_action_says_what_starts_and_where() {
        for (targets, workload, expected) in [
            (vec!["a"], false, "Run Restart on 1 host"),
            (vec!["a", "b"], false, "Run Restart on 2 hosts"),
            (vec!["a", "b"], true, "Run Restart on 2 clusters"),
        ] {
            let f = FleetAction {
                action: action("web", "Restart", false),
                workload,
                targets: targets.into_iter().map(str::to_owned).collect(),
            };
            assert_eq!(rollout_label(&f), expected);
            assert_eq!(fleet_key(&f), "web/restart");
        }
    }
}
