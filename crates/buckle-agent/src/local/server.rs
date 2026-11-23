//! The agent's end of the local socket: binds it with the right owner and
//! mode, identifies each caller from the connection's peer credentials and
//! hands requests to the session.

use super::{Actor, MAX_MESSAGE, wire};
use crate::session::LocalControl;
use buckle_proto::v1 as pb;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::server::UdsConnectInfo;

/// Where to listen and who, besides root, may connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Socket {
    pub path: PathBuf,
    /// Members of this group may plan, check health and read the status
    /// (applying stays root's).
    pub group: Option<String>,
}

fn io_error(e: std::io::Error, what: String) -> buckle_core::Error {
    buckle_core::error::wrap_user(
        e,
        what,
        &["Run the agent as root, or point `local.socket` in agent.yaml at a path it may create."],
    )
}

/// Binds `socket.path`, replacing a socket no agent answers on.
///
/// The socket is created in a private directory beside its final path,
/// given its owner and mode there, and only then renamed into place: nobody
/// can connect to it before its permissions are right, and the old socket is
/// replaced atomically.
pub fn bind(socket: &Socket) -> buckle_core::Result<tokio::net::UnixListener> {
    let path = &socket.path;
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)
        .map_err(|e| io_error(e, format!("Could not create {}.", parent.display())))?;
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(buckle_core::error::user(
            format!(
                "Another Buckle agent is already listening on {}.",
                path.display()
            ),
            &["Stop the other agent, or give this one its own `local.socket` in agent.yaml."],
        ));
    }
    let gid = match &socket.group {
        None => None,
        Some(name) => match nix::unistd::Group::from_name(name) {
            Ok(Some(group)) => Some(group.gid),
            _ => {
                return Err(buckle_core::error::user(
                    format!(
                        "The group '{name}' named by `local.group` in agent.yaml does not exist."
                    ),
                    &[
                        "Create the group (`groupadd`), or remove `local.group` to keep the socket to root.",
                    ],
                ));
            }
        },
    };
    let staging = parent.join(format!(".sock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|e| io_error(e, format!("Could not create {}.", staging.display())))?;
    let staged = staging.join("s");
    let bound = (|| {
        let listener = std::os::unix::net::UnixListener::bind(&staged)?;
        listener.set_nonblocking(true)?;
        let mode = if gid.is_some() { 0o660 } else { 0o600 };
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(mode))?;
        if gid.is_some() {
            nix::unistd::chown(&staged, None, gid).map_err(std::io::Error::from)?;
        }
        std::fs::rename(&staged, path)?;
        tokio::net::UnixListener::from_std(listener)
    })();
    let _ = std::fs::remove_dir_all(&staging);
    bound.map_err(|e| {
        io_error(
            e,
            format!("Could not create the agent's socket at {}.", path.display()),
        )
    })
}

/// Serves `listener` until `shutdown`, then removes the socket.
pub async fn serve(
    listener: tokio::net::UnixListener,
    path: PathBuf,
    control: LocalControl,
    shutdown: CancellationToken,
) {
    let service = pb::local_agent_server::LocalAgentServer::new(Service { control })
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
    let incoming = tokio_stream::wrappers::UnixListenerStream::new(listener);
    tracing::info!(socket = %path.display(), "serving local runs");
    let served = tonic::transport::Server::builder()
        .add_service(service)
        .serve_with_incoming_shutdown(incoming, shutdown.cancelled_owned())
        .await;
    if let Err(err) = served {
        tracing::warn!(error = %err, "the local socket stopped serving");
    }
    let _ = std::fs::remove_file(&path);
}

struct Service {
    control: LocalControl,
}

/// Who is on the other end of the connection `request` came over.
fn actor<T>(request: &tonic::Request<T>) -> Result<Actor, tonic::Status> {
    let cred = request
        .extensions()
        .get::<UdsConnectInfo>()
        .and_then(|info| info.peer_cred)
        .ok_or_else(|| {
            tonic::Status::permission_denied("the connection carries no peer credentials")
        })?;
    let uid = cred.uid();
    let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| uid.to_string());
    Ok(Actor {
        uid,
        user,
        pid: cred.pid(),
    })
}

type Events = std::pin::Pin<
    Box<dyn tokio_stream::Stream<Item = Result<pb::LocalEvent, tonic::Status>> + Send>,
>;

impl Service {
    async fn run(
        &self,
        request: super::Request,
        actor: Actor,
        no_wait: bool,
    ) -> tonic::Response<Events> {
        let events = self.control.submit(request, actor, no_wait).await;
        // Dropping the stream (the caller left) closes the run's channel,
        // which takes a run that has not started out of the queue.
        let stream = tokio_stream::wrappers::ReceiverStream::new(events)
            .map(|event| Ok(wire::event_to_proto(&event)));
        tonic::Response::new(Box::pin(stream))
    }
}

#[tonic::async_trait]
impl pb::local_agent_server::LocalAgent for Service {
    type PlanStream = Events;
    type HealthStream = Events;
    type ApplyStream = Events;

    async fn plan(
        &self,
        request: tonic::Request<pb::LocalPlanRequest>,
    ) -> Result<tonic::Response<Events>, tonic::Status> {
        let actor = actor(&request)?;
        let request = request.into_inner();
        let no_wait = request.no_wait;
        Ok(self.run(wire::plan_request(request), actor, no_wait).await)
    }

    async fn health(
        &self,
        request: tonic::Request<pb::LocalHealthRequest>,
    ) -> Result<tonic::Response<Events>, tonic::Status> {
        let actor = actor(&request)?;
        let request = request.into_inner();
        let no_wait = request.no_wait;
        Ok(self
            .run(wire::health_request(request), actor, no_wait)
            .await)
    }

    async fn apply(
        &self,
        request: tonic::Request<pb::LocalApplyRequest>,
    ) -> Result<tonic::Response<Events>, tonic::Status> {
        let actor = actor(&request)?;
        let request = request.into_inner();
        let no_wait = request.no_wait;
        Ok(self.run(wire::apply_request(request), actor, no_wait).await)
    }

    async fn status(
        &self,
        request: tonic::Request<pb::LocalStatusRequest>,
    ) -> Result<tonic::Response<pb::LocalStatus>, tonic::Status> {
        actor(&request)?;
        Ok(tonic::Response::new(wire::status_to_proto(
            &self.control.status().await,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).expect("stat").permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn the_socket_is_private_and_replaces_a_stale_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = Socket {
            path: dir.path().join("run/agent.sock"),
            group: None,
        };
        let first = bind(&socket).expect("binds");
        assert_eq!(mode(&socket.path), 0o600, "root only without a group");

        // A second agent must not take a live socket away.
        let err = bind(&socket).expect_err("in use");
        assert!(err.description().contains("already listening"), "{err}");

        // One whose agent is gone is replaced.
        drop(first);
        bind(&socket).expect("replaces the stale socket");
        let left: Vec<String> = std::fs::read_dir(dir.path().join("run"))
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["agent.sock"], "no staging directory is left");
    }

    #[tokio::test]
    async fn an_unknown_group_is_refused_before_anything_is_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = Socket {
            path: dir.path().join("agent.sock"),
            group: Some("buckle-no-such-group".into()),
        };
        let err = bind(&socket).expect_err("refused");
        assert!(err.description().contains("does not exist"), "{err}");
        assert!(!socket.path.exists());
    }
}
