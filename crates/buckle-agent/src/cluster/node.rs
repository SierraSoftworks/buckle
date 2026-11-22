//! Node operations: what a cluster's steward does to one of the cluster's
//! nodes when that node's host asks through the server. The steward acts
//! with a `node` credential; the host holds none.

use super::{Access, Executor};
use crate::identity::Purpose;
use buckle_core::model::ClusterKind;
use buckle_core::task::{NodeOp, NodeReply};
use std::time::Duration;

/// How long a join token is valid: long enough for the host to write it and
/// start its client.
pub(crate) const JOIN_TTL: Duration = Duration::from_secs(10 * 60);
/// How long a drained node's work gets to move unless the task says.
pub(crate) const DRAIN_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// One node operation, as the server passed it on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NodeTask<'a> {
    pub op: NodeOp,
    /// The node's name: the id of the host that asked.
    pub node: &'a str,
    /// `join`: the pool the server picked; `None`: the default pool.
    pub pool: Option<&'a str>,
    pub deadline: Option<Duration>,
}

impl Executor {
    /// Carries out `task` on the cluster `access` reaches.
    pub(crate) async fn node(
        &self,
        access: Access<'_>,
        task: NodeTask<'_>,
    ) -> buckle_core::Result<NodeReply> {
        if access.spec.bounds.scoped() {
            return Err(buckle_core::error::user(
                format!(
                    "{} is a scoped target; nodes belong to its cluster.",
                    access.target
                ),
                &["Name the cluster itself in `cluster.node`."],
            ));
        }
        match access.spec.kind {
            ClusterKind::Nomad => self.nomad_node(access, task).await,
            ClusterKind::Kubernetes => self.kube_node(access, task).await,
        }
    }

    async fn nomad_node(
        &self,
        access: Access<'_>,
        task: NodeTask<'_>,
    ) -> buckle_core::Result<NodeReply> {
        use buckle_nomad::nodes;
        let nomad = self.nomad_as(access, Purpose::Node, None).await?;
        let name = task.node;
        if task.op == NodeOp::Join {
            let token = nodes::introduction_token(&nomad, name, task.pool, JOIN_TTL).await?;
            return Ok(NodeReply {
                changed: true,
                summary: format!("issued an introduction token for {name}"),
                pool: task.pool.unwrap_or_default().to_owned(),
                token: Some(token),
                ..Default::default()
            });
        }
        let Some(node) = nodes::find(&nomad, name).await? else {
            return Ok(unregistered(access.target, name));
        };
        let changed = match task.op {
            NodeOp::Undrain => nodes::undrain(&nomad, &node).await?,
            NodeOp::Drain => {
                nodes::drain(&nomad, &node, task.deadline.unwrap_or(DRAIN_DEADLINE)).await?
            }
            NodeOp::Status | NodeOp::Join => false,
        };
        let node = match changed {
            true => nodes::find(&nomad, name).await?.unwrap_or(node),
            false => node,
        };
        Ok(NodeReply {
            registered: true,
            eligible: node.eligible,
            draining: node.draining,
            changed,
            summary: summary(task.op, changed, name, access.target),
            pool: node.pool,
            token: None,
        })
    }

    async fn kube_node(
        &self,
        access: Access<'_>,
        task: NodeTask<'_>,
    ) -> buckle_core::Result<NodeReply> {
        use buckle_kube::nodes;
        let (kube, _) = self.kube_as(access, Purpose::Node, None).await?;
        let name = task.node;
        if task.op == NodeOp::Join {
            let id = format!("{:06x}", rand::random::<u32>() & 0x00ff_ffff);
            let secret = format!("{:016x}", rand::random::<u64>());
            let token =
                nodes::bootstrap_token(&kube, name, task.pool, JOIN_TTL, &id, &secret).await?;
            return Ok(NodeReply {
                changed: true,
                summary: format!("issued a bootstrap token for {name}"),
                pool: task.pool.unwrap_or_default().to_owned(),
                token: Some(token),
                ..Default::default()
            });
        }
        let Some(node) = nodes::find(&kube, name).await? else {
            return Ok(unregistered(access.target, name));
        };
        let mut note = String::new();
        let changed = match task.op {
            NodeOp::Undrain => nodes::uncordon(&kube, &node).await?,
            NodeOp::Drain => match nodes::drain(&kube, &node).await? {
                Some(d) => {
                    note = format!(
                        " ({} pods evicted, {} held by a disruption budget)",
                        d.evicted, d.held
                    );
                    true
                }
                None => false,
            },
            NodeOp::Status | NodeOp::Join => false,
        };
        let schedulable = match changed {
            true => task.op == NodeOp::Undrain,
            false => node.schedulable,
        };
        Ok(NodeReply {
            registered: true,
            eligible: schedulable,
            draining: false,
            changed,
            summary: summary(task.op, changed, name, access.target) + &note,
            pool: String::new(),
            token: None,
        })
    }
}

fn unregistered(target: &str, name: &str) -> NodeReply {
    NodeReply {
        summary: format!("{target} has no live node named {name}"),
        ..Default::default()
    }
}

fn summary(op: NodeOp, changed: bool, name: &str, target: &str) -> String {
    match (op, changed) {
        (NodeOp::Undrain, true) => format!("handed {name} back to {target}'s scheduler"),
        (NodeOp::Undrain, false) => format!("{name} is already eligible on {target}"),
        (NodeOp::Drain, true) => format!("started draining {name} on {target}"),
        (NodeOp::Drain, false) => format!("{name} is already out of scheduling on {target}"),
        (NodeOp::Status | NodeOp::Join, _) => format!("read {name} on {target}"),
    }
}
