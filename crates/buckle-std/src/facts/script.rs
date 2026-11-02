//! The `script` fact: JSON emitted by executables in `facts.d`.

use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::system::{Command, FileKind};
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::time::Duration;

/// Runs custom fact scripts.
///
/// Every executable file in `<state dir>/facts.d/` (the state dir is
/// `$BUCKLE_STATE_DIR`, default `/var/lib/buckle`) is run with a ten second
/// timeout and its standard output parsed as JSON. The result is mounted at
/// `script.<file name>`, so `facts.d/rack` printing `{"id": 7}` is
/// `host.facts.script.rack.id`. A script that fails, times out or prints
/// something that is not JSON contributes `null` under its name and a warning;
/// it never stops the other scripts or the run. Non-executable files are
/// ignored. Refreshed every five minutes.
pub struct ScriptFacts;

const SCRIPT_TIMEOUT: Duration = Duration::from_secs(10);

#[async_trait]
impl FactProvider for ScriptFacts {
    const NAME: &'static str = "script";
    const TTL: Duration = Duration::from_secs(300);
    const PLATFORMS: Platforms = Platforms::UNIX;
    const DESCRIPTION: &'static str =
        "Custom facts from executables in the facts.d directory, mounted at script.<name>.";

    #[tracing::instrument(name = "facts.script", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        let dir = crate::tasks::sys_exec::state_dir(cx).join("facts.d");
        let mut out = Map::new();
        let Some(entries) = cx.system.list_dir(&dir).await? else {
            return Ok(Value::Object(out));
        };
        for path in entries {
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let executable = match cx.system.stat(&path).await {
                Ok(Some(meta)) => {
                    meta.kind == FileKind::File && meta.mode.is_some_and(|m| m & 0o111 != 0)
                }
                _ => false,
            };
            if !executable {
                continue;
            }
            out.insert(name.clone(), run_script(cx, &name, path).await);
        }
        Ok(Value::Object(out))
    }
}

async fn run_script(cx: &Cx, name: &str, path: PathBuf) -> Value {
    let command = Command::new(path.to_string_lossy().into_owned()).timeout(SCRIPT_TIMEOUT);
    let output = match cx.system.run(command).await {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(script = name, error = %e.description(), "fact script could not run; recording null");
            return Value::Null;
        }
    };
    if !output.success() {
        tracing::warn!(
            script = name,
            code = ?output.code,
            timed_out = output.timed_out,
            "fact script failed; recording null"
        );
        return Value::Null;
    }
    match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(script = name, error = %e, "fact script did not print JSON; recording null");
            Value::Null
        }
    }
}

buckle_core::register_facts!(ScriptFacts);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::system::FilePerms;
    use buckle_core::testing::FakeSystem;
    use serde_json::json;

    const DIR: &str = "/var/lib/buckle/facts.d";

    fn script(sys: FakeSystem, name: &str, mode: u32) -> FakeSystem {
        sys.with_file(
            format!("{DIR}/{name}"),
            "#!/bin/sh\n",
            FilePerms::mode(mode),
        )
    }

    #[tokio::test]
    async fn executables_are_run_and_mounted_by_file_name() {
        let sys = script(script(FakeSystem::new(), "rack", 0o755), "role", 0o700)
            .on_command_ok(&format!("{DIR}/rack"), &[], "{\"id\": 7, \"row\": \"b\"}\n")
            .on_command_ok(&format!("{DIR}/role"), &[], "[\"db\", \"primary\"]");
        let f = ScriptFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["rack"], json!({"id": 7, "row": "b"}));
        assert_eq!(f["role"], json!(["db", "primary"]));
        assert_eq!(sys.commands_run()[0].timeout, Some(SCRIPT_TIMEOUT));
    }

    #[tokio::test]
    async fn failures_yield_null_without_stopping_the_others() {
        let sys = ["broken", "garbage", "good"]
            .iter()
            .fold(FakeSystem::new(), |s, n| script(s, n, 0o755))
            .on_command_fail(&format!("{DIR}/broken"), &[], 2, "boom")
            .on_command_ok(&format!("{DIR}/garbage"), &[], "this is not json")
            .on_command_ok(&format!("{DIR}/good"), &[], "{\"ok\": true}");
        let f = ScriptFacts.collect(&cx(&sys)).await.unwrap();
        assert!(f["broken"].is_null() && f.as_object().unwrap().contains_key("broken"));
        assert!(f["garbage"].is_null());
        assert_eq!(f["good"], json!({"ok": true}));
    }

    #[tokio::test]
    async fn non_executables_and_hidden_files_are_ignored() {
        let sys = script(
            script(
                script(FakeSystem::new(), "notes.txt", 0o644),
                ".hidden",
                0o755,
            ),
            "ok",
            0o755,
        )
        .on_command_ok(&format!("{DIR}/ok"), &[], "1");
        let f = ScriptFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f, json!({"ok": 1}));
    }

    #[tokio::test]
    async fn no_directory_means_no_facts_and_the_state_dir_is_configurable() {
        assert_eq!(
            ScriptFacts.collect(&cx(&FakeSystem::new())).await.unwrap(),
            json!({})
        );
        let sys = FakeSystem::new()
            .with_env("BUCKLE_STATE_DIR", "/opt/buckle")
            .with_file(
                "/opt/buckle/facts.d/zone",
                "#!/bin/sh\n",
                FilePerms::mode(0o755),
            )
            .on_command_ok("/opt/buckle/facts.d/zone", &[], "\"eu\"");
        assert_eq!(
            ScriptFacts.collect(&cx(&sys)).await.unwrap(),
            json!({"zone": "eu"})
        );
    }
}
