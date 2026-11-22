//! `cluster.node`: an operation on this host's own node of a cluster, done
//! by the cluster's steward.

use super::hostkit::{self, StringOrArgv};
use async_trait::async_trait;
use buckle_core::error;
use buckle_core::model::RemovalPolicy;
use buckle_core::plan::{Diff, Planned};
use buckle_core::system::FilePerms;
use buckle_core::task::{Applied, ClusterCredentials, Cx, NodeOp, NodeReply, NodeRequest, Task};
use buckle_core::{Platforms, Result};
use schemars::JsonSchema;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Asks the steward of a cluster this host is a node of to act on the
/// host's own node. The host holds no cluster credential for it: the Buckle
/// server checks that the host matches the cluster's `nodes`, and the
/// steward acts on the node named like the host.
///
/// ```yaml
/// - cluster.node: { cluster: nomad, op: undrain }   # eligible again after a reboot
/// - cluster.node: { cluster: nomad, op: drain, deadline: 10m }
///   when: 'host.labels.maintenance == true'
/// - cluster.node:
///     cluster: nomad
///     op: join                                      # a token for the pool the server picks
///     dest: /etc/nomad.d/intro.jwt
///     unless: test -s /opt/nomad/data/client/client-id
/// ```
///
/// - `undrain` stops a drain and marks the node eligible (Kubernetes:
///   uncordon). The node's state is read on every plan, so a drift pass
///   hands a node back whenever it finds it out of scheduling.
/// - `drain` takes the node out of scheduling and moves its work elsewhere
///   (Kubernetes: cordon and evict). A node already out of scheduling is
///   left as it is.
/// - `join` writes a token the node joins the cluster with to `dest` (mode
///   `0600` unless set): a Nomad client introduction token, a Kubernetes
///   bootstrap token. The server picks the pool from the cluster's `pools`;
///   the host has no say. It runs on every apply unless `unless` succeeds,
///   so guard it with something the node keeps once it has joined.
///
/// Every operation needs the Buckle server and the cluster's steward to be
/// reachable. Nothing is undone on removal.
pub struct ClusterNode;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The cluster this host is a node of, as `fleet.clusters` names it
    /// (never a scoped target).
    pub cluster: String,
    pub op: Op,
    /// `join`: where the token is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dest: Option<PathBuf>,
    /// `join`: the token file's mode, owner and group. Default mode `0600`.
    #[serde(flatten)]
    pub perms: FilePerms,
    /// Skip the operation when this command succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unless: Option<StringOrArgv>,
    /// `drain`: how long the node's work gets to move before it is stopped.
    /// Default: the steward's (one hour).
    #[serde(
        default,
        with = "humantime_serde",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    pub deadline: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Undrain,
    Drain,
    Join,
}

impl Op {
    fn node_op(self) -> NodeOp {
        match self {
            Op::Undrain => NodeOp::Undrain,
            Op::Drain => NodeOp::Drain,
            Op::Join => NodeOp::Join,
        }
    }
}

impl ClusterNode {
    fn check(spec: &Spec) -> Result<()> {
        let misplaced = |field: &str, op: &str| {
            Err(error::user(
                format!("cluster.node takes `{field}` only with `op: {op}`."),
                &["Remove it, or change the operation."],
            ))
        };
        match (spec.op, &spec.dest) {
            (Op::Join, None) => {
                return Err(error::user(
                    "cluster.node `op: join` needs `dest`, the file the token is written to.",
                    &["Set `dest` to the path the scheduler's client reads its join token from."],
                ));
            }
            (Op::Join, Some(dest)) => hostkit::require_absolute(dest, "dest")?,
            (_, Some(_)) => return misplaced("dest", "join"),
            (_, None) if hostkit::has_perms(&spec.perms) => return misplaced("mode", "join"),
            _ => {}
        }
        if spec.deadline.is_some() && spec.op != Op::Drain {
            return misplaced("deadline", "drain");
        }
        Ok(())
    }

    fn steward<'a>(cx: &'a Cx, spec: &Spec) -> Result<&'a std::sync::Arc<dyn ClusterCredentials>> {
        cx.clusters.as_ref().ok_or_else(|| {
            error::user(
                format!(
                    "cluster.node asks {}'s steward to act, which only an enrolled agent can do.",
                    spec.cluster
                ),
                &["Run it through the Buckle server."],
            )
        })
    }

    async fn ask(cx: &Cx, spec: &Spec, op: NodeOp) -> Result<NodeReply> {
        Self::steward(cx, spec)?
            .node(&NodeRequest {
                target: spec.cluster.clone(),
                op,
                deadline: spec.deadline,
            })
            .await
    }

    /// Why the operation is skipped, when `unless` succeeds.
    async fn skipped(cx: &Cx, spec: &Spec) -> Result<Option<String>> {
        let Some(unless) = &spec.unless else {
            return Ok(None);
        };
        let out = hostkit::probe(cx, unless.to_command(cx.platform())?).await?;
        Ok(out
            .success()
            .then(|| format!("`{}` succeeded", unless.display())))
    }

    fn intent(spec: &Spec) -> String {
        match (spec.op, &spec.dest) {
            (Op::Undrain, _) => format!("hand this node back to {}'s scheduler", spec.cluster),
            (Op::Drain, _) => format!("drain this node of {}", spec.cluster),
            (Op::Join, dest) => format!(
                "write a token to join {} to {}",
                spec.cluster,
                dest.as_deref()
                    .unwrap_or(std::path::Path::new(""))
                    .display()
            ),
        }
    }
}

#[async_trait]
impl Task for ClusterNode {
    type Spec = Spec;
    const KIND: &'static str = "cluster.node";
    const PLATFORMS: Platforms = Platforms::ALL;
    const REMOVAL_DEFAULT: RemovalPolicy = RemovalPolicy::Leave;
    const DESCRIPTION: &'static str = "Have a cluster's steward undrain, drain or issue a join token for this host's own node; the host holds no cluster credential.";

    fn key(spec: &Spec) -> String {
        format!("{}:{}", spec.cluster, spec.op.node_op())
    }

    fn always_runs(spec: &Spec) -> bool {
        spec.op == Op::Join && spec.unless.is_none()
    }

    fn describe(spec: &Spec) -> String {
        Self::intent(spec)
    }

    #[tracing::instrument(name = "task.cluster.node.plan", skip_all, fields(cluster = %spec.cluster, op = %spec.op.node_op()))]
    async fn plan(&self, cx: &Cx, spec: &Spec) -> Result<Planned> {
        Self::check(spec)?;
        Self::steward(cx, spec)?;
        if Self::skipped(cx, spec).await?.is_some() {
            return Ok(Planned::NoOp);
        }
        let settled = match spec.op {
            Op::Join => false,
            Op::Undrain => {
                let node = Self::ask(cx, spec, NodeOp::Status).await?;
                node.registered && node.eligible && !node.draining
            }
            // A node the scheduler does not know has nothing to move.
            Op::Drain => {
                let node = Self::ask(cx, spec, NodeOp::Status).await?;
                !node.registered || node.draining || !node.eligible
            }
        };
        Ok(if settled {
            Planned::NoOp
        } else {
            Planned::change(Diff::Opaque {
                summary: Self::intent(spec),
            })
        })
    }

    #[tracing::instrument(name = "task.cluster.node.apply", skip_all, fields(cluster = %spec.cluster, op = %spec.op.node_op()))]
    async fn apply(&self, cx: &Cx, spec: &Spec, _planned: &Planned) -> Result<Applied> {
        Self::check(spec)?;
        if let Some(reason) = Self::skipped(cx, spec).await? {
            return Ok(Applied::unchanged(format!("skipped: {reason}")));
        }
        let reply = Self::ask(cx, spec, spec.op.node_op()).await?;
        match (spec.op, &spec.dest) {
            (Op::Join, Some(dest)) => {
                let token = reply.token.as_ref().ok_or_else(|| {
                    error::system(
                        format!("{}'s steward answered without a join token.", spec.cluster),
                        &["Check the steward's agent log."],
                    )
                })?;
                let existing = cx.system.stat(dest).await?;
                let mut perms = hostkit::effective_perms(&spec.perms, existing.as_ref());
                if spec.perms.mode.is_none() {
                    perms.mode = Some(0o600);
                }
                cx.system
                    .write_atomic(dest, token.expose_secret().as_bytes(), &perms)
                    .await?;
                let pool = match reply.pool.as_str() {
                    "" => "the default pool".to_owned(),
                    pool => format!("the pool {pool}"),
                };
                Ok(Applied::changed(format!(
                    "wrote a token to join {pool} of {} to {}",
                    spec.cluster,
                    dest.display()
                )))
            }
            (Op::Undrain, _) if !reply.registered => Err(error::user(
                format!(
                    "{} has no live node named like this host yet.",
                    spec.cluster
                ),
                &[
                    "Give the task a `retry` so it waits for the node to register, and check that the node is named like the host: its id, or its tailnet DNS name.",
                ],
            )),
            _ if reply.changed => Ok(Applied::changed(reply.summary)),
            _ => Ok(Applied::unchanged(reply.summary)),
        }
    }
}

buckle_core::register_task!(ClusterNode);

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// A steward answering with `node` for every request.
    #[derive(Debug, Default)]
    struct Steward {
        node: NodeReply,
        asked: Mutex<Vec<NodeOp>>,
    }

    #[async_trait]
    impl ClusterCredentials for Steward {
        async fn credential(
            &self,
            _: &buckle_core::task::CredentialRequest,
        ) -> Result<secrecy::SecretString> {
            unreachable!("cluster.node asks for no credential")
        }

        async fn node(&self, request: &NodeRequest) -> Result<NodeReply> {
            self.asked.lock().unwrap().push(request.op);
            Ok(NodeReply {
                changed: request.op != NodeOp::Status,
                summary: format!("{} done", request.op),
                ..self.node.clone()
            })
        }
    }

    fn steward(registered: bool, eligible: bool, draining: bool) -> Arc<Steward> {
        Arc::new(Steward {
            node: NodeReply {
                registered,
                eligible,
                draining,
                pool: "public".into(),
                token: Some("join-token".into()),
                ..Default::default()
            },
            asked: Mutex::default(),
        })
    }

    fn cx(sys: &FakeSystem, steward: &Arc<Steward>) -> Cx {
        buckle_core::testing::CxBuilder::new(sys.clone())
            .clusters(steward.clone())
            .build()
    }

    fn spec(raw: serde_json::Value) -> Spec {
        serde_json::from_value(raw).unwrap()
    }

    /// A node already in the state the operation wants plans as no change,
    /// read from the scheduler each time.
    #[rstest]
    #[case::eligible_node_stays("undrain", true, true, false, false)]
    #[case::drained_node_is_handed_back("undrain", true, false, false, true)]
    #[case::draining_node_is_handed_back("undrain", true, true, true, true)]
    #[case::unregistered_node_is_handed_back_once_it_appears("undrain", false, false, false, true)]
    #[case::eligible_node_is_drained("drain", true, true, false, true)]
    #[case::drained_node_stays("drain", true, false, false, false)]
    #[case::unregistered_node_has_nothing_to_drain("drain", false, false, false, false)]
    #[tokio::test]
    async fn a_plan_reads_the_nodes_state(
        #[case] op: &str,
        #[case] registered: bool,
        #[case] eligible: bool,
        #[case] draining: bool,
        #[case] change: bool,
    ) {
        let (sys, steward) = (FakeSystem::new(), steward(registered, eligible, draining));
        let planned = ClusterNode
            .plan(
                &cx(&sys, &steward),
                &spec(json!({"cluster": "nomad", "op": op})),
            )
            .await
            .unwrap();
        assert_eq!(planned.is_change(), change);
        assert_eq!(*steward.asked.lock().unwrap(), [NodeOp::Status]);
    }

    #[tokio::test]
    async fn an_apply_reports_what_the_steward_did() {
        let (sys, steward) = (FakeSystem::new(), steward(true, false, false));
        let s = spec(json!({"cluster": "nomad", "op": "undrain"}));
        let applied = ClusterNode
            .apply(&cx(&sys, &steward), &s, &Planned::NoOp)
            .await
            .unwrap();
        assert!(applied.changed);
        assert_eq!(*steward.asked.lock().unwrap(), [NodeOp::Undrain]);
    }

    #[tokio::test]
    async fn undrain_fails_until_the_node_registers() {
        let (sys, steward) = (FakeSystem::new(), steward(false, false, false));
        let s = spec(json!({"cluster": "nomad", "op": "undrain"}));
        let outcome = ClusterNode
            .apply(&cx(&sys, &steward), &s, &Planned::NoOp)
            .await;
        assert!(outcome.is_err());
    }

    /// The token reaches the file and nothing else; once the guard passes
    /// no token is asked for.
    #[tokio::test]
    async fn join_writes_the_token_until_its_guard_passes() {
        let sys = FakeSystem::new().on_command_fail("sh", &["-c", "test -s /node-id"], 1, "");
        let steward = steward(false, false, false);
        let s = spec(json!({
            "cluster": "nomad", "op": "join",
            "dest": "/etc/nomad.d/intro.jwt", "unless": "test -s /node-id",
        }));
        let c = cx(&sys, &steward);

        let planned = ClusterNode.plan(&c, &s).await.unwrap();
        let applied = ClusterNode.apply(&c, &s, &planned).await.unwrap();

        assert!(planned.is_change());
        assert!(!format!("{planned:?}{applied:?}").contains("join-token"));
        assert!(applied.summary.contains("public"), "{}", applied.summary);
        assert_eq!(
            sys.file("/etc/nomad.d/intro.jwt").as_deref(),
            Some(b"join-token".as_slice())
        );
        assert_eq!(*steward.asked.lock().unwrap(), [NodeOp::Join]);

        let joined = FakeSystem::new().on_command_ok("sh", &["-c", "test -s /node-id"], "");
        let c = cx(&joined, &steward);
        assert!(!ClusterNode.plan(&c, &s).await.unwrap().is_change());
        assert!(!ClusterNode.apply(&c, &s, &planned).await.unwrap().changed);
        assert_eq!(steward.asked.lock().unwrap().len(), 1);
    }

    #[rstest]
    #[case::join_without_dest(json!({"cluster": "nomad", "op": "join"}))]
    #[case::dest_without_join(json!({"cluster": "nomad", "op": "drain", "dest": "/x"}))]
    #[case::deadline_without_drain(json!({"cluster": "nomad", "op": "undrain", "deadline": "5m"}))]
    #[tokio::test]
    async fn a_spec_that_mixes_operations_is_refused(#[case] raw: serde_json::Value) {
        let (sys, steward) = (FakeSystem::new(), steward(true, true, false));
        assert!(
            ClusterNode
                .plan(&cx(&sys, &steward), &spec(raw))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_local_run_has_no_steward_to_ask() {
        let sys = FakeSystem::new();
        let c = buckle_core::testing::CxBuilder::new(sys).build();
        let s = spec(json!({"cluster": "nomad", "op": "undrain"}));
        assert!(ClusterNode.plan(&c, &s).await.is_err());
    }
}
