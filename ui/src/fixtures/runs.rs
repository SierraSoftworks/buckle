//! Run reports for the demo fleet:
//!
//! * pi-4 is applying rollout #42: common, tailscale and docker are done,
//!   nomad is on its template with the unit and the restart handler still to
//!   come, otel_ebpf_profiler is deferred and a removal is pending. With
//!   `?demo&live`, [`live_frame`] moves it on step by step until it succeeds.
//! * pi-5 failed: nomad's template was rejected by its validator (a task error
//!   with causes and advice), so the unit and the handler never ran, and
//!   rolling nomad back failed too; otel_ebpf_profiler failed its health check
//!   and was rolled back.
//! * edge-1 needs attention: tailscale's unit is blocked (systemd is not PID 1
//!   in its container) and docker's selection could not be decided.
//! * pi-6 rolled itself back to the stable revision.
//! * home-nomad (a cluster) is applying #42 through pi-1: postgresql is in
//!   place, atuin's job is following its deployment and paste waits.
//! * Every other run is clean, or rolled back (the docker 28 run of rollout
//!   #40), generated from its summary so the run picker has reports to load.

use buckle_api::{
    ErrorKind, ErrorView, HealthResultDto, HostStatus, HostSummary, ModuleReport, ModuleState,
    RemovalReport, RunCounts, RunProgress, RunReport, RunState, RunSummary, StepReport, StepState,
};
use chrono::{DateTime, Duration, Utc};

use super::data::{
    ABORTED_REV, NEW_REV, OLD_REV, PLAN_NOMAD, PLAN_WORKLOADS, ROLLED_BACK_REV, STABLE_REV,
    fake_trace, trace_url,
};
use crate::logic::run::run_finished;

/// The host whose run `?demo&live` moves on.
pub const LIVE_HOST: &str = "pi-4";

/// A run's id: the host and the revision it applied (each revision runs once
/// per host in the demo).
pub fn run_id(host: &str, revision: &str) -> String {
    format!("{host}-{}", &revision[..7])
}

fn rollout_of(revision: &str) -> Option<String> {
    match revision {
        NEW_REV => Some("42"),
        STABLE_REV => Some("41"),
        ROLLED_BACK_REV => Some("40"),
        ABORTED_REV => Some("39"),
        _ => None,
    }
    .map(Into::into)
}

fn err(kind: ErrorKind, message: &str, causes: &[&str], advice: &[&str]) -> ErrorView {
    ErrorView {
        kind,
        message: message.into(),
        causes: causes.iter().map(|c| c.to_string()).collect(),
        advice: advice.iter().map(|a| a.to_string()).collect(),
    }
}

fn step(
    kind: &str,
    key: &str,
    summary: &str,
    state: StepState,
    result: Option<&str>,
    ms: u64,
) -> StepReport {
    StepReport {
        id: format!("{kind}[{key}]"),
        kind: kind.into(),
        name: None,
        summary: summary.into(),
        state,
        result: result.map(Into::into),
        diff: None,
        destructive: false,
        handler: false,
        started_at: None,
        finished_at: None,
        duration_ms: (ms > 0).then_some(ms),
        error: None,
    }
}

fn handler(
    kind: &str,
    key: &str,
    summary: &str,
    state: StepState,
    result: Option<&str>,
    ms: u64,
) -> StepReport {
    StepReport {
        handler: true,
        ..step(kind, key, summary, state, result, ms)
    }
}

fn failing(mut step: StepReport, error: ErrorView) -> StepReport {
    step.error = Some(error);
    step
}

fn module(
    name: &str,
    state: ModuleState,
    steps: Vec<StepReport>,
    handlers: Vec<StepReport>,
) -> ModuleReport {
    ModuleReport {
        name: name.into(),
        state,
        note: None,
        started_at: None,
        finished_at: None,
        duration_ms: None,
        changes: steps
            .iter()
            .filter(|s| s.state == StepState::Changed)
            .count() as u32,
        error: None,
        steps,
        handlers,
        health: vec![],
    }
}

fn noted(mut m: ModuleReport, note: &str) -> ModuleReport {
    m.note = Some(note.into());
    m
}

fn broken(mut m: ModuleReport, error: ErrorView) -> ModuleReport {
    m.error = Some(error);
    m
}

fn check(
    module: &str,
    check: &str,
    kind: &str,
    status: &str,
    reason: Option<&str>,
    at: DateTime<Utc>,
    rollout: Option<&str>,
) -> HealthResultDto {
    HealthResultDto {
        scope: "bake".into(),
        module: module.into(),
        check: check.into(),
        kind: kind.into(),
        required: true,
        status: status.into(),
        reason: reason.map(Into::into),
        at,
        rollout_id: rollout.map(Into::into),
    }
}

/// Gives the steps of each module times one after another from `start`, the
/// way the executor would have run them, and the modules their spans. A step
/// that has not finished keeps no duration; a running one started where the
/// previous one ended. Returns when the last finished step ended.
fn lay_out(modules: &mut [ModuleReport], start: DateTime<Utc>) -> DateTime<Utc> {
    let mut t = start;
    for m in modules.iter_mut() {
        let begun = t;
        let mut ran = false;
        for s in m.steps.iter_mut().chain(m.handlers.iter_mut()) {
            match s.state {
                StepState::Pending | StepState::NotRun | StepState::Skipped => {
                    s.duration_ms = None;
                }
                StepState::Running => {
                    s.started_at = Some(t);
                    s.duration_ms = None;
                    ran = true;
                }
                _ => {
                    let ms = s.duration_ms.unwrap_or(20);
                    s.started_at = Some(t);
                    t += Duration::milliseconds(ms as i64);
                    s.finished_at = Some(t);
                    ran = true;
                }
            }
        }
        if ran {
            m.started_at = Some(begun);
            if !matches!(m.state, ModuleState::Running | ModuleState::Pending) {
                m.finished_at = Some(t);
                m.duration_ms = Some((t - begun).num_milliseconds() as u64);
            }
        }
    }
    t
}

fn report(
    host: &str,
    revision: &str,
    state: RunState,
    start: DateTime<Utc>,
    mut modules: Vec<ModuleReport>,
    removals: Vec<RemovalReport>,
    now: DateTime<Utc>,
) -> RunReport {
    let end = lay_out(&mut modules, start);
    let finished = run_finished(state);
    let seed = host.bytes().map(u64::from).sum::<u64>() * 31 + revision.as_bytes()[0] as u64;
    let trace = fake_trace(seed);
    let mut r = RunReport {
        id: run_id(host, revision),
        host: host.into(),
        revision: revision.into(),
        plan_digest: if revision == NEW_REV && host.ends_with("-nomad") {
            PLAN_WORKLOADS.into()
        } else if revision == NEW_REV {
            PLAN_NOMAD.into()
        } else {
            format!("{}{}", &revision[8..16], &PLAN_NOMAD[8..])
        },
        rollout_id: rollout_of(revision),
        state,
        started_at: Some(start),
        finished_at: finished.then_some(end),
        updated_at: if finished { end } else { now },
        counts: RunCounts::default(),
        modules,
        removals,
        error: None,
        trace_url: Some(trace_url(&trace)),
        trace_id: Some(trace),
    };
    r.recount();
    r
}

fn common(changed: bool) -> ModuleReport {
    let (line, swap) = if changed {
        (StepState::Changed, StepState::Changed)
    } else {
        (StepState::Unchanged, StepState::Unchanged)
    };
    module(
        "common",
        if changed {
            ModuleState::Applied
        } else {
            ModuleState::Unchanged
        },
        vec![
            step(
                "apt.package",
                "ufw, jq, curl",
                "ufw, jq and curl are installed",
                StepState::Unchanged,
                None,
                420,
            ),
            step(
                "file.line",
                "/etc/ssh/sshd_config:PasswordAuthentication",
                "append line PasswordAuthentication no",
                line,
                changed.then_some("appended 1 line"),
                35,
            ),
            step(
                "sysctl.param",
                "vm.swappiness",
                "1 → 1 (drift: current 60)",
                swap,
                changed.then_some("vm.swappiness = 1"),
                12,
            ),
        ],
        if changed {
            vec![handler(
                "systemd.reload",
                "ssh",
                "reload ssh.service",
                StepState::Changed,
                Some("reloaded ssh.service"),
                180,
            )]
        } else {
            vec![]
        },
    )
}

fn tailscale() -> ModuleReport {
    module(
        "tailscale",
        ModuleState::Unchanged,
        vec![
            step(
                "apt.package",
                "tailscale",
                "tailscale 1.76.1 is installed",
                StepState::Unchanged,
                None,
                380,
            ),
            step(
                "systemd.unit",
                "tailscaled",
                "tailscaled.service enabled and running",
                StepState::Unchanged,
                None,
                60,
            ),
        ],
        vec![],
    )
}

fn docker() -> ModuleReport {
    module(
        "docker",
        ModuleState::Unchanged,
        vec![
            step(
                "apt.package",
                "docker-ce",
                "docker-ce 27.3.1 is installed",
                StepState::Unchanged,
                None,
                510,
            ),
            step(
                "file.copy",
                "/etc/docker/daemon.json",
                "daemon.json matches",
                StepState::Unchanged,
                None,
                8,
            ),
            step(
                "systemd.unit",
                "docker",
                "docker.service enabled and running",
                StepState::Unchanged,
                None,
                40,
            ),
        ],
        vec![],
    )
}

const APT_NOMAD: (&str, &str, &str) = ("apt.package", "nomad", "1.9.2 → 1.9.3");
const TEMPLATE: (&str, &str, &str) = (
    "file.template",
    "/etc/nomad.d/nomad.hcl",
    "3 lines changed (rotates «secret:nomad_server@b21c»)",
);
const UNIT: (&str, &str, &str) = (
    "systemd.unit",
    "nomad",
    "keep nomad.service enabled and running",
);
const RESTART: (&str, &str, &str) = ("systemd.restart", "nomad", "restart nomad.service");
const OTEL_APT: (&str, &str, &str) = (
    "apt.package",
    "otel-ebpf-profiler",
    "install otel-ebpf-profiler 0.4.1",
);
const OTEL_UNIT: (&str, &str, &str) = (
    "systemd.unit",
    "otel-ebpf-profiler",
    "enable and start otel-ebpf-profiler.service",
);
const REMOVAL: &str = "nomad/file.copy[/etc/nomad.d/client.hcl]";

fn id((kind, key, _): (&str, &str, &str)) -> String {
    format!("{kind}[{key}]")
}

fn otel_steps(state: StepState) -> Vec<StepReport> {
    vec![
        step(OTEL_APT.0, OTEL_APT.1, OTEL_APT.2, state, None, 0),
        step(OTEL_UNIT.0, OTEL_UNIT.1, OTEL_UNIT.2, state, None, 0),
    ]
}

/// pi-4, mid-way through rollout #42.
fn live(start: DateTime<Utc>, now: DateTime<Utc>) -> RunReport {
    let nomad = module(
        "nomad",
        ModuleState::Running,
        vec![
            step(
                APT_NOMAD.0,
                APT_NOMAD.1,
                APT_NOMAD.2,
                StepState::Changed,
                Some("upgraded nomad 1.9.2 → 1.9.3"),
                38_200,
            ),
            step(
                TEMPLATE.0,
                TEMPLATE.1,
                TEMPLATE.2,
                StepState::Running,
                None,
                0,
            ),
            step(UNIT.0, UNIT.1, UNIT.2, StepState::Pending, None, 0),
        ],
        vec![handler(
            RESTART.0,
            RESTART.1,
            RESTART.2,
            StepState::Pending,
            None,
            0,
        )],
    );
    let otel = noted(
        module("otel_ebpf_profiler", ModuleState::Deferred, vec![], vec![]),
        "Waits for facts from docker (docker.version).",
    );
    report(
        LIVE_HOST,
        NEW_REV,
        RunState::Running,
        start,
        vec![common(true), tailscale(), docker(), nomad, otel],
        vec![RemovalReport {
            resource: REMOVAL.into(),
            kind: "file.copy".into(),
            summary: "delete /etc/nomad.d/client.hcl (merged into nomad.hcl)".into(),
            state: StepState::Pending,
            caution: false,
            error: None,
        }],
        now,
    )
}

/// The next step of pi-4's run under `?demo&live`: the updates the agent
/// would send for it. The report given is the demo server's copy, which the
/// caller applies the updates to; a frame that plans a deferred module adds
/// its steps there first, so a page that holds the old report has to fetch
/// it again to see them.
pub fn live_frame(frame: usize, report: &mut RunReport) -> Vec<RunProgress> {
    let at = Utc::now();
    let p = |module: Option<&str>, step: Option<String>| RunProgress {
        host: report.host.clone(),
        run_id: report.id.clone(),
        rollout_id: report.rollout_id.clone(),
        at,
        run_state: RunState::Running,
        module: module.map(Into::into),
        module_state: None,
        step,
        step_state: None,
        result: None,
        error: None,
    };
    let to = |mut u: RunProgress, state: StepState, result: Option<&str>| {
        u.step_state = Some(state);
        u.result = result.map(Into::into);
        u
    };
    let module_to = |mut u: RunProgress, state: ModuleState| {
        u.module_state = Some(state);
        u
    };
    match frame {
        0 => vec![
            to(
                p(Some("nomad"), Some(id(TEMPLATE))),
                StepState::Changed,
                Some("rendered; nomad config validate passed"),
            ),
            to(p(Some("nomad"), Some(id(UNIT))), StepState::Running, None),
        ],
        1 => vec![
            to(
                p(Some("nomad"), Some(id(UNIT))),
                StepState::Unchanged,
                Some("enabled and running"),
            ),
            to(
                p(Some("nomad"), Some(id(RESTART))),
                StepState::Running,
                None,
            ),
        ],
        2 => vec![
            to(
                p(Some("nomad"), Some(id(RESTART))),
                StepState::Changed,
                Some("restarted nomad.service; active after 2.1s"),
            ),
            module_to(p(Some("nomad"), None), ModuleState::Applied),
        ],
        3 => {
            // docker's facts are in: the deferred module is planned now.
            if let Some(m) = report
                .modules
                .iter_mut()
                .find(|m| m.name == "otel_ebpf_profiler")
            {
                m.steps = otel_steps(StepState::Pending);
                m.note = None;
            }
            vec![module_to(
                to(
                    p(Some("otel_ebpf_profiler"), Some(id(OTEL_APT))),
                    StepState::Running,
                    None,
                ),
                ModuleState::Running,
            )]
        }
        4 => vec![
            to(
                p(Some("otel_ebpf_profiler"), Some(id(OTEL_APT))),
                StepState::Changed,
                Some("installed otel-ebpf-profiler 0.4.1"),
            ),
            to(
                p(Some("otel_ebpf_profiler"), Some(id(OTEL_UNIT))),
                StepState::Running,
                None,
            ),
        ],
        5 => vec![
            to(
                p(Some("otel_ebpf_profiler"), Some(id(OTEL_UNIT))),
                StepState::Changed,
                Some("enabled and started otel-ebpf-profiler.service"),
            ),
            module_to(p(Some("otel_ebpf_profiler"), None), ModuleState::Applied),
        ],
        6 => vec![to(p(None, Some(REMOVAL.into())), StepState::Running, None)],
        7 => {
            let mut done = to(
                p(None, Some(REMOVAL.into())),
                StepState::Changed,
                Some("deleted /etc/nomad.d/client.hcl"),
            );
            done.run_state = RunState::Succeeded;
            vec![done]
        }
        _ => vec![],
    }
}

/// A clean run of the new revision.
fn clean(host: &str, start: DateTime<Utc>, now: DateTime<Utc>) -> RunReport {
    let mut nomad = module(
        "nomad",
        ModuleState::Applied,
        vec![
            step(
                APT_NOMAD.0,
                APT_NOMAD.1,
                APT_NOMAD.2,
                StepState::Changed,
                Some("upgraded nomad 1.9.2 → 1.9.3"),
                36_900,
            ),
            step(
                TEMPLATE.0,
                TEMPLATE.1,
                TEMPLATE.2,
                StepState::Changed,
                Some("rendered; nomad config validate passed"),
                1_150,
            ),
            step(
                UNIT.0,
                UNIT.1,
                UNIT.2,
                StepState::Unchanged,
                Some("enabled and running"),
                55,
            ),
        ],
        vec![handler(
            RESTART.0,
            RESTART.1,
            RESTART.2,
            StepState::Changed,
            Some("restarted nomad.service; active after 2.3s"),
            2_300,
        )],
    );
    // A bake reading every 30s, the api blipping once while nomad settled,
    // then the drift checks since.
    let at = start + Duration::seconds(160);
    nomad.health = (0..10)
        .flat_map(|i| {
            let at = at + Duration::seconds(30 * i);
            let blip = i == 3;
            [
                check(
                    "nomad",
                    "api",
                    "http",
                    if blip { "fail" } else { "pass" },
                    blip.then_some("GET http://127.0.0.1:4646/v1/agent/health answered 500"),
                    at,
                    Some("42"),
                ),
                check(
                    "nomad",
                    "unit",
                    "systemd",
                    "pass",
                    Some("active for 2m (min_uptime 30s)"),
                    at,
                    Some("42"),
                ),
            ]
        })
        .collect();
    for at in (1..=3)
        .map(|i| at + Duration::minutes(10 * i))
        .filter(|t| *t <= now)
    {
        for (c, kind) in [("api", "http"), ("unit", "systemd")] {
            nomad.health.push(HealthResultDto {
                scope: "drift".into(),
                ..check("nomad", c, kind, "pass", None, at, Some("42"))
            });
        }
    }
    let mut otel = module(
        "otel_ebpf_profiler",
        ModuleState::Applied,
        vec![
            step(
                OTEL_APT.0,
                OTEL_APT.1,
                OTEL_APT.2,
                StepState::Changed,
                Some("installed otel-ebpf-profiler 0.4.1"),
                9_400,
            ),
            step(
                OTEL_UNIT.0,
                OTEL_UNIT.1,
                OTEL_UNIT.2,
                StepState::Changed,
                Some("enabled and started otel-ebpf-profiler.service"),
                640,
            ),
        ],
        vec![],
    );
    otel.note = Some("Deferred until docker's facts arrived, then planned.".into());
    report(
        host,
        NEW_REV,
        RunState::Succeeded,
        start,
        vec![common(true), tailscale(), docker(), nomad, otel],
        vec![],
        now,
    )
}

/// pi-5: a task error in nomad, the rest of nomad never ran, its rollback
/// failed, and otel_ebpf_profiler was rolled back after failing its check.
fn failed(start: DateTime<Utc>, now: DateTime<Utc>) -> RunReport {
    let template = failing(
        step(
            TEMPLATE.0,
            TEMPLATE.1,
            TEMPLATE.2,
            StepState::Failed,
            Some("not written: the validator refused it, the old nomad.hcl is still in place"),
            1_240,
        ),
        err(
            ErrorKind::User,
            "The rendered nomad.hcl was rejected by its validator",
            &[
                "nomad config validate /etc/nomad.d/.nomad.hcl.buckle exited with status 1",
                "client.servers: \"«secret:nomad_server@b21c»\" is not a valid address: missing port in address",
            ],
            &[
                "The rotated nomad_server secret has no port: store it as host:4647.",
                "Run buckle plan --host pi-5 after fixing it to see the template render, then retry pi-5.",
            ],
        ),
    );
    let mut nomad = broken(
        module(
            "nomad",
            ModuleState::Failed,
            vec![
                step(
                    APT_NOMAD.0,
                    APT_NOMAD.1,
                    APT_NOMAD.2,
                    StepState::Changed,
                    Some("upgraded nomad 1.9.2 → 1.9.3; the rollback could not undo it"),
                    41_300,
                ),
                template,
                step(UNIT.0, UNIT.1, UNIT.2, StepState::NotRun, None, 0),
            ],
            vec![handler(
                RESTART.0,
                RESTART.1,
                RESTART.2,
                StepState::NotRun,
                None,
                0,
            )],
        ),
        err(
            ErrorKind::User,
            "Rolling nomad back to 5d4a9b2 failed: apt.package[nomad] is blocked",
            &[
                "nomad 1.9.2 is no longer in the repository (apt.releases.hashicorp.com bookworm main)",
                "apt-cache policy nomad lists 1.9.3 and 1.9.4 only",
            ],
            &[
                "Pin nomad to a version the repository still serves in modules/nomad/module.yaml, or keep 1.9.2 in a local mirror.",
                "Until then pi-5 runs nomad 1.9.3 with the stable revision's configuration: a mix of revisions.",
            ],
        ),
    );
    nomad.health = vec![check(
        "nomad",
        "api",
        "http",
        "fail",
        Some("answered 500 for longer than settle (2m)"),
        start + Duration::seconds(170),
        Some("42"),
    )];
    let mut otel = broken(
        noted(
            module(
                "otel_ebpf_profiler",
                ModuleState::RolledBack,
                vec![
                    step(
                        OTEL_APT.0,
                        OTEL_APT.1,
                        OTEL_APT.2,
                        StepState::RolledBack,
                        Some("installed 0.4.1, then removed by the rollback"),
                        9_800,
                    ),
                    step(
                        OTEL_UNIT.0,
                        OTEL_UNIT.1,
                        OTEL_UNIT.2,
                        StepState::RolledBack,
                        Some("started, then stopped and disabled by the rollback"),
                        700,
                    ),
                ],
                vec![],
            ),
            "New in this revision, so rolling it back removed what it installed.",
        ),
        err(
            ErrorKind::System,
            "Required check otel_ebpf_profiler/agent failed for longer than settle (2m)",
            &[
                "otel-ebpf-profiler.service is in state failed (exit status 1)",
                "failed to load BPF program: operation not permitted",
                "the running kernel (6.1.21-v8+) was built without BTF type information",
            ],
            &[
                "Upgrade pi-5's kernel to the one the other Pis run, or select the module only where host.facts.kernel.btf is true.",
            ],
        ),
    );
    otel.health = vec![check(
        "otel_ebpf_profiler",
        "agent",
        "systemd",
        "fail",
        Some("unit failed 3 times in 2m"),
        start + Duration::seconds(175),
        Some("42"),
    )];
    report(
        "pi-5",
        NEW_REV,
        RunState::Failed,
        start,
        vec![common(true), tailscale(), docker(), nomad, otel],
        vec![],
        now,
    )
}

/// pi-6: the agent rolled itself back after losing its session.
fn local_rollback(start: DateTime<Utc>, now: DateTime<Utc>) -> RunReport {
    let nomad = broken(
        module(
            "nomad",
            ModuleState::RolledBack,
            vec![
                step(
                    APT_NOMAD.0,
                    APT_NOMAD.1,
                    APT_NOMAD.2,
                    StepState::RolledBack,
                    Some("upgraded to 1.9.3, then returned to 1.9.2"),
                    37_400,
                ),
                step(
                    TEMPLATE.0,
                    TEMPLATE.1,
                    TEMPLATE.2,
                    StepState::RolledBack,
                    Some("restored the stable revision's content"),
                    1_100,
                ),
                step(
                    UNIT.0,
                    UNIT.1,
                    UNIT.2,
                    StepState::Unchanged,
                    Some("enabled and running"),
                    50,
                ),
            ],
            vec![handler(
                RESTART.0,
                RESTART.1,
                RESTART.2,
                StepState::RolledBack,
                Some("restarted on the stable configuration"),
                2_200,
            )],
        ),
        local_rollback_error(),
    );
    report(
        "pi-6",
        NEW_REV,
        RunState::RolledBack,
        start,
        vec![common(true), tailscale(), docker(), nomad],
        vec![],
        now,
    )
}

/// Why pi-6 rolled itself back, as its rollout entry and its run say it.
pub fn local_rollback_error() -> ErrorView {
    err(
        ErrorKind::System,
        "The agent applied the cached stable revision after 5m without a session while nomad/unit was failing",
        &[
            "nomad.service exited 3 times in 2 minutes (status 1)",
            "the server was unreachable for 5m12s, so the agent rolled back on its own (local rollback)",
        ],
        &[
            "Read journalctl -u nomad on pi-6 for why nomad kept exiting.",
            "Retry pi-6 once the cause is fixed; the stable revision is in place meanwhile.",
        ],
    )
}

/// What pi-5's rollout entry says, structured.
pub fn pi5_error() -> ErrorView {
    err(
        ErrorKind::User,
        "nomad failed on pi-5, and rolling it back failed too",
        &[
            "file.template[/etc/nomad.d/nomad.hcl]: nomad config validate rejected the rendered file (client.servers is missing its port)",
            "rolling back apt.package[nomad]: 1.9.2 is no longer in the repository",
        ],
        &[
            "Fix the nomad_server secret (store it as host:4647), then retry pi-5.",
            "Until then pi-5 runs nomad 1.9.3 with the stable revision's configuration: a mix of revisions.",
        ],
    )
}

/// edge-1's first run: blocked steps and an undecidable module.
fn needs_attention(start: DateTime<Utc>, now: DateTime<Utc>) -> RunReport {
    let unit = failing(
        step(
            "systemd.unit",
            "tailscaled",
            "enable and start tailscaled.service",
            StepState::Blocked,
            Some("planned again after apt.package[tailscale] changed; still blocked"),
            30,
        ),
        err(
            ErrorKind::User,
            "systemd.unit[tailscaled] is blocked: systemd is not PID 1 on edge-1",
            &[
                "edge-1 runs in a container whose PID 1 is /sbin/docker-init",
                "systemctl: System has not been booted with systemd as init system (PID 1)",
            ],
            &[
                "Run the agent where systemd is PID 1, or start tailscaled another way and leave the unit out for site=edge hosts.",
                "Nothing failed: the rest of the revision is applied, and the step is planned again on the next run.",
            ],
        ),
    );
    let tailscale = module(
        "tailscale",
        ModuleState::Blocked,
        vec![
            step(
                "apt.package",
                "tailscale",
                "install tailscale 1.76.1",
                StepState::Changed,
                Some("installed tailscale 1.76.1"),
                14_200,
            ),
            unit,
            step(
                "command.run",
                "tailscale up",
                "tailscale up --auth-key «secret:ts_authkey@77c1»",
                StepState::Blocked,
                Some("waits for tailscaled, which is blocked"),
                10,
            ),
        ],
        vec![],
    );
    let docker = noted(
        module("docker", ModuleState::Indeterminate, vec![], vec![]),
        "Its selector reads host.facts.virt.kind, which the agent could not determine in this container.",
    );
    let common = module(
        "common",
        ModuleState::Applied,
        vec![
            step(
                "apt.package",
                "ufw, jq, curl",
                "install ufw, jq, curl",
                StepState::Changed,
                Some("installed 3 packages"),
                22_500,
            ),
            step(
                "file.line",
                "/etc/ssh/sshd_config:PasswordAuthentication",
                "append line PasswordAuthentication no",
                StepState::Changed,
                Some("appended 1 line"),
                30,
            ),
            step(
                "sysctl.param",
                "vm.swappiness",
                "60 → 1",
                StepState::Changed,
                Some("vm.swappiness = 1"),
                10,
            ),
        ],
        vec![],
    );
    let mut r = report(
        "edge-1",
        STABLE_REV,
        RunState::NeedsAttention,
        start,
        vec![common, tailscale, docker],
        vec![],
        now,
    );
    // Its first run after enrolling, not part of a rollout.
    r.rollout_id = None;
    r
}

/// An older run, from its summary: clean, or the docker 28 rollback.
fn generic(host: &str, s: &RunSummary, now: DateTime<Utc>) -> RunReport {
    if s.outcome == "rolled_back" {
        let mut docker = broken(
            module(
                "docker",
                ModuleState::RolledBack,
                vec![
                    step(
                        "apt.package",
                        "docker-ce",
                        "27.3.1 → 28.0.1",
                        StepState::RolledBack,
                        Some("upgraded to 28.0.1, then returned to 27.3.1"),
                        52_000,
                    ),
                    step(
                        "file.copy",
                        "/etc/docker/daemon.json",
                        "set the cgroup driver to systemd",
                        StepState::RolledBack,
                        Some("restored the stable revision's content"),
                        9,
                    ),
                ],
                vec![handler(
                    "systemd.restart",
                    "docker",
                    "restart docker.service",
                    StepState::RolledBack,
                    Some("restarted on 27.3.1"),
                    3_100,
                )],
            ),
            err(
                ErrorKind::System,
                "Required check docker/daemon failed for longer than settle (2m)",
                &[
                    "docker info exited with status 1",
                    "Cannot connect to the Docker daemon at unix:///var/run/docker.sock",
                    "dockerd: failed to start daemon: iptables-legacy is not supported by docker 28",
                ],
                &["Move these hosts to the nftables backend before rolling docker 28 out again."],
            ),
        );
        docker.health = vec![check(
            "docker",
            "daemon",
            "command",
            "fail",
            Some("docker info exited 1 for 2m"),
            s.started_at + Duration::seconds(90),
            Some("40"),
        )];
        let nomad = noted(
            module(
                "nomad",
                ModuleState::NotStarted,
                vec![
                    step(
                        APT_NOMAD.0,
                        APT_NOMAD.1,
                        "1.9.1 → 1.9.2",
                        StepState::NotRun,
                        None,
                        0,
                    ),
                    step(UNIT.0, UNIT.1, UNIT.2, StepState::NotRun, None, 0),
                ],
                vec![],
            ),
            "Not started: the run halted after docker failed.",
        );
        return report(
            host,
            &s.revision,
            RunState::RolledBack,
            s.started_at,
            vec![common(false), tailscale(), docker, nomad],
            vec![],
            now,
        );
    }
    let removals = (0..s.removals)
        .map(|i| RemovalReport {
            resource: format!("common/file.copy[/etc/buckle/legacy-{i}.conf]"),
            kind: "file.copy".into(),
            summary: format!("delete /etc/buckle/legacy-{i}.conf (module no longer declares it)"),
            state: StepState::Changed,
            caution: false,
            error: None,
        })
        .collect();
    report(
        host,
        &s.revision,
        RunState::Succeeded,
        s.started_at,
        vec![common(s.changes > 0), tailscale(), docker()],
        removals,
        now,
    )
}

/// A cluster's run: its workloads in plan order. On the new revision atuin
/// is following its deployment (the job step's result is its progress);
/// earlier ones applied cleanly.
fn workloads_run(host: &str, s: &RunSummary, now: DateTime<Utc>) -> RunReport {
    let live = s.revision == NEW_REV;
    let job = |name: &str, state: StepState, result: Option<&str>, ms: u64| {
        step(
            "nomad.job",
            &format!("default/{name}"),
            &format!("update default/{name}"),
            state,
            result,
            ms,
        )
    };
    let done = |name: &str, extra: Vec<StepReport>| {
        let mut steps = extra;
        steps.push(job(name, StepState::Unchanged, None, 140));
        module(name, ModuleState::Unchanged, steps, vec![])
    };
    let postgresql = done(
        "postgresql",
        vec![step(
            "nomad.volume",
            "default/postgresql-data",
            "volume default/postgresql-data",
            StepState::Unchanged,
            None,
            90,
        )],
    );
    let (atuin, paste) = if live {
        (
            module(
                "atuin",
                ModuleState::Running,
                vec![
                    step(
                        "nomad.variable",
                        "nomad/jobs/atuin",
                        "update nomad/jobs/atuin",
                        StepState::Changed,
                        Some("updated 1 item"),
                        210,
                    ),
                    job(
                        "atuin",
                        StepState::Running,
                        Some("deploying: server 1/2 healthy"),
                        0,
                    ),
                ],
                vec![],
            ),
            module(
                "paste",
                ModuleState::Pending,
                vec![job("paste", StepState::Pending, None, 0)],
                vec![],
            ),
        )
    } else {
        (done("atuin", vec![]), done("paste", vec![]))
    };
    report(
        host,
        &s.revision,
        if live {
            RunState::Running
        } else {
            RunState::Succeeded
        },
        s.started_at,
        vec![postgresql, atuin, paste],
        vec![],
        now,
    )
}

/// The report of a run, from its summary.
pub fn report_for(host: &str, s: &RunSummary, now: DateTime<Utc>) -> RunReport {
    match (host, s.revision.as_str()) {
        ("home-nomad", _) => workloads_run(host, s, now),
        (LIVE_HOST, NEW_REV) => live(s.started_at, now),
        ("pi-5", NEW_REV) => failed(s.started_at, now),
        ("pi-6", NEW_REV) => local_rollback(s.started_at, now),
        ("edge-1", _) => needs_attention(s.started_at, now),
        (_, NEW_REV) => clean(host, s.started_at, now),
        _ => generic(host, s, now),
    }
}

/// The summary a report is listed under.
pub fn summary_of(r: &RunReport, outcome: &str, error: Option<&str>) -> RunSummary {
    let started = r.started_at.unwrap_or(r.updated_at);
    RunSummary {
        id: r.id.clone(),
        state: Some(r.state),
        revision: r.revision.clone(),
        plan_digest: r.plan_digest.clone(),
        started_at: started,
        // A run in progress has no end; the contract wants a time.
        finished_at: r.finished_at.unwrap_or(started),
        outcome: outcome.into(),
        changes: r.counts.steps.changed,
        removals: r.removals.len() as u32,
        error: error.map(Into::into),
        trace_id: r.trace_id.clone(),
        trace_url: r.trace_url.clone(),
    }
}

/// A host's runs, newest first.
pub fn summaries(host: &HostSummary, now: DateTime<Utc>) -> Vec<RunSummary> {
    if host.status == HostStatus::Pending {
        return vec![];
    }
    let id = host.id.as_str();
    let ago = |mins: i64| now - Duration::minutes(mins);
    let entry = |revision: &str, start: DateTime<Utc>, outcome: &str, error: Option<&str>| {
        let skeleton = RunSummary {
            id: run_id(id, revision),
            state: None,
            revision: revision.into(),
            plan_digest: String::new(),
            started_at: start,
            finished_at: start,
            outcome: outcome.into(),
            changes: 3,
            removals: if revision == OLD_REV { 2 } else { 0 },
            error: None,
            trace_id: None,
            trace_url: None,
        };
        summary_of(&report_for(id, &skeleton, now), outcome, error)
    };
    if host.kind == buckle_api::TargetKind::Cluster {
        return if id == "home-nomad" {
            vec![
                entry(NEW_REV, ago(3), "running", None),
                entry(STABLE_REV, ago(48 * 60), "success", None),
            ]
        } else {
            vec![]
        };
    }
    if id == "edge-1" {
        return vec![entry(
            STABLE_REV,
            ago(18),
            "needs_attention",
            Some("tailscale: 2 steps blocked"),
        )];
    }
    let mut runs = vec![
        entry(STABLE_REV, ago(48 * 60), "success", None),
        entry(
            ROLLED_BACK_REV,
            ago(120 * 60),
            "rolled_back",
            Some("docker/daemon failed; rolled back to 0aa1b2c"),
        ),
        entry(OLD_REV, ago(288 * 60), "success", None),
    ];
    let newest = match id {
        LIVE_HOST => Some(entry(NEW_REV, now - Duration::seconds(44), "running", None)),
        "pi-5" => Some(entry(
            NEW_REV,
            ago(13),
            "apply_failed",
            Some("nomad failed at file.template[/etc/nomad.d/nomad.hcl]; rolling it back failed"),
        )),
        "pi-6" => Some(entry(
            NEW_REV,
            ago(15),
            "rolled_back",
            Some("nomad/unit failing; rolled back locally to 5d4a9b2"),
        )),
        "pi-1" => Some(entry(NEW_REV, ago(28), "success", None)),
        "pi-2" => Some(entry(NEW_REV, ago(12), "success", None)),
        "pi-3" => Some(entry(NEW_REV, ago(6), "success", None)),
        _ => None,
    };
    if let Some(run) = newest {
        runs.insert(0, run);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::run::{Applied, apply_progress, module_has_problem};

    fn summary(host: &str, revision: &str, outcome: &str) -> RunSummary {
        RunSummary {
            id: run_id(host, revision),
            state: None,
            revision: revision.into(),
            plan_digest: String::new(),
            started_at: Utc::now() - Duration::minutes(10),
            finished_at: Utc::now(),
            outcome: outcome.into(),
            changes: 1,
            removals: 0,
            error: None,
            trace_id: None,
            trace_url: None,
        }
    }

    #[test]
    fn the_live_run_is_mid_way_and_its_frames_take_it_to_success() {
        let now = Utc::now();
        let mut r = report_for(LIVE_HOST, &summary(LIVE_HOST, NEW_REV, "running"), now);
        assert_eq!(r.state, RunState::Running);
        let nomad = r.modules.iter().find(|m| m.name == "nomad").unwrap();
        assert_eq!(nomad.state, ModuleState::Running);
        assert_eq!(nomad.steps[1].state, StepState::Running);
        assert!(nomad.steps[1].started_at.is_some());
        assert_eq!(nomad.steps[2].state, StepState::Pending);
        assert_eq!(r.removals[0].state, StepState::Pending);
        assert_eq!(r.counts.steps.running, 1);

        let mut refetches = 0;
        for frame in 0.. {
            let mut held = r.clone();
            let updates = live_frame(frame, &mut r);
            if updates.is_empty() {
                break;
            }
            for u in &updates {
                assert_eq!(apply_progress(&mut r, u), Applied::Changed, "frame {frame}");
                if apply_progress(&mut held, u) == Applied::Refetch {
                    refetches += 1;
                }
            }
        }
        assert_eq!(
            refetches, 1,
            "planning the deferred module asks pages to refetch"
        );
        assert_eq!(r.state, RunState::Succeeded);
        assert!(r.finished_at.is_some());
        assert_eq!(r.counts.steps.pending + r.counts.steps.running, 0);
        assert!(r.modules.iter().all(|m| !module_has_problem(m)));
    }

    #[test]
    fn the_failed_run_has_a_task_error_steps_not_run_and_a_rollback() {
        let r = report_for(
            "pi-5",
            &summary("pi-5", NEW_REV, "apply_failed"),
            Utc::now(),
        );
        assert_eq!(r.state, RunState::Failed);
        let nomad = r.modules.iter().find(|m| m.name == "nomad").unwrap();
        let e = nomad.steps[1].error.as_ref().unwrap();
        assert!(!e.causes.is_empty() && !e.advice.is_empty());
        assert_eq!(nomad.steps[2].state, StepState::NotRun);
        assert_eq!(nomad.handlers[0].state, StepState::NotRun);
        assert!(r.modules.iter().any(|m| m.state == ModuleState::RolledBack));
        assert_eq!(r.counts.steps.failed, 1);
    }

    #[test]
    fn every_host_has_a_report_for_each_of_its_runs() {
        let now = Utc::now();
        for host in super::super::data::hosts(now) {
            let runs = summaries(&host, now);
            if host.status == HostStatus::Pending {
                assert!(runs.is_empty());
            }
            for s in &runs {
                let r = report_for(&host.id, s, now);
                assert_eq!(
                    (r.id.as_str(), r.host.as_str()),
                    (s.id.as_str(), host.id.as_str())
                );
                assert_eq!(Some(r.state), s.state);
            }
            let ids: std::collections::BTreeSet<_> = runs.iter().map(|r| &r.id).collect();
            assert_eq!(ids.len(), runs.len(), "{}: run ids are unique", host.id);
        }
        let edge = summaries(
            &super::super::data::hosts(now)
                .into_iter()
                .find(|h| h.id == "edge-1")
                .unwrap(),
            now,
        );
        assert_eq!(edge[0].state, Some(RunState::NeedsAttention));
    }
}
