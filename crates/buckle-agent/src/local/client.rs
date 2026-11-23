//! The command line's end of the local socket.

use super::{Event, Request, Status};
use std::path::Path;

/// A connection to the agent running on this host.
pub struct Client {
    #[cfg(unix)]
    inner: buckle_proto::v1::local_agent_client::LocalAgentClient<tonic::transport::Channel>,
}

/// The events of one run, in order; the last is [`Event::Done`].
pub struct Events {
    #[cfg(unix)]
    stream: tonic::Streaming<buckle_proto::v1::LocalEvent>,
}

/// Why the agent could not be reached on `path`.
#[cfg(unix)]
fn unreachable(path: &Path, err: &std::io::Error) -> buckle_core::Error {
    match err.kind() {
        std::io::ErrorKind::PermissionDenied => buckle_core::error::user(
            format!(
                "You may not talk to the Buckle agent on {}.",
                path.display()
            ),
            &[
                "Run the command as root, or ask for membership of the group `local.group` names in agent.yaml.",
            ],
        ),
        _ => buckle_core::error::user(
            format!("No Buckle agent is answering on {}.", path.display()),
            // Shown last first.
            &[
                "To run against a configuration directory instead of the agent's revision, pass --config DIR.",
                "Check that the agent is running: `systemctl status buckle-agent`.",
            ],
        ),
    }
}

/// A call the agent answered with an error status.
#[cfg(unix)]
fn refused(status: tonic::Status) -> buckle_core::Error {
    match status.code() {
        tonic::Code::Unimplemented => buckle_core::error::user(
            "The Buckle agent on this host is too old for this command.",
            &["Upgrade the agent from the server's hosts page, then try again."],
        ),
        _ => buckle_core::error::wrap_system(
            status,
            "The Buckle agent closed the request.",
            &["Check the agent's log: `journalctl -u buckle-agent`."],
        ),
    }
}

#[cfg(unix)]
impl Client {
    /// Connects to the agent listening on `path`.
    pub async fn connect(path: &Path) -> buckle_core::Result<Self> {
        use buckle_proto::v1::local_agent_client::LocalAgentClient;
        // Checked first for the error: tonic's own hides the cause.
        let probe = tokio::net::UnixStream::connect(path)
            .await
            .map_err(|e| unreachable(path, &e))?;
        drop(probe);
        let socket = path.to_path_buf();
        // The URI is a placeholder: the connector dials the socket.
        let channel = tonic::transport::Endpoint::from_static("http://buckle.agent")
            .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
                let socket = socket.clone();
                async move {
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                        tokio::net::UnixStream::connect(socket).await?,
                    ))
                }
            }))
            .await
            .map_err(|e| {
                buckle_core::error::wrap_system(
                    e,
                    format!("Could not talk to the Buckle agent on {}.", path.display()),
                    &["Check the agent's log: `journalctl -u buckle-agent`."],
                )
            })?;
        Ok(Self {
            inner: LocalAgentClient::new(channel)
                .max_decoding_message_size(super::MAX_MESSAGE)
                .max_encoding_message_size(super::MAX_MESSAGE),
        })
    }

    /// Asks the agent to run `request`. With `no_wait` it refuses rather
    /// than queue behind other work.
    pub async fn run(&mut self, request: Request, no_wait: bool) -> buckle_core::Result<Events> {
        use buckle_proto::v1 as pb;
        let stream = match request {
            Request::Plan { revision, select } => {
                self.inner
                    .plan(pb::LocalPlanRequest {
                        revision: pb::LocalRevision::from(revision) as i32,
                        select: select.unwrap_or_default(),
                        no_wait,
                    })
                    .await
            }
            Request::Health { revision, modules } => {
                self.inner
                    .health(pb::LocalHealthRequest {
                        revision: pb::LocalRevision::from(revision) as i32,
                        modules,
                        no_wait,
                    })
                    .await
            }
            Request::Apply {
                select,
                waive,
                include_rolled_back,
                reboot,
            } => {
                self.inner
                    .apply(pb::LocalApplyRequest {
                        select: select.unwrap_or_default(),
                        waive,
                        include_rolled_back,
                        reboot,
                        no_wait,
                    })
                    .await
            }
        }
        .map_err(refused)?;
        Ok(Events {
            stream: stream.into_inner(),
        })
    }

    /// What the agent is doing and the last run it recorded.
    pub async fn status(&mut self) -> buckle_core::Result<Status> {
        let status = self
            .inner
            .status(buckle_proto::v1::LocalStatusRequest {})
            .await
            .map_err(refused)?;
        Ok(super::wire::status_from_proto(status.into_inner()))
    }
}

#[cfg(unix)]
impl Events {
    /// The next event; `None` once the agent has closed the run. A run
    /// that ends without [`Event::Done`] was dropped (the agent stopped).
    pub async fn next(&mut self) -> buckle_core::Result<Option<Event>> {
        loop {
            match self.stream.message().await.map_err(refused)? {
                None => return Ok(None),
                Some(event) => {
                    if let Some(event) = super::wire::event_from_proto(event)? {
                        return Ok(Some(event));
                    }
                }
            }
        }
    }
}

#[cfg(not(unix))]
fn unsupported<T>() -> buckle_core::Result<T> {
    Err(buckle_core::error::user(
        "Runs through the Buckle agent are not available on this platform yet.",
        &["Pass --config DIR to run against a configuration directory."],
    ))
}

#[cfg(not(unix))]
impl Client {
    /// Connects to the agent listening on `path`.
    pub async fn connect(_path: &Path) -> buckle_core::Result<Self> {
        unsupported()
    }

    /// Asks the agent to run `request`.
    pub async fn run(&mut self, _request: Request, _no_wait: bool) -> buckle_core::Result<Events> {
        unsupported()
    }

    /// What the agent is doing and the last run it recorded.
    pub async fn status(&mut self) -> buckle_core::Result<Status> {
        unsupported()
    }
}

#[cfg(not(unix))]
impl Events {
    /// The next event.
    pub async fn next(&mut self) -> buckle_core::Result<Option<Event>> {
        unsupported()
    }
}
