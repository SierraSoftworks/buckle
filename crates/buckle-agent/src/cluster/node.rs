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
    /// The node's name: one the server verified for the host that asked.
    pub node: &'a str,
    /// That host's other verified names; its node carries any of them.
    pub aliases: &'a [String],
    /// `join`: the pool the server picked; `None`: the default pool.
    pub pool: Option<&'a str>,
    pub deadline: Option<Duration>,
}

impl<'a> NodeTask<'a> {
    /// Every name the host's node may carry, the server's choice first.
    fn names(&self) -> Vec<&'a str> {
        let mut names = vec![self.node];
        for alias in self.aliases {
            if !names.contains(&alias.as_str()) {
                names.push(alias);
            }
        }
        names
    }
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
        let names = task.names();
        let Some(node) = nodes::find(&nomad, &names).await? else {
            return Ok(unregistered(access.target, &names));
        };
        let changed = match task.op {
            NodeOp::Undrain => nodes::undrain(&nomad, &node).await?,
            NodeOp::Drain => {
                nodes::drain(&nomad, &node, task.deadline.unwrap_or(DRAIN_DEADLINE)).await?
            }
            NodeOp::Status | NodeOp::Join => false,
        };
        let node = match changed {
            true => nodes::find(&nomad, &[node.name.as_str()])
                .await?
                .unwrap_or(node),
            false => node,
        };
        Ok(NodeReply {
            registered: true,
            eligible: node.eligible,
            draining: node.draining,
            changed,
            summary: summary(task.op, changed, &node.name, access.target),
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
        let names = task.names();
        let mut found = None;
        for candidate in &names {
            if let Some(node) = nodes::find(&kube, candidate).await? {
                found = Some(node);
                break;
            }
        }
        let Some(node) = found else {
            return Ok(unregistered(access.target, &names));
        };
        let name = node.name.as_str();
        // A drain Buckle has under way goes on whenever the node is read,
        // as Nomad's drainer carries on its own (`nodes::proceed`).
        let (changed, drained) = match task.op {
            NodeOp::Undrain => (nodes::uncordon(&kube, &node).await?, None),
            NodeOp::Drain => {
                let deadline = task.deadline.unwrap_or(DRAIN_DEADLINE);
                match nodes::drain(&kube, &node, deadline).await? {
                    Some(d) => (true, Some(d)),
                    None => (false, nodes::proceed(&kube, &node).await?),
                }
            }
            NodeOp::Status => (false, nodes::proceed(&kube, &node).await?),
            NodeOp::Join => (false, None),
        };
        let schedulable = match changed {
            true => task.op == NodeOp::Undrain,
            false => node.schedulable,
        };
        let summary = match (drained, changed) {
            (Some(d), false) => format!("draining {name} on {}{}", access.target, progress(&d)),
            (Some(d), true) => summary(task.op, changed, name, access.target) + &progress(&d),
            (None, _) => summary(task.op, changed, name, access.target),
        };
        Ok(NodeReply {
            registered: true,
            eligible: schedulable,
            draining: drained.is_some_and(|d| !d.done()),
            changed,
            summary,
            pool: String::new(),
            token: None,
        })
    }
}

/// How a Kubernetes drain stands, for the reply's summary.
fn progress(d: &buckle_kube::nodes::Drained) -> String {
    let mut parts = Vec::new();
    if d.evicted > 0 {
        parts.push(format!("{} evicted", pods(d.evicted)));
    }
    if d.deleted > 0 {
        parts.push(format!("{} deleted past the deadline", pods(d.deleted)));
    }
    if d.held > 0 {
        parts.push(format!("{} held by a disruption budget", pods(d.held)));
    }
    parts.push(match d.remaining {
        0 => "nothing left to move".to_owned(),
        n => format!("{} still on the node", pods(n)),
    });
    format!(" ({})", parts.join(", "))
}

fn pods(n: usize) -> String {
    match n {
        1 => "1 pod".to_owned(),
        n => format!("{n} pods"),
    }
}

fn unregistered(target: &str, names: &[&str]) -> NodeReply {
    NodeReply {
        summary: format!("{target} has no live node named {}", names.join(" or ")),
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
