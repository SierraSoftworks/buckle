//! A host's blueprint: what Buckle manages there, module by module, with each
//! resource's current state and whether removing it from the revision would
//! clean it up. Resources come from the applied plan; their state from the
//! latest run (failed, blocked, still applying) and the latest drift check,
//! and resources the latest run is removing join their module, tagged for
//! cleanup, until the removal finishes.

use buckle_api::{DriftItem, ResourceSummary, RunReport, StepState};

use super::run::resource_module;
use super::tone::Tone;

/// Where one resource stands now, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourceState {
    Failed,
    Blocked,
    Drifted,
    RolledBack,
    Applying,
    Removing,
    Pending,
    InSync,
}

impl ResourceState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Drifted => "drifted",
            Self::RolledBack => "rolled back",
            Self::Applying => "applying",
            Self::Removing => "removing",
            Self::Pending => "pending",
            Self::InSync => "in sync",
        }
    }

    /// The BEM modifier and `data-state` (`run-step--in-sync`).
    pub fn slug(self) -> &'static str {
        match self {
            Self::RolledBack => "rolled-back",
            Self::InSync => "in-sync",
            other => other.label(),
        }
    }

    pub fn tone(self) -> Tone {
        match self {
            Self::Failed => Tone::Danger,
            Self::Blocked | Self::Drifted => Tone::Warn,
            Self::RolledBack => Tone::Mute,
            Self::Applying | Self::Removing => Tone::Active,
            Self::Pending => Tone::Neutral,
            Self::InSync => Tone::Ok,
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Failed => "✕",
            Self::Blocked => "!",
            Self::Drifted => "≠",
            Self::RolledBack => "↶",
            Self::Applying => "◉",
            Self::Removing => "−",
            Self::Pending => "○",
            Self::InSync => "✓",
        }
    }

    /// Something an operator should read: opens its module by default.
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            Self::Failed | Self::Blocked | Self::Drifted | Self::RolledBack
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub kind: String,
    pub key: String,
    pub state: ResourceState,
    /// Why it is in that state: the drift found, the step's error or reason.
    pub note: Option<String>,
    /// Removing it from the revision deletes it (`cleanup`), rather than
    /// Buckle only forgetting it (`leave`).
    pub cleanup: bool,
    /// The revision that applied it and when; absent for removals.
    pub applied: Option<(String, chrono::DateTime<chrono::Utc>)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub name: String,
    pub resources: Vec<Resource>,
}

impl Module {
    /// The worst state of its resources.
    pub fn state(&self) -> ResourceState {
        self.resources
            .iter()
            .map(|r| r.state)
            .min()
            .unwrap_or(ResourceState::InSync)
    }

    pub fn cleanup(&self) -> usize {
        self.resources.iter().filter(|r| r.cleanup).count()
    }
}

/// The host's blueprint, modules in plan order: its owned `resources`, read
/// against the latest `drift` check and its latest `run`.
pub fn blueprint(
    resources: &[ResourceSummary],
    drift: &[DriftItem],
    run: Option<&RunReport>,
) -> Vec<Module> {
    let step = |module: &str, id: &str| {
        run.into_iter()
            .flat_map(|r| &r.modules)
            .filter(|m| m.name == module)
            .flat_map(|m| &m.steps)
            .find(|s| s.id == id)
    };
    let mut modules: Vec<Module> = Vec::new();
    let mut push =
        |module: &str, resource: Resource| match modules.iter_mut().find(|m| m.name == module) {
            Some(m) => m.resources.push(resource),
            None => modules.push(Module {
                name: module.to_string(),
                resources: vec![resource],
            }),
        };

    for r in resources {
        let id = format!("{}[{}]", r.kind, r.key);
        let drift = drift
            .iter()
            .find(|x| x.module == r.module && x.kind == r.kind && x.key == r.key);
        let ran = step(&r.module, &id).and_then(|s| {
            let state = match s.state {
                StepState::Failed => ResourceState::Failed,
                StepState::Blocked => ResourceState::Blocked,
                StepState::RolledBack => ResourceState::RolledBack,
                StepState::Running => ResourceState::Applying,
                StepState::Pending | StepState::NotRun => ResourceState::Pending,
                StepState::Changed | StepState::Unchanged | StepState::Skipped => return None,
            };
            let note = s
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .or_else(|| (!s.summary.is_empty()).then(|| s.summary.clone()));
            Some((state, note))
        });
        let (state, note) = match (ran, drift) {
            (Some(ran), _) => ran,
            (None, Some(x)) => (ResourceState::Drifted, Some(x.summary.clone())),
            (None, None) => (ResourceState::InSync, None),
        };
        push(
            &r.module,
            Resource {
                kind: r.kind.clone(),
                key: r.key.clone(),
                state,
                note,
                cleanup: r.removal != "leave",
                applied: Some((r.revision.clone(), r.applied_at)),
            },
        );
    }

    for x in run.into_iter().flat_map(|r| &r.removals) {
        let state = match x.state {
            StepState::Failed => ResourceState::Failed,
            StepState::Blocked => ResourceState::Blocked,
            StepState::Pending | StepState::Running => ResourceState::Removing,
            _ => continue,
        };
        let module = resource_module(&x.resource).unwrap_or_default();
        let key = super::run::step_key(&x.kind, &x.resource).to_string();
        push(
            module,
            Resource {
                kind: x.kind.clone(),
                key,
                state,
                note: x
                    .error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .or_else(|| Some(x.summary.clone()).filter(|s| !s.is_empty())),
                cleanup: true,
                applied: None,
            },
        );
    }
    modules
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_api::{ModuleReport, ModuleState, RemovalReport};
    use chrono::Utc;

    fn resource(module: &str, key: &str, removal: &str) -> ResourceSummary {
        ResourceSummary {
            module: module.into(),
            kind: "file.copy".into(),
            key: key.into(),
            revision: "abc".into(),
            applied_at: Utc::now(),
            removal: removal.into(),
        }
    }

    fn run(modules: Vec<ModuleReport>, removals: Vec<RemovalReport>) -> RunReport {
        let mut r = crate::logic::run::tests::report(modules);
        r.removals = removals;
        r
    }

    fn module(name: &str, steps: Vec<buckle_api::StepReport>) -> ModuleReport {
        crate::logic::run::tests::module(name, ModuleState::Applied, steps)
    }

    fn state_of(modules: &[Module], key: &str) -> ResourceState {
        modules
            .iter()
            .flat_map(|m| &m.resources)
            .find(|r| r.key == key)
            .map(|r| r.state)
            .expect("resource in the blueprint")
    }

    #[test]
    fn groups_resources_by_module_in_order_with_cleanup_tags() {
        let owned = [
            resource("common", "/a", "cleanup"),
            resource("nomad", "/b", "leave"),
            resource("common", "/c", "leave"),
        ];
        let b = blueprint(&owned, &[], None);
        let names: Vec<&str> = b.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["common", "nomad"]);
        assert_eq!(b[0].resources.len(), 2);
        assert_eq!(b[0].cleanup(), 1);
        assert_eq!(b[1].cleanup(), 0);
        assert_eq!(b[0].state(), ResourceState::InSync);
    }

    #[test]
    fn state_follows_the_latest_run_then_drift() {
        use crate::logic::run::tests::step;
        let cases = [
            (Some(StepState::Failed), false, ResourceState::Failed),
            (Some(StepState::Blocked), true, ResourceState::Blocked),
            (Some(StepState::Running), false, ResourceState::Applying),
            (Some(StepState::NotRun), false, ResourceState::Pending),
            (
                Some(StepState::RolledBack),
                false,
                ResourceState::RolledBack,
            ),
            (Some(StepState::Unchanged), true, ResourceState::Drifted),
            (Some(StepState::Changed), false, ResourceState::InSync),
            (None, true, ResourceState::Drifted),
            (None, false, ResourceState::InSync),
        ];
        for (ran, drifted, want) in cases {
            let owned = [resource("common", "/a", "cleanup")];
            let latest = ran.map(|s| run(vec![module("common", vec![step("/a", s)])], vec![]));
            let mut drift = vec![];
            if drifted {
                drift.push(DriftItem {
                    module: "common".into(),
                    kind: "file.copy".into(),
                    key: "/a".into(),
                    summary: "mode 0644 → 0600".into(),
                    unified: None,
                    detected_at: None,
                });
            }
            let b = blueprint(&owned, &drift, latest.as_ref());
            assert_eq!(state_of(&b, "/a"), want, "{ran:?} drifted={drifted}");
            assert_eq!(b[0].state(), want);
        }
    }

    #[test]
    fn unfinished_removals_join_their_module_tagged_for_cleanup() {
        let removal = |key: &str, state| RemovalReport {
            resource: format!("old/file.copy[{key}]"),
            kind: "file.copy".into(),
            summary: format!("delete {key}"),
            caution: false,
            state,
            error: None,
        };
        let owned = [resource("common", "/a", "leave")];
        let latest = run(
            vec![],
            vec![
                removal("/gone", StepState::Changed),
                removal("/going", StepState::Pending),
                removal("/stuck", StepState::Failed),
            ],
        );
        let b = blueprint(&owned, &[], Some(&latest));
        let old = b.iter().find(|m| m.name == "old").expect("removals module");
        let keys: Vec<&str> = old.resources.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, ["/going", "/stuck"]);
        assert!(old.resources.iter().all(|r| r.cleanup));
        assert_eq!(state_of(&b, "/going"), ResourceState::Removing);
        assert_eq!(old.state(), ResourceState::Failed);
    }
}
