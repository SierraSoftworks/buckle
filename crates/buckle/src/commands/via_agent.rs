//! `buckle plan|health|status` without `--config`: the command is carried
//! out by the Buckle agent running on this host, over its local socket, and
//! only rendered here.
//!
//! * The agent runs it against a revision the server assigned, in turn with
//!   its own work; this process never touches the state directory.
//! * Progress notes (what the run waits behind, which revision it is about)
//!   go to stderr, so stdout stays the result alone.
//! * A host with no agent set up is an error naming `--config`, never a
//!   silent run of the engine here.

use super::local::OutputFormat;
use crate::render;
use buckle_agent::local::client::Client;
use buckle_agent::local::{Choice, Event, Request, Status};
use buckle_core::error::{ErrorKind, ErrorReport};
use buckle_core::health::HealthReport;
use buckle_core::plan::{Plan, RevisionId};
use clap::Args;
use std::io::Write;
use std::path::PathBuf;

/// Where the agent is.
#[derive(Args, Debug, Clone)]
pub struct AgentArgs {
    /// The agent's configuration, read for where its socket is.
    #[arg(long, env = "BUCKLE_AGENT_CONFIG", value_name = "FILE", default_value = super::agent::default_config_path())]
    pub agent_config: PathBuf,

    /// The agent's socket, when it is not the one agent.yaml names.
    #[arg(long, env = "BUCKLE_AGENT_SOCKET", value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl Default for AgentArgs {
    fn default() -> Self {
        Self {
            agent_config: super::agent::default_config_path().into(),
            socket: None,
        }
    }
}

/// Which of the agent's revisions a command is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Revision {
    /// The revision applied on this host.
    #[default]
    Applied,
    /// The revision the server last assigned this host (what its rollout
    /// will apply).
    Assigned,
}

impl From<Revision> for Choice {
    fn from(revision: Revision) -> Self {
        match revision {
            Revision::Applied => Choice::Applied,
            Revision::Assigned => Choice::Assigned,
        }
    }
}

/// How a command runs through the agent.
#[derive(Args, Debug, Clone, Default)]
pub struct RunArgs {
    #[command(flatten)]
    pub agent: AgentArgs,

    /// Without --config: which of the agent's revisions to use.
    #[arg(long, value_enum, default_value_t = Revision::Applied, conflicts_with = "config")]
    pub revision: Revision,

    /// Without --config: fail instead of waiting when the agent is busy.
    #[arg(long, conflicts_with = "config")]
    pub no_wait: bool,
}

impl AgentArgs {
    /// The socket of the agent set up on this host; `None` when none is.
    pub fn find(&self) -> buckle_core::Result<Option<PathBuf>> {
        match &self.socket {
            Some(socket) => Ok(Some(socket.clone())),
            None => buckle_agent::local::configured_socket(&self.agent_config),
        }
    }

    /// The agent's socket, for a command that needs the agent.
    fn socket(&self) -> buckle_core::Result<PathBuf> {
        self.find()?.ok_or_else(|| {
            buckle_core::error::user(
                format!(
                    "No Buckle agent is set up on this host ({} does not exist) and no configuration directory was given.",
                    self.agent_config.display()
                ),
                // Shown last first.
                &[
                    "Run `buckle up` to enrol this host with a Buckle server.",
                    "If the agent's configuration is somewhere else, pass --agent-config FILE.",
                    "Pass --config DIR to run against a configuration directory.",
                ],
            )
        })
    }
}

/// One link in the chain of causes of an error the agent reported.
#[derive(Debug)]
struct Cause {
    message: String,
    source: Option<Box<Cause>>,
}

impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Cause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|c| c.as_ref() as _)
    }
}

/// The error the agent reported, as this command's own.
fn error_from(report: ErrorReport) -> buckle_core::Error {
    // Advice is `'static` in an error; the command ends with this one. An
    // error lists its advice last first, a report in reading order.
    let advice: Vec<&'static str> = report
        .advice
        .into_iter()
        .rev()
        .map(|a| &*Box::leak(a.into_boxed_str()))
        .collect();
    let advice: &'static [&'static str] = Box::leak(advice.into_boxed_slice());
    let cause = report
        .causes
        .into_iter()
        .rev()
        .fold(None, |source, message| {
            Some(Box::new(Cause { message, source }))
        });
    match (report.kind, cause) {
        (ErrorKind::User, None) => buckle_core::error::user(report.message, advice),
        (ErrorKind::System, None) => buckle_core::error::system(report.message, advice),
        (ErrorKind::User, Some(cause)) => {
            buckle_core::error::wrap_user(*cause, report.message, advice)
        }
        (ErrorKind::System, Some(cause)) => {
            buckle_core::error::wrap_system(*cause, report.message, advice)
        }
    }
}

/// What a run through the agent came back with.
#[derive(Default)]
struct Ran {
    revision: Option<RevisionId>,
    applied: Option<RevisionId>,
    assigned: Option<RevisionId>,
    plan: Option<(Plan, serde_json::Value)>,
    health: Option<HealthReport>,
    exit_code: i32,
}

impl Ran {
    /// How the revision the run was about reads in a header.
    fn label(&self) -> String {
        match &self.revision {
            Some(revision) if self.applied.as_ref() == Some(revision) => {
                format!("{revision} (applied)")
            }
            Some(revision) => format!("{revision} (assigned, not applied yet)"),
            None => "unknown".into(),
        }
    }

    /// What the caller should know beside the result.
    fn note(&self, asked: Revision) -> Option<String> {
        let newer = self
            .assigned
            .as_ref()
            .filter(|assigned| Some(*assigned) != self.applied.as_ref());
        match (asked, newer) {
            (Revision::Applied, Some(assigned)) if self.applied.is_some() => Some(format!(
                "note: the server has assigned revision {} to this host; preview it with --revision assigned.",
                assigned.short()
            )),
            (Revision::Assigned, None) => Some(
                "note: no newer revision is assigned; this is the one applied here.".to_owned(),
            ),
            _ => None,
        }
    }
}

/// Runs `request` on the agent, saying on `notes` what it waits behind.
async fn run(args: &RunArgs, request: Request, notes: &mut dyn Write) -> buckle_core::Result<Ran> {
    let mut client = Client::connect(&args.agent.socket()?).await?;
    let mut events = client.run(request, args.no_wait).await?;
    let mut ran = Ran::default();
    loop {
        match events.next().await? {
            None => {
                return Err(buckle_core::error::user(
                    "The Buckle agent stopped before the run finished.",
                    &[
                        "Run the command again once the agent is back: `systemctl status buckle-agent`.",
                    ],
                ));
            }
            Some(Event::Queued { ahead }) => {
                let _ = writeln!(notes, "waiting behind: {}", ahead.join(", "));
            }
            Some(Event::Started {
                revision,
                applied,
                assigned,
                ..
            }) => {
                ran.revision = Some(revision);
                ran.applied = applied;
                ran.assigned = assigned;
            }
            Some(Event::Plan { plan, facts }) => ran.plan = Some((*plan, facts)),
            Some(Event::Health(report)) => ran.health = Some(report),
            Some(Event::Done { exit_code, error }) => {
                return match error {
                    Some(error) => Err(error_from(error)),
                    None => {
                        ran.exit_code = exit_code;
                        Ok(ran)
                    }
                };
            }
        }
    }
}

/// A run that ended well without the result its command is for.
fn no_result(what: &'static str) -> buckle_core::Error {
    buckle_core::error::system(
        format!("The Buckle agent finished the run without sending {what}."),
        &["Run the same Buckle version as the agent: `buckle --version`."],
    )
}

fn json<T: serde::Serialize>(out: &mut dyn Write, value: &T) {
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    );
}

/// `buckle plan` through the agent.
pub(crate) async fn plan(
    args: &RunArgs,
    select: Option<String>,
    output: OutputFormat,
    out: &mut dyn Write,
    notes: &mut dyn Write,
) -> buckle_core::Result<i32> {
    let request = Request::Plan {
        revision: args.revision.into(),
        select,
    };
    let ran = run(args, request, notes).await?;
    let (plan, facts) = ran.plan.as_ref().ok_or_else(|| no_result("a plan"))?;
    match output {
        OutputFormat::Text => {
            let _ = out.write_all(render::render_plan(plan, &ran.label(), Some(facts)).as_bytes());
        }
        OutputFormat::Json => json(out, plan),
    }
    if let Some(note) = ran.note(args.revision) {
        let _ = writeln!(notes, "{note}");
    }
    Ok(ran.exit_code)
}

/// `buckle health` through the agent.
pub(crate) async fn health(
    args: &RunArgs,
    modules: Vec<String>,
    output: OutputFormat,
    out: &mut dyn Write,
    notes: &mut dyn Write,
) -> buckle_core::Result<i32> {
    let request = Request::Health {
        revision: args.revision.into(),
        modules,
    };
    let ran = run(args, request, notes).await?;
    let report = ran
        .health
        .as_ref()
        .ok_or_else(|| no_result("a health report"))?;
    match output {
        OutputFormat::Text => {
            let _ = writeln!(out, "revision  {}", ran.label());
            let _ = out.write_all(render::render_health(report, &[]).as_bytes());
        }
        OutputFormat::Json => json(out, report),
    }
    if let Some(note) = ran.note(args.revision) {
        let _ = writeln!(notes, "{note}");
    }
    Ok(ran.exit_code)
}

/// What the agent is doing, in the lines above its last run.
fn render_agent(status: &Status) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let link = match (&status.server, status.connected) {
        (Some(server), true) => format!("connected to {server}"),
        (Some(server), false) => format!("not connected to {server}"),
        (None, _) => "watching a local directory".to_owned(),
    };
    let _ = write!(
        out,
        "agent     {} · {} · {link}",
        status.host_id, status.agent_version
    );
    if let Some(host_status) = &status.host_status {
        let _ = write!(out, " ({host_status})");
    }
    out.push('\n');
    let revision = |r: &Option<RevisionId>| match r {
        Some(r) => r.short().to_owned(),
        None => "none".to_owned(),
    };
    let _ = write!(out, "revision  applied {}", revision(&status.applied));
    if status.assigned.is_some() && status.assigned != status.applied {
        let _ = write!(out, " · assigned {}", revision(&status.assigned));
    }
    if status.stable.is_some() && status.stable != status.applied {
        let _ = write!(out, " · stable {}", revision(&status.stable));
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "doing     {}",
        status.running.as_deref().unwrap_or("nothing (idle)")
    );
    if !status.queued.is_empty() {
        let _ = writeln!(out, "queued    {}", status.queued.join(", "));
    }
    if !status.paused_modules.is_empty() {
        let _ = writeln!(out, "paused    {}", status.paused_modules.join(", "));
    }
    if !status.rolled_back_modules.is_empty() {
        let _ = writeln!(
            out,
            "rolled back  {}",
            status.rolled_back_modules.join(", ")
        );
    }
    out
}

/// `buckle status` through the agent listening on `socket`.
pub(crate) async fn status(
    socket: &std::path::Path,
    output: OutputFormat,
    out: &mut dyn Write,
) -> buckle_core::Result<i32> {
    let status = Client::connect(socket).await?.status().await?;
    let last = super::local::LastRun {
        run: status.run.clone(),
        outcome: status.outcome.clone(),
    };
    match output {
        OutputFormat::Json => json(
            out,
            &serde_json::json!({
                "agent": {
                    "host_id": status.host_id,
                    "version": status.agent_version,
                    "server": status.server,
                    "connected": status.connected,
                    "host_status": status.host_status,
                    "applied_revision": status.applied,
                    "assigned_revision": status.assigned,
                    "stable_revision": status.stable,
                    "running": status.running,
                    "queued": status.queued,
                    "paused_modules": status.paused_modules,
                    "rolled_back_modules": status.rolled_back_modules,
                },
                "run": last.run,
                "outcome": last.outcome,
            }),
        ),
        OutputFormat::Text => {
            let _ = out.write_all(render_agent(&status).as_bytes());
            let _ = writeln!(out);
            last.render(out, "the agent has not recorded a run yet");
        }
    }
    Ok(last.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn ran(applied: Option<&str>, assigned: Option<&str>, revision: &str) -> Ran {
        Ran {
            revision: Some(RevisionId::new(revision)),
            applied: applied.map(RevisionId::new),
            assigned: assigned.map(RevisionId::new),
            ..Ran::default()
        }
    }

    #[rstest]
    #[case::in_step(Revision::Applied, Some("a"), Some("a"), "a", "(applied)", None)]
    #[case::newer_assigned(
        Revision::Applied,
        Some("a"),
        Some("b"),
        "a",
        "(applied)",
        Some("--revision assigned")
    )]
    #[case::preview(Revision::Assigned, Some("a"), Some("b"), "b", "(assigned", None)]
    #[case::nothing_newer(
        Revision::Assigned,
        Some("a"),
        Some("a"),
        "a",
        "(applied)",
        Some("no newer revision")
    )]
    fn a_run_says_which_revision_it_was_about(
        #[case] asked: Revision,
        #[case] applied: Option<&str>,
        #[case] assigned: Option<&str>,
        #[case] revision: &str,
        #[case] label: &str,
        #[case] note: Option<&str>,
    ) {
        let ran = ran(applied, assigned, revision);
        assert!(ran.label().contains(label), "{}", ran.label());
        let said = ran.note(asked);
        match note {
            Some(expected) => assert!(
                said.as_deref().is_some_and(|n| n.contains(expected)),
                "{said:?}"
            ),
            None => assert_eq!(said, None),
        }
    }

    #[test]
    fn an_error_the_agent_reported_keeps_its_causes_and_advice() {
        let report = ErrorReport::user("This host has not applied a revision yet.")
            .with_causes(["the state is empty", "nothing was assigned"])
            .with_advice(["Preview the assigned one.", "Or roll one out."]);
        let err = error_from(report.clone());
        assert_eq!(ErrorReport::of(&err), report);
        assert!(error_from(ErrorReport::system("boom")).is(buckle_core::error::Kind::System));
    }

    #[test]
    fn a_host_without_an_agent_is_told_to_pass_a_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = AgentArgs {
            agent_config: dir.path().join("agent.yaml"),
            socket: None,
        };
        let err = args.socket().expect_err("no agent");
        assert!(err.description().contains("No Buckle agent is set up"));
        assert!(err.advice().iter().any(|a| a.contains("--config DIR")));
    }

    #[test]
    fn the_agent_s_state_reads_at_a_glance() {
        let status = Status {
            host_id: "seer".into(),
            agent_version: "2.21.0".into(),
            server: Some("https://buckle:7700".into()),
            connected: true,
            host_status: Some("active".into()),
            applied: Some(RevisionId::new("aaaaaaaa1111")),
            assigned: Some(RevisionId::new("bbbbbbbb2222")),
            stable: Some(RevisionId::new("aaaaaaaa1111")),
            running: Some("apply bbbbbbbb (rollout r1)".into()),
            queued: vec!["local plan by ben".into()],
            ..Status::default()
        };
        let text = render_agent(&status);
        for expected in [
            "seer",
            "connected to https://buckle:7700",
            "applied aaaaaaaa",
            "assigned bbbbbbbb",
            "apply bbbbbbbb (rollout r1)",
            "local plan by ben",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        assert!(
            !text.contains("stable"),
            "stable is the applied one:\n{text}"
        );
    }
}
