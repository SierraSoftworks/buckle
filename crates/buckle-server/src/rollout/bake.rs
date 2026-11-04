//! How long a target bakes after a run: the phase's `bake` as the minimum,
//! raised by the `bake` of each module (or workload) the run changed, and
//! nothing at all when the run changed nothing.

use std::collections::BTreeSet;
use std::time::Duration;

use buckle_core::engine::ApplyOutcome;
use buckle_core::plan::Plan;

pub use buckle_api::rollout::BakeWindow;

/// Why a step bakes for as long as it does, for its transition.
pub fn reason(w: &BakeWindow) -> String {
    match (&w.module, w.unchanged) {
        (_, true) => "applied; nothing changed, so no bake".to_owned(),
        (Some(m), _) => format!(
            "applied; baking {} for {m}",
            humantime_serde::re::humantime::format_duration(Duration::from_secs(w.seconds))
        ),
        (None, _) => "applied; baking".to_owned(),
    }
}

/// The modules (or workloads) a run changed: those with changed steps and
/// those whose resources it removed.
pub fn changed(outcome: &ApplyOutcome) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = outcome
        .module_results
        .iter()
        .filter(|m| m.changes > 0)
        .map(|m| m.module.clone())
        .collect();
    out.extend(
        outcome
            .removals_run
            .iter()
            .filter(|r| r.state == buckle_core::engine::StepState::Changed)
            .filter_map(|r| r.resource.split_once('/').map(|(m, _)| m.to_owned())),
    );
    out
}

/// The modules a plan expects to change: those with changes or removals,
/// and the deferred ones (whose changes are not known until they apply).
pub fn planned(plan: &Plan) -> BTreeSet<String> {
    plan.modules
        .iter()
        .filter(|m| m.changes() > 0)
        .map(|m| m.module.clone())
        .chain(plan.removals.iter().map(|r| r.resource.module.clone()))
        .chain(plan.deferred.iter().map(|d| d.module.clone()))
        .collect()
}

/// The window for a step that changed `changed`: at least `min` seconds,
/// raised by the longest `bake_of` among them, and zero when nothing
/// changed.
pub fn window(
    min: u64,
    changed: &BTreeSet<String>,
    bake_of: impl Fn(&str) -> Option<Duration>,
) -> BakeWindow {
    if changed.is_empty() {
        return BakeWindow {
            unchanged: true,
            ..Default::default()
        };
    }
    let longest = changed
        .iter()
        .filter_map(|m| bake_of(m).map(|b| (b.as_secs(), m)))
        .filter(|(secs, _)| *secs > min)
        .max_by_key(|(secs, _)| *secs);
    match longest {
        Some((seconds, m)) => BakeWindow {
            seconds,
            module: Some(m.clone()),
            unchanged: false,
        },
        None => BakeWindow {
            seconds: min,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn bakes(name: &str) -> Option<Duration> {
        match name {
            "nomad" => Some(Duration::from_secs(900)),
            "consul" => Some(Duration::from_secs(600)),
            "shell" => Some(Duration::from_secs(10)),
            _ => None,
        }
    }

    #[test]
    fn window_is_the_phase_minimum_raised_by_changed_modules() {
        for (changed, seconds, module) in [
            (vec!["shell"], 60, None),
            (vec!["atuin"], 60, None),
            (vec!["shell", "consul"], 600, Some("consul")),
            (vec!["consul", "nomad", "shell"], 900, Some("nomad")),
        ] {
            let w = window(60, &set(&changed), bakes);
            assert_eq!(w.seconds, seconds, "{changed:?}");
            assert_eq!(w.module.as_deref(), module, "{changed:?}");
            assert!(!w.unchanged);
        }
    }

    #[test]
    fn an_unchanged_step_does_not_bake() {
        let w = window(300, &set(&[]), bakes);
        assert_eq!(w.seconds, 0);
        assert!(w.unchanged);
    }
}
