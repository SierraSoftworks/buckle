//! Local runs: `buckle plan`, `buckle health` and `buckle status` on a host
//! whose agent is running ask that agent, over a Unix socket, instead of
//! running the engine themselves.
//!
//! * The agent runs each request as an operation on its own worker
//!   ([`crate::session`]), so it can never collide with work the server sent.
//! * A caller chooses among the revisions the server assigned
//!   ([`Choice`]); it never supplies a directory, facts or vars.
//! * Plans arrive redacted and health reports scrubbed: secret values never
//!   cross the socket.
//!
//! [`client::Client`] is the command line's end; the agent serves the other
//! (`server`, Unix only for now).

pub mod client;
#[cfg(unix)]
pub(crate) mod server;
#[cfg(unix)]
mod wire;

use buckle_core::error::ErrorReport;
use buckle_core::health::HealthReport;
use buckle_core::plan::{Plan, RevisionId};
use std::path::{Path, PathBuf};

/// Results (a plan above all) are not bounded by tonic's 4 MiB default.
#[cfg(unix)]
const MAX_MESSAGE: usize = 256 * 1024 * 1024;

/// Which of the agent's revisions a run is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Choice {
    /// The revision applied on the host.
    #[default]
    Applied,
    /// The revision the server last assigned (what a rollout wants here);
    /// the applied one when nothing newer is assigned.
    Assigned,
}

/// Who asked, from the socket's peer credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    pub uid: u32,
    /// The account's name, or the uid when it has none.
    pub user: String,
    pub pid: Option<i32>,
}

/// What a caller may ask the agent to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Plan the revision. The applied revision is planned as a drift pass
    /// plans it; a newer assigned one as its rollout would.
    Plan {
        revision: Choice,
        /// A filt-rs task selector (it only narrows the plan).
        select: Option<String>,
    },
    /// Run the health checks of the revision's modules.
    Health {
        revision: Choice,
        /// Only these modules; empty: every module selected here.
        modules: Vec<String>,
    },
}

impl Request {
    /// The command's name, as the caller typed it.
    pub fn name(&self) -> &'static str {
        match self {
            Request::Plan { .. } => "plan",
            Request::Health { .. } => "health",
        }
    }

    pub fn revision(&self) -> Choice {
        match self {
            Request::Plan { revision, .. } | Request::Health { revision, .. } => *revision,
        }
    }
}

/// One step in the life of a run: any number of `Queued`, then `Started`,
/// the run's result, and `Done` last.
#[derive(Debug, Clone)]
pub enum Event {
    /// The run waits behind other work (sent again whenever that changes).
    Queued {
        /// What runs before it, the running operation first.
        ahead: Vec<String>,
    },
    Started {
        run_id: String,
        /// The revision the run is about.
        revision: RevisionId,
        applied: Option<RevisionId>,
        assigned: Option<RevisionId>,
    },
    Plan {
        plan: Box<Plan>,
        /// The facts tree the plan was made with.
        facts: serde_json::Value,
    },
    Health(HealthReport),
    Done {
        /// The exit code the command ends with.
        exit_code: i32,
        /// Why the run could not happen, when it could not.
        error: Option<ErrorReport>,
    },
}

/// What the agent is doing, and the last run it recorded.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub host_id: String,
    pub agent_version: String,
    /// The server the agent is enrolled with; `None` for one watching a
    /// directory.
    pub server: Option<String>,
    pub connected: bool,
    /// `pending`, `active` or `paused`, as the server last said.
    pub host_status: Option<String>,
    pub applied: Option<RevisionId>,
    pub assigned: Option<RevisionId>,
    pub stable: Option<RevisionId>,
    /// What the worker is doing; `None` when it is idle.
    pub running: Option<String>,
    /// What waits behind it, in order.
    pub queued: Vec<String>,
    pub paused_modules: Vec<String>,
    pub rolled_back_modules: Vec<String>,
    /// The last run recorded in the state directory, and its full outcome.
    pub run: Option<buckle_core::state::RunRecord>,
    pub outcome: Option<buckle_core::engine::ApplyOutcome>,
}

/// Where the agent listens when agent.yaml does not say.
pub fn default_socket() -> PathBuf {
    if cfg!(target_os = "linux") {
        PathBuf::from("/run/buckle/agent.sock")
    } else {
        PathBuf::from("/var/run/buckle/agent.sock")
    }
}

/// Where the agent configured by `agent_config` listens; `None` when that
/// file does not exist (no agent is set up here). A file the caller may not
/// read (it holds the agent's bootstrap token) gives the default socket.
pub fn configured_socket(agent_config: &Path) -> buckle_core::Result<Option<PathBuf>> {
    /// Only what says where the socket is: the rest of agent.yaml is the
    /// agent's to validate.
    #[derive(serde::Deserialize)]
    struct Peek {
        #[serde(default)]
        local: crate::config::LocalConfig,
    }
    let text = match std::fs::read_to_string(agent_config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(Some(default_socket()));
        }
        Err(e) => {
            return Err(buckle_core::error::wrap_user(
                e,
                format!(
                    "Could not read the agent configuration at {}.",
                    agent_config.display()
                ),
                &["Pass --socket PATH to name the agent's socket yourself."],
            ));
        }
    };
    let peek: Peek = serde_yaml::from_str(&text).map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            format!(
                "The agent configuration at {} is not valid.",
                agent_config.display()
            ),
            &["Fix `local` in agent.yaml, or pass --socket PATH to name the agent's socket yourself."],
        )
    })?;
    Ok(Some(peek.local.socket_path()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::unset("server: https://buckle:7700\n", Some(default_socket()))]
    #[case::named(
        "server: https://buckle:7700\nlocal:\n  socket: /tmp/b.sock\n  group: ops\n",
        Some(PathBuf::from("/tmp/b.sock"))
    )]
    #[case::no_agent("", None)]
    fn the_socket_comes_from_agent_yaml(#[case] yaml: &str, #[case] expected: Option<PathBuf>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent.yaml");
        if !yaml.is_empty() {
            std::fs::write(&path, yaml).expect("write");
        }
        assert_eq!(configured_socket(&path).expect("reads"), expected);
    }
}
