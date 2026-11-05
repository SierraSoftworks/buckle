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
/// for confirmation, the module first when the list spans modules.
pub fn label(a: &ActionInfo, qualified: bool) -> String {
    let base = match click(a) {
        Click::Confirm => format!("{}…", a.label),
        Click::Run => a.label.clone(),
    };
    if qualified {
        format!("{} · {base}", a.module)
    } else {
        base
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

/// The Run action dialog's button: what starts, on how many targets.
pub fn rollout_label(f: &FleetAction) -> String {
    let n = f.targets.len();
    format!(
        "Run {} on {n} {}",
        f.action.label,
        if n == 1 { "target" } else { "targets" }
    )
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
        for (confirm, qualified, expected, does) in [
            (false, false, "Restart", Click::Run),
            (true, false, "Restart…", Click::Confirm),
            (false, true, "web · Restart", Click::Run),
        ] {
            let a = action("web", "Restart", confirm);
            assert_eq!(label(&a, qualified), expected);
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
        for (targets, expected) in [
            (vec!["a"], "Run Restart on 1 target"),
            (vec!["a", "b"], "Run Restart on 2 targets"),
        ] {
            let f = FleetAction {
                action: action("web", "Restart", false),
                targets: targets.into_iter().map(str::to_owned).collect(),
            };
            assert_eq!(rollout_label(&f), expected);
            assert_eq!(fleet_key(&f), "web/restart");
        }
    }
}
