//! `systemd.unit`: enable, start, stop, mask a unit.

use super::hostkit::ownership::previous_artefacts;
use super::sys_exec::run_checked;
use super::systemd_common::{UnitStatus, show, systemctl};
use async_trait::async_trait;
use buckle_core::plan::{Diff, Planned};
use buckle_core::state::ResourceRecord;
use buckle_core::task::{Applied, Artefact, Cx, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Manages the boot-time and runtime state of a systemd unit.
///
/// Each of `enabled`, `active` and `masked` is optional: a field you leave out
/// is not managed. `enabled: true` runs `systemctl enable`, `active: true`
/// `systemctl start`, and `false` the inverse. `masked: true` masks the unit
/// (it cannot be combined with `enabled: true` or `active: true`); `masked:
/// false` unmasks it. `daemon_reload: true` runs `systemctl daemon-reload`
/// first whenever the task has something to change, for units whose file was
/// written earlier in the same run.
///
/// A unit that does not exist yet (its file is written by an earlier task) is
/// treated as disabled and inactive, so the plan shows the enable and start.
///
/// Removal undoes only what Buckle did: it stops the unit if Buckle started
/// it, disables it if Buckle enabled it and unmasks it if Buckle masked it.
/// A unit that was already enabled or running when Buckle took it over is left
/// as it was.
pub struct SystemdUnit;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// Unit name, e.g. `nomad.service` or `nomad`.
    pub name: String,
    /// Start at boot (`systemctl enable`) when true, disable when false.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Running (`systemctl start`) when true, stopped when false.
    #[serde(default)]
    pub active: Option<bool>,
    /// Masked (cannot be started by anything) when true, unmasked when false.
    #[serde(default)]
    pub masked: Option<bool>,
    /// Run `systemctl daemon-reload` before changing the unit.
    #[serde(default)]
    pub daemon_reload: bool,
}

/// One thing the task will do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Unmask,
    Mask,
    Enable,
    Disable,
    Start,
    Stop,
}

impl Step {
    fn verb(self) -> &'static str {
        match self {
            Step::Unmask => "unmask",
            Step::Mask => "mask",
            Step::Enable => "enable",
            Step::Disable => "disable",
            Step::Start => "start",
            Step::Stop => "stop",
        }
    }
}

fn validate(spec: &Spec) -> Result<()> {
    if spec.masked == Some(true) && (spec.enabled == Some(true) || spec.active == Some(true)) {
        return Err(buckle_core::error::user(
            format!(
                "systemd.unit '{}' cannot be masked and enabled or active at the same time.",
                spec.name
            ),
            &["Set `masked: false`, or drop `enabled: true` / `active: true`."],
        ));
    }
    Ok(())
}

/// What needs doing to bring `status` to `spec`, or why that is impossible.
fn steps(spec: &Spec, status: &UnitStatus) -> std::result::Result<Vec<Step>, String> {
    let mut out = Vec::new();
    let masked = status.is_masked();
    match spec.masked {
        Some(true) if !masked => out.push(Step::Mask),
        Some(false) if masked => out.push(Step::Unmask),
        _ => {}
    }
    if spec.masked == Some(true) {
        // Nothing else can be done to a masked unit.
        return Ok(out);
    }
    if masked
        && spec.masked != Some(false)
        && (spec.enabled == Some(true) || spec.active == Some(true))
    {
        return Err(format!(
            "{} is masked; set `masked: false` to let Buckle unmask it",
            spec.name
        ));
    }
    match spec.enabled {
        Some(true) if !status.enable_satisfied() => out.push(Step::Enable),
        Some(false) if status.is_enabled() => out.push(Step::Disable),
        _ => {}
    }
    match spec.active {
        Some(true) if !status.is_active() => out.push(Step::Start),
        Some(false) if status.is_running() => out.push(Step::Stop),
        _ => {}
    }
    Ok(out)
}

const MASKED_PREFIX: &str = "systemd: masked ";

/// What this resource's earlier applies did to the unit.
#[derive(Debug, Default)]
struct Owned {
    enabled: bool,
    started: bool,
    masked: bool,
    unit_file: Option<std::path::PathBuf>,
}

impl Owned {
    fn from_previous(cx: &Cx, name: &str) -> Self {
        let mut owned = Owned::default();
        for artefact in previous_artefacts(cx) {
            match artefact {
                Artefact::Unit {
                    name: n,
                    enabled,
                    started,
                    unit_file,
                } if n == name => {
                    owned.enabled |= *enabled;
                    owned.started |= *started;
                    owned.unit_file = owned.unit_file.take().or_else(|| unit_file.clone());
                }
                Artefact::Other { description }
                    if description.strip_prefix(MASKED_PREFIX) == Some(name) =>
                {
                    owned.masked = true;
                }
                _ => {}
            }
        }
        owned
    }
}

#[async_trait]
impl Task for SystemdUnit {
    type Spec = Spec;
    const KIND: &'static str = "systemd.unit";
    const LOCKS: &'static [&'static str] = &["systemd"];
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str =
        "Enable, disable, start, stop, mask or unmask a systemd unit.";

    fn key(spec: &Spec) -> String {
        spec.name.clone()
    }

    fn describe(spec: &Spec) -> String {
        let mut parts = Vec::new();
        if let Some(e) = spec.enabled {
            parts.push(if e { "enabled" } else { "disabled" });
        }
        if let Some(a) = spec.active {
            parts.push(if a { "active" } else { "inactive" });
        }
        if let Some(m) = spec.masked {
            parts.push(if m { "masked" } else { "unmasked" });
        }
        format!("{} {}", spec.name, parts.join(", "))
    }

    #[tracing::instrument(name = "task.systemd.unit.plan", skip_all, fields(unit = %spec.name))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        validate(spec)?;
        let status = show(cx, &spec.name).await?;
        match steps(spec, &status) {
            Err(reason) => Ok(Planned::blocked(reason)),
            Ok(steps) if steps.is_empty() => Ok(Planned::NoOp),
            Ok(steps) => Ok(Planned::change(Diff::Opaque {
                summary: format!(
                    "{}: {} (now {})",
                    spec.name,
                    steps
                        .iter()
                        .map(|s| s.verb())
                        .collect::<Vec<_>>()
                        .join(", "),
                    status.describe()
                ),
            })),
        }
    }

    #[tracing::instrument(name = "task.systemd.unit.apply", skip_all, fields(unit = %spec.name))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        validate(spec)?;
        let name = spec.name.as_str();
        if spec.daemon_reload {
            run_checked(cx, systemctl(&["daemon-reload"])).await?;
        }
        let status = show(cx, name).await?;
        let todo = steps(spec, &status).map_err(|reason| {
            buckle_core::error::user(
                reason,
                &["Set `masked: false` on the task, or unmask the unit by hand."],
            )
        })?;
        // What this resource did on earlier applies (never what another module did).
        let mut owned = Owned::from_previous(cx, name);
        // A unit someone else enabled is theirs to run: starting it does not make
        // Buckle responsible for stopping it again on removal.
        let enabled_elsewhere = status.is_enabled() && !owned.enabled;
        for step in &todo {
            run_checked(cx, systemctl(&[step.verb(), name])).await?;
            match step {
                Step::Enable => owned.enabled = true,
                Step::Start => owned.started |= !enabled_elsewhere,
                Step::Mask => owned.masked = true,
                Step::Disable => owned.enabled = false,
                Step::Stop => owned.started = false,
                Step::Unmask => owned.masked = false,
            }
        }

        let mut applied = Applied::unchanged(format!("{name} already in the desired state"));
        if !todo.is_empty() {
            applied = Applied::changed(format!(
                "{name}: {}",
                todo.iter().map(|s| s.verb()).collect::<Vec<_>>().join(", ")
            ));
        }
        applied = applied.with_artefact(Artefact::Unit {
            name: name.to_owned(),
            enabled: owned.enabled,
            started: owned.started,
            unit_file: owned.unit_file,
        });
        if owned.masked {
            applied = applied.with_artefact(Artefact::Other {
                description: format!("{MASKED_PREFIX}{name}"),
            });
        }
        Ok(applied)
    }

    fn describe_removal(&self, record: &ResourceRecord) -> String {
        let plan = removal_plan(record);
        if plan.is_empty() {
            format!(
                "leave {} as it is (Buckle did not enable, start or mask it)",
                record.key.key
            )
        } else {
            plan.iter()
                .map(|(verb, name)| format!("systemctl {verb} {name}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    #[tracing::instrument(name = "task.systemd.unit.remove", skip_all, fields(resource = %record.key))]
    async fn remove(&self, cx: &Cx, record: &ResourceRecord) -> Result<Applied> {
        let plan = removal_plan(record);
        let name = record.key.key.clone();
        if plan.is_empty() {
            return Ok(Applied::skipped(format!(
                "Buckle did not enable, start or mask {name}; left as it is"
            )));
        }
        for (verb, unit) in &plan {
            run_checked(cx, systemctl(&[verb, unit])).await?;
        }
        Ok(Applied::changed(
            plan.iter()
                .map(|(v, u)| format!("{v} {u}"))
                .collect::<Vec<_>>()
                .join(", "),
        ))
    }
}

/// `(verb, unit)` pairs that undo what the record says Buckle did: stop, then disable, then unmask.
fn removal_plan(record: &ResourceRecord) -> Vec<(&'static str, String)> {
    let mut plan = Vec::new();
    for artefact in &record.artefacts {
        match artefact {
            Artefact::Unit {
                name,
                enabled,
                started,
                ..
            } => {
                if *started {
                    plan.push(("stop", name.clone()));
                }
                if *enabled {
                    plan.push(("disable", name.clone()));
                }
            }
            Artefact::Other { description } => {
                if let Some(name) = description.strip_prefix("systemd: masked ") {
                    plan.push(("unmask", name.to_owned()));
                }
            }
            _ => {}
        }
    }
    plan
}

buckle_core::register_task!(SystemdUnit);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, cx_after, record_of, spec};
    use crate::tasks::systemd_common::testkit::{Systemd, Unit};
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn host(world: &Systemd) -> (FakeSystem, Cx) {
        let sys = world.attach(FakeSystem::new());
        let cx = cx(&sys);
        (sys, cx)
    }

    #[rstest]
    #[case::already_enabled_and_running(json!({"name": "nomad", "enabled": true, "active": true}), Unit::new(true, true))]
    #[case::already_off(json!({"name": "nomad", "enabled": false, "active": false}), Unit::new(false, false))]
    #[case::unmanaged_fields_ignored(json!({"name": "nomad", "enabled": true}), Unit::new(true, false))]
    #[case::masked_already(json!({"name": "nomad", "masked": true}), Unit { load: "masked".into(), active: "inactive".into(), sub: "dead".into(), ufs: "masked".into() })]
    #[tokio::test]
    async fn plan_is_a_noop_when_the_host_already_matches(
        #[case] spec_json: serde_json::Value,
        #[case] unit: Unit,
    ) {
        let world = Systemd::new().unit("nomad", unit);
        let (_sys, cx) = host(&world);
        assert_eq!(
            SystemdUnit.plan(&cx, &spec(spec_json)).await.unwrap(),
            Planned::NoOp
        );
    }

    #[rstest]
    #[case::needs_enable_and_start(json!({"name": "nomad", "enabled": true, "active": true}), Unit::new(false, false), "enable, start")]
    #[case::needs_start_only(json!({"name": "nomad", "enabled": true, "active": true}), Unit::new(true, false), "start")]
    #[case::needs_disable_and_stop(json!({"name": "nomad", "enabled": false, "active": false}), Unit::new(true, true), "disable, stop")]
    #[case::needs_mask(json!({"name": "nomad", "masked": true}), Unit::new(false, false), "mask")]
    #[tokio::test]
    async fn plan_is_a_change_when_the_host_differs(
        #[case] spec_json: serde_json::Value,
        #[case] unit: Unit,
        #[case] expected: &str,
    ) {
        let world = Systemd::new().unit("nomad", unit);
        let (_sys, cx) = host(&world);
        match SystemdUnit.plan(&cx, &spec(spec_json)).await.unwrap() {
            Planned::Change { diff } => {
                assert!(
                    diff.summary()
                        .starts_with(&format!("nomad: {expected} (now ")),
                    "{}",
                    diff.summary()
                )
            }
            other => panic!("expected change, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_unit_that_does_not_exist_yet_plans_enable_and_start() {
        let (_sys, cx) = host(&Systemd::new());
        let planned = SystemdUnit
            .plan(
                &cx,
                &spec(json!({"name": "new", "enabled": true, "active": true})),
            )
            .await
            .unwrap();
        assert!(planned.is_change());
        // ... and nothing to do when it should not be running anyway.
        let planned = SystemdUnit
            .plan(
                &cx,
                &spec(json!({"name": "new", "enabled": false, "active": false})),
            )
            .await
            .unwrap();
        assert_eq!(planned, Planned::NoOp);
    }

    #[tokio::test]
    async fn a_masked_unit_blocks_enabling_unless_unmasking_is_requested() {
        let masked = Unit {
            load: "masked".into(),
            active: "inactive".into(),
            sub: "dead".into(),
            ufs: "masked".into(),
        };
        let world = Systemd::new().unit("nomad", masked);
        let (_sys, cx) = host(&world);
        let blocked = SystemdUnit
            .plan(&cx, &spec(json!({"name": "nomad", "enabled": true})))
            .await
            .unwrap();
        assert!(matches!(blocked, Planned::Blocked { .. }));
        let s: Spec =
            spec(json!({"name": "nomad", "masked": false, "enabled": true, "active": true}));
        SystemdUnit.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert_eq!(world.get("nomad").unwrap(), Unit::new(true, true));
        assert_eq!(SystemdUnit.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn conflicting_specs_are_user_errors() {
        let (_sys, cx) = host(&Systemd::new());
        let err = SystemdUnit
            .plan(
                &cx,
                &spec(json!({"name": "x", "masked": true, "enabled": true})),
            )
            .await
            .unwrap_err();
        assert!(err.is(buckle_core::error::Kind::User));
    }

    #[tokio::test]
    async fn apply_issues_the_commands_in_order_and_converges() {
        let world = Systemd::new().unit("nomad", Unit::new(false, false));
        let (sys, cx) = host(&world);
        let s: Spec =
            spec(json!({"name": "nomad", "enabled": true, "active": true, "daemon_reload": true}));
        let applied = SystemdUnit.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(applied.changed);
        let lines: Vec<String> = sys
            .command_lines()
            .into_iter()
            .filter(|l| !l.starts_with("systemctl show"))
            .collect();
        assert_eq!(
            lines,
            vec![
                "systemctl daemon-reload",
                "systemctl enable nomad",
                "systemctl start nomad"
            ]
        );
        assert_eq!(
            applied.artefacts,
            vec![Artefact::Unit {
                name: "nomad".into(),
                enabled: true,
                started: true,
                unit_file: None
            }]
        );
        assert_eq!(SystemdUnit.plan(&cx, &s).await.unwrap(), Planned::NoOp);
    }

    #[tokio::test]
    async fn removal_undoes_only_what_buckle_did() {
        // Buckle enables a unit that was already running: it must not stop it later.
        let world = Systemd::new().unit("nomad", Unit::new(false, true));
        let (sys, cx) = host(&world);
        let s: Spec = spec(json!({"name": "nomad", "enabled": true, "active": true}));
        let applied = SystemdUnit.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let record = record_of("systemd.unit", "nomad", &applied);
        assert_eq!(
            SystemdUnit.describe_removal(&record),
            "systemctl disable nomad"
        );
        let removed = SystemdUnit.remove(&cx, &record).await.unwrap();
        assert!(removed.changed);
        let unit = world.get("nomad").unwrap();
        assert_eq!(unit.ufs, "disabled");
        assert_eq!(unit.active, "active", "it was running before Buckle");
        assert!(
            !sys.command_lines()
                .iter()
                .any(|l| l == "systemctl stop nomad")
        );
    }

    #[tokio::test]
    async fn removal_stops_and_disables_what_buckle_started_and_enabled() {
        let world = Systemd::new().unit("nomad", Unit::new(false, false));
        let (_sys, cx) = host(&world);
        let s: Spec = spec(json!({"name": "nomad", "enabled": true, "active": true}));
        let applied = SystemdUnit.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        let record = record_of("systemd.unit", "nomad", &applied);
        assert_eq!(
            SystemdUnit.describe_removal(&record),
            "systemctl stop nomad, systemctl disable nomad"
        );
        SystemdUnit.remove(&cx, &record).await.unwrap();
        assert_eq!(world.get("nomad").unwrap(), Unit::new(false, false));
    }

    #[tokio::test]
    async fn removal_leaves_units_buckle_found_already_configured() {
        let world = Systemd::new().unit("nomad", Unit::new(true, true));
        let (sys, cx) = host(&world);
        let s: Spec = spec(json!({"name": "nomad", "enabled": true, "active": true}));
        // The plan is a NoOp, so the engine would not even apply; record anyway.
        let applied = SystemdUnit.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert!(!applied.changed);
        let record = record_of("systemd.unit", "nomad", &applied);
        assert!(
            SystemdUnit
                .describe_removal(&record)
                .starts_with("leave nomad")
        );
        let removed = SystemdUnit.remove(&cx, &record).await.unwrap();
        assert!(!removed.changed);
        assert_eq!(world.get("nomad").unwrap(), Unit::new(true, true));
        assert!(
            !sys.command_lines()
                .iter()
                .any(|l| l.contains("disable") || l.contains("stop"))
        );
    }

    #[tokio::test]
    async fn ownership_survives_a_later_edit_of_the_spec() {
        let world = Systemd::new().unit("nomad", Unit::new(false, false));
        let (sys, cx) = host(&world);
        let first: Spec = spec(json!({"name": "nomad", "enabled": true}));
        let applied = SystemdUnit
            .apply(&cx, &first, &Planned::NoOp)
            .await
            .unwrap();
        // Later the author also asks for it to be running; enabling is still Buckle's doing.
        let second: Spec = spec(json!({"name": "nomad", "enabled": true, "active": true}));
        let again = cx_after(&sys, record_of("systemd.unit", "nomad", &applied));
        let applied = SystemdUnit
            .apply(&again, &second, &Planned::NoOp)
            .await
            .unwrap();
        assert_eq!(
            applied.artefacts,
            vec![Artefact::Unit {
                name: "nomad".into(),
                enabled: true,
                started: true,
                unit_file: None
            }]
        );
    }

    #[tokio::test]
    async fn starting_a_unit_someone_else_enabled_does_not_claim_it() {
        // A package enabled nomad; it is stopped when Buckle asks for it to run.
        let world = Systemd::new().unit("nomad", Unit::new(true, false));
        let (_sys, cx) = host(&world);
        let applied = SystemdUnit
            .apply(
                &cx,
                &spec(json!({"name": "nomad", "enabled": true, "active": true})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        assert_eq!(world.get("nomad").unwrap(), Unit::new(true, true));
        assert_eq!(
            applied.artefacts,
            vec![Artefact::Unit {
                name: "nomad".into(),
                enabled: false,
                started: false,
                unit_file: None
            }]
        );
        let rec = record_of("systemd.unit", "nomad", &applied);
        assert!(!SystemdUnit.remove(&cx, &rec).await.unwrap().changed);
        assert_eq!(world.get("nomad").unwrap(), Unit::new(true, true));
    }

    #[tokio::test]
    async fn another_module_does_not_inherit_what_this_one_did() {
        let world = Systemd::new().unit("nomad", Unit::new(false, false));
        let (sys, cx) = host(&world);
        SystemdUnit
            .apply(
                &cx,
                &spec(json!({"name": "nomad", "enabled": true, "active": true})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        // A second resource (no previous record) managing the same unit owns nothing.
        let other = buckle_core::testing::CxBuilder::new(sys.clone())
            .module("other")
            .build();
        let applied = SystemdUnit
            .apply(
                &other,
                &spec(json!({"name": "nomad", "active": true})),
                &Planned::NoOp,
            )
            .await
            .unwrap();
        assert!(matches!(
            applied.artefacts.as_slice(),
            [Artefact::Unit {
                enabled: false,
                started: false,
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn masking_is_recorded_and_unmasked_on_removal() {
        let world = Systemd::new().unit("apport", Unit::new(false, false));
        let (_sys, cx) = host(&world);
        let s: Spec = spec(json!({"name": "apport", "masked": true}));
        let applied = SystemdUnit.apply(&cx, &s, &Planned::NoOp).await.unwrap();
        assert_eq!(world.get("apport").unwrap().ufs, "masked");
        let record = record_of("systemd.unit", "apport", &applied);
        SystemdUnit.remove(&cx, &record).await.unwrap();
        assert_eq!(world.get("apport").unwrap().ufs, "disabled");
    }
}
