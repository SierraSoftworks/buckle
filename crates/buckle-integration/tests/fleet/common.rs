//! Fixtures the scenarios share: a small `app` module whose revisions differ
//! by the content of one file, and hosts that pass its health check.

use buckle_core::system::FilePerms;
use buckle_core::testing::FakeSystem;
use buckle_integration::{AgentSpec, Fleet};

/// The file the `app` module manages.
pub const APP_CONF: &str = "/etc/app/app.conf";
/// The marker the `app` module's health check looks for.
pub const HEALTHY: &str = "/run/app.healthy";

/// The content of `app.conf` in revision `n`.
pub fn app_conf(n: u32) -> String {
    format!("app revision {n}\n")
}

/// `app` writes `app.conf` and has no health checks.
pub fn app_module(fleet: Fleet, n: u32) -> Fleet {
    fleet
        .module(
            "app",
            "name: app\ntasks:\n  - file.copy: { src: app.conf, dest: /etc/app/app.conf, mode: \"0644\" }\n",
        )
        .file("app", "app.conf", app_conf(n))
}

/// `app` writes `app.conf`, and is healthy while [`HEALTHY`] exists.
pub fn checked_app_module(fleet: Fleet, n: u32) -> Fleet {
    fleet
        .module(
            "app",
            "name: app\ntasks:\n  - file.copy: { src: app.conf, dest: /etc/app/app.conf, mode: \"0644\" }\nhealth:\n  - name: healthy\n    interval: 1s\n    file: { path: /run/app.healthy }\n",
        )
        .file("app", "app.conf", app_conf(n))
}

/// One implicit-style phase for everyone, auto-approved.
pub const ONE_PHASE: &str =
    "    - { name: fleet, select: 'true', parallel: 10, approval: auto, bake: 1s }\n";

/// Canary (one host, approval required) then everyone else.
pub const CANARY_THEN_REST: &str = "    - { name: canary, select: 'host.groups contains \"canary\"', parallel: 1, approval: required, bake: 1s }\n    - { name: rest, select: 'true', parallel: 2, approval: auto, bake: 1s }\n";

/// A fleet with `phases` and the unchecked `app` module at revision `n`.
pub fn app(phases: &str, n: u32) -> Fleet {
    app_module(Fleet::with_phases("", phases), n)
}

/// A fake host that passes the `app` health check.
pub fn healthy_system(id: &str) -> FakeSystem {
    FakeSystem::new()
        .with_hostname(id)
        .with_file(HEALTHY, "ok\n", FilePerms::mode(0o644))
}

/// An agent on a healthy fake host.
pub fn host(id: &str) -> AgentSpec {
    AgentSpec::new(id).system(healthy_system(id))
}
