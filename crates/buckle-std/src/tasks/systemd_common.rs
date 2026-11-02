//! Parsing and command helpers shared by the `systemd.*` task kinds.

use buckle_core::Result;
use buckle_core::system::Command;
use buckle_core::task::Cx;
use std::collections::HashMap;

pub(crate) fn systemctl(args: &[&str]) -> Command {
    Command::new("systemctl").args(args.iter().copied())
}

/// A unit as `systemctl show -p UnitFileState,ActiveState,LoadState,SubState` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct UnitStatus {
    pub load: String,
    pub active: String,
    pub sub: String,
    pub unit_file_state: String,
}

impl UnitStatus {
    pub(crate) fn parse(text: &str) -> Self {
        let props: HashMap<&str, &str> = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let get = |k: &str| props.get(k).copied().unwrap_or("").to_owned();
        Self {
            load: get("LoadState"),
            active: get("ActiveState"),
            sub: get("SubState"),
            unit_file_state: get("UnitFileState"),
        }
    }

    pub(crate) fn is_masked(&self) -> bool {
        self.load == "masked" || self.unit_file_state.starts_with("masked")
    }

    /// Enabled via `systemctl enable` (this is the state `disable` can undo).
    pub(crate) fn is_enabled(&self) -> bool {
        self.unit_file_state.starts_with("enabled") || self.unit_file_state.starts_with("linked")
    }

    /// Whether `enabled: true` is already satisfied. Units that cannot be
    /// enabled in the first place (`static`, `indirect`, `generated`, `alias`)
    /// are treated as satisfied so that the plan converges.
    pub(crate) fn enable_satisfied(&self) -> bool {
        self.is_enabled()
            || matches!(
                self.unit_file_state.as_str(),
                "static" | "indirect" | "generated" | "alias" | "transient"
            )
    }

    /// Running, or on its way up or down.
    pub(crate) fn is_running(&self) -> bool {
        matches!(self.active.as_str(), "active" | "activating" | "reloading")
    }

    /// Fully started.
    pub(crate) fn is_active(&self) -> bool {
        matches!(self.active.as_str(), "active" | "reloading")
    }

    pub(crate) fn describe(&self) -> String {
        format!(
            "{} ({}), unit file {}",
            self.active,
            self.sub,
            if self.unit_file_state.is_empty() {
                "none"
            } else {
                &self.unit_file_state
            }
        )
    }
}

pub(crate) async fn show(cx: &Cx, unit: &str) -> Result<UnitStatus> {
    let out = cx
        .system
        .run(systemctl(&[
            "show",
            unit,
            "-p",
            "UnitFileState,ActiveState,LoadState,SubState",
        ]))
        .await?;
    if !out.success() {
        return Err(buckle_core::error::user(
            format!(
                "`systemctl show {unit}` failed: {}",
                super::sys_exec::first_lines(&out.stderr_str(), &out.stdout_str(), 3)
            ),
            &["Check that systemd is the init system on this host."],
        ));
    }
    Ok(UnitStatus::parse(&out.stdout_str()))
}

#[cfg(test)]
pub(crate) mod testkit {
    //! A stateful pretend `systemctl` for task tests.

    use buckle_core::system::Output;
    use buckle_core::testing::{FakeSystem, ok_output};
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Unit {
        pub load: String,
        pub active: String,
        pub sub: String,
        pub ufs: String,
    }

    impl Unit {
        pub fn new(enabled: bool, active: bool) -> Self {
            Self {
                load: "loaded".into(),
                active: if active { "active" } else { "inactive" }.into(),
                sub: if active { "running" } else { "dead" }.into(),
                ufs: if enabled { "enabled" } else { "disabled" }.into(),
            }
        }
    }

    #[derive(Clone, Default)]
    pub struct Systemd(pub Arc<Mutex<BTreeMap<String, Unit>>>);

    impl Systemd {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn unit(self, name: &str, unit: Unit) -> Self {
            self.0.lock().unwrap().insert(name.into(), unit);
            self
        }

        pub fn get(&self, name: &str) -> Option<Unit> {
            self.0.lock().unwrap().get(name).cloned()
        }

        pub fn attach(&self, sys: FakeSystem) -> FakeSystem {
            let world = self.0.clone();
            sys.on_command("systemctl", &[], move |cmd| {
                let mut units = world.lock().unwrap();
                let sub = cmd.args.first().cloned().unwrap_or_default();
                let name = cmd
                    .args
                    .iter()
                    .skip(1)
                    .find(|a| !a.starts_with('-'))
                    .cloned()
                    .unwrap_or_default();
                let fail = |msg: &str| Output {
                    code: Some(1),
                    stdout: vec![],
                    stderr: msg.as_bytes().to_vec(),
                    timed_out: false,
                };
                match sub.as_str() {
                    "show" => match units.get(&name) {
                        Some(u) => ok_output(&format!(
                            "LoadState={}\nActiveState={}\nSubState={}\nUnitFileState={}\n",
                            u.load, u.active, u.sub, u.ufs
                        )),
                        None => ok_output(
                            "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n",
                        ),
                    },
                    "enable" | "disable" | "start" | "stop" | "mask" | "unmask" | "restart"
                    | "reload-or-restart" => {
                        let Some(u) = units.get_mut(&name) else {
                            return fail(&format!("Unit {name} does not exist"));
                        };
                        match sub.as_str() {
                            "enable" if u.ufs == "masked" => {
                                return fail(&format!("Unit {name} is masked"));
                            }
                            "enable" => u.ufs = "enabled".into(),
                            "disable" => u.ufs = "disabled".into(),
                            "start" | "restart" | "reload-or-restart" => {
                                u.active = "active".into();
                                u.sub = "running".into();
                            }
                            "stop" => {
                                u.active = "inactive".into();
                                u.sub = "dead".into();
                            }
                            "mask" => {
                                u.ufs = "masked".into();
                                u.load = "masked".into();
                                u.active = "inactive".into();
                            }
                            "unmask" => {
                                u.ufs = "disabled".into();
                                u.load = "loaded".into();
                            }
                            _ => {}
                        }
                        ok_output("")
                    }
                    _ => ok_output(""),
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const ENABLED_RUNNING: &str =
        "LoadState=loaded\nActiveState=active\nSubState=running\nUnitFileState=enabled\n";
    const DISABLED_DEAD: &str =
        "LoadState=loaded\nActiveState=inactive\nSubState=dead\nUnitFileState=disabled\n";
    const MASKED: &str =
        "LoadState=masked\nActiveState=inactive\nSubState=dead\nUnitFileState=masked\n";
    const MISSING: &str =
        "LoadState=not-found\nActiveState=inactive\nSubState=dead\nUnitFileState=\n";
    const STATIC_UNIT: &str =
        "LoadState=loaded\nActiveState=active\nSubState=exited\nUnitFileState=static\n";
    const CRASH_LOOP: &str =
        "LoadState=loaded\nActiveState=activating\nSubState=auto-restart\nUnitFileState=enabled\n";

    #[rstest]
    #[case(ENABLED_RUNNING, true, true, true, false)]
    #[case(DISABLED_DEAD, false, false, false, false)]
    #[case(MASKED, false, false, false, true)]
    #[case(MISSING, false, false, false, false)]
    #[case(STATIC_UNIT, false, true, true, false)]
    #[case(CRASH_LOOP, true, true, false, false)]
    fn systemctl_show_output_parses(
        #[case] text: &str,
        #[case] enabled: bool,
        #[case] running: bool,
        #[case] active: bool,
        #[case] masked: bool,
    ) {
        let s = UnitStatus::parse(text);
        assert_eq!(s.is_enabled(), enabled, "{s:?}");
        assert_eq!(s.is_running(), running, "{s:?}");
        assert_eq!(s.is_active(), active, "{s:?}");
        assert_eq!(s.is_masked(), masked, "{s:?}");
    }

    #[test]
    fn static_units_satisfy_enabled_true_but_are_not_disableable() {
        let s = UnitStatus::parse(STATIC_UNIT);
        assert!(s.enable_satisfied());
        assert!(!s.is_enabled());
    }

    #[test]
    fn missing_properties_are_tolerated() {
        let s = UnitStatus::parse("ActiveState=active\n");
        assert!(s.is_active());
        assert_eq!(s.unit_file_state, "");
    }
}
