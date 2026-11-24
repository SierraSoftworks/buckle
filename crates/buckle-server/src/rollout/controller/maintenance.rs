//! Node maintenance: a host whose run disrupts its node of a cluster with
//! `maintenance.drain` has the node taken out of scheduling by the
//! cluster's steward before the Apply goes out, and handed back once the
//! host's checks pass, before it counts as healthy.
//!
//! The steward is asked as for a host's own `cluster.node` task
//! (`RunNodeOperation`), with ids starting [`NODE_OP_PREFIX`] so that the
//! RPC layer hands their answers here. A row that drains:
//!
//! 1. enters [`HostPhase::Draining`]: which clusters it is a node of, and
//!    whether its run disrupts them, is worked out from its host's facts
//!    and the run's revision ([`buckle_core::engine::maintenance`]);
//! 2. reads each node first: one already out of scheduling, or unknown to
//!    the scheduler, is left as it is;
//! 3. drains the rest and polls them until the scheduler has moved their
//!    work, then sends the Apply. A drain that fails or does not finish
//!    fails the row before anything is applied, and hands back what it
//!    took;
//! 4. after its bake, enters [`HostPhase::Undraining`] and becomes healthy
//!    once the nodes are back.
//!
//! A row that fails (or is skipped) after its Apply went out leaves its
//! nodes drained: they are handed back when a retry is healthy, or after
//! a rollback. Nothing applied (a skip mid-drain, a pause): handed back.

use super::super::model::{NodeHold, NodeOpKind, NodeOpWait};
use super::*;
use buckle_api::RolloutNodeState as NodeState;

/// What the ids of the node operations a rollout sends start with: their
/// answers come back to the controller, never to a waiting `NodeOperation`
/// call.
pub const NODE_OP_PREFIX: &str = "rollout-node:";

/// How long the steward has to answer one operation before it is asked
/// again (the RPC's own wait).
const ANSWER_TIMEOUT: chrono::Duration = chrono::Duration::seconds(45);
/// How often a drain under way is read.
const DRAIN_POLL: chrono::Duration = chrono::Duration::seconds(5);
/// How soon an operation no steward could take is tried again.
const RETRY: chrono::Duration = chrono::Duration::seconds(10);
/// How long past its deadline a drain may take to report itself done.
const DRAIN_GRACE: chrono::Duration = chrono::Duration::minutes(2);
/// How long a node is given to move its work when the cluster sets no
/// `deadline` (the steward's default).
const DEFAULT_DEADLINE: Duration = Duration::from_secs(60 * 60);
/// How long handing a node back may take, stewards coming and going.
const UNDRAIN_TIMEOUT: chrono::Duration = chrono::Duration::minutes(5);

impl Controller {
    /// Whether row `hi`'s Apply may have to wait for a drain: a host whose
    /// run changes something, in a rollout whose revision declares a
    /// cluster that drains its nodes.
    fn may_drain(&self, ri: usize, hi: usize) -> bool {
        let live = &self.live[ri];
        live.hosts[hi].runtime.kind == TargetKind::Host
            && !self.unchanged(ri, hi)
            && live
                .revision
                .fleet
                .fleet
                .clusters
                .values()
                .any(|c| c.drains().is_some())
    }

    /// Holds row `hi`'s Apply while its nodes drain. `true` when it does:
    /// the Apply goes out once they are out of scheduling.
    pub(super) fn hold_for_drain(&mut self, ri: usize, hi: usize) -> bool {
        let h = &mut self.live[ri].hosts[hi];
        if std::mem::take(&mut h.runtime.drain_ready) {
            return false;
        }
        if !self.may_drain(ri, hi) {
            return false;
        }
        self.live[ri].hosts[hi].runtime.node_check = true;
        self.touch_host(ri, hi);
        self.set_host_state(
            ri,
            hi,
            HostPhase::Draining,
            "checking whether its run disrupts a node it runs",
        );
        true
    }

    /// Holds row `hi`'s healthy state (or its next step) while the nodes
    /// the rollout drained are handed back. `true` when it does.
    pub(super) fn hold_for_undrain(&mut self, ri: usize, hi: usize) -> bool {
        if self.live[ri].hosts[hi].state == HostPhase::Undraining {
            return false;
        }
        let clusters = self.hand_back(ri, hi, "handing the node back to");
        if clusters.is_empty() {
            return false;
        }
        self.set_host_state(
            ri,
            hi,
            HostPhase::Undraining,
            &format!("baked; handing its node back to {}", clusters.join(", ")),
        );
        true
    }

    /// Starts handing back every node row `hi` holds out of scheduling;
    /// returns their clusters.
    fn hand_back(&mut self, ri: usize, hi: usize, what: &str) -> Vec<String> {
        let now = self.now();
        let mut clusters = Vec::new();
        for n in self.live[ri].hosts[hi].runtime.nodes.iter_mut() {
            if !n.held() {
                continue;
            }
            n.state = NodeState::Undraining;
            n.since = now;
            n.until = Some(now + UNDRAIN_TIMEOUT);
            n.next_at = Some(now);
            n.op = None;
            n.started = false;
            n.summary = Some(format!("{what} {}", n.cluster));
            clusters.push(n.cluster.clone());
        }
        if !clusters.is_empty() {
            self.touch_host(ri, hi);
        }
        clusters
    }

    /// Row `hi` moved to `state`: a row that failed or was skipped after
    /// its Apply went out leaves its nodes drained (they are handed back
    /// once a retry is healthy); one that applied nothing, or that rolled
    /// back, hands them back now.
    pub(super) fn on_row_state(&mut self, ri: usize, hi: usize, state: HostPhase) {
        if self.live[ri].hosts[hi].runtime.nodes.is_empty() {
            return;
        }
        let applied = self.live[ri].hosts[hi].runtime.applied;
        match state {
            HostPhase::RolledBack => {
                self.hand_back(ri, hi, "rolled back; handing the node back to");
            }
            HostPhase::Failed | HostPhase::Skipped | HostPhase::Missed if !applied => {
                self.hand_back(ri, hi, "nothing was applied; handing the node back to");
                self.forget_unheld(ri, hi);
            }
            HostPhase::Failed | HostPhase::Skipped | HostPhase::Missed => {
                self.leave_drained(ri, hi, state);
            }
            _ => {}
        }
    }

    /// A node still being read before its drain is the rollout's no more.
    fn forget_unheld(&mut self, ri: usize, hi: usize) {
        for n in self.live[ri].hosts[hi].runtime.nodes.iter_mut() {
            if n.state == NodeState::Draining && !n.ours {
                n.state = NodeState::Unmanaged;
                n.op = None;
                n.next_at = None;
                n.until = None;
                n.summary = Some("the host left the rollout before its node was drained".into());
            }
        }
    }

    fn leave_drained(&mut self, ri: usize, hi: usize, state: HostPhase) {
        let now = self.now();
        let mut left = Vec::new();
        self.forget_unheld(ri, hi);
        for n in self.live[ri].hosts[hi].runtime.nodes.iter_mut() {
            if !(n.ours && matches!(n.state, NodeState::Draining | NodeState::Drained)) {
                continue;
            }
            n.state = NodeState::LeftDrained;
            n.since = now;
            n.until = None;
            n.next_at = None;
            n.op = None;
            n.summary = Some(format!(
                "stays out of scheduling on {} until the host is healthy again",
                n.cluster
            ));
            left.push(n.cluster.clone());
        }
        for cluster in left {
            self.node_audit(
                ri,
                hi,
                "rollout.host.node_left_drained",
                &cluster,
                format!(
                    "the host is {}; its node stays out of scheduling on {cluster} until a retry is healthy (or hand it back on the scheduler)",
                    state.as_str()
                ),
            );
        }
        self.touch_host(ri, hi);
    }

    /// Works on every row's nodes: finds which ones a draining row drains,
    /// asks the stewards what is due, and lets a row go on once its nodes
    /// are where its next step needs them. Returns whether a row moved.
    pub(super) async fn drive_maintenance(&mut self) -> bool {
        let mut moved = false;
        for ri in 0..self.live.len() {
            for hi in 0..self.live[ri].hosts.len() {
                let h = &self.live[ri].hosts[hi];
                if h.state == HostPhase::Draining && h.runtime.node_check {
                    self.find_nodes(ri, hi).await;
                }
                let now = self.now();
                let due: Vec<usize> = (0..self.live[ri].hosts[hi].runtime.nodes.len())
                    .filter(|&ni| {
                        let n = &self.live[ri].hosts[hi].runtime.nodes[ni];
                        n.op.is_none() && n.next_at.is_some_and(|t| t <= now)
                    })
                    .collect();
                for ni in due {
                    self.ask_steward(ri, hi, ni).await;
                }
                moved |= self.settle_nodes(ri, hi);
            }
        }
        moved
    }

    /// Which nodes row `hi`'s run drains: one per cluster with
    /// `maintenance` whose `nodes` the host matches, when the cluster's
    /// `drain` takes this run (see [`buckle_core::model::DrainPolicy`]).
    async fn find_nodes(&mut self, ri: usize, hi: usize) {
        self.live[ri].hosts[hi].runtime.node_check = false;
        self.touch_host(ri, hi);
        let (host, revision_id, plan) = {
            let h = &self.live[ri].hosts[hi];
            (h.host.clone(), h.runtime.revision.clone(), h.plan.clone())
        };
        let action = self.live[ri].record.runtime.action.clone();
        let revision = match self.revisions.load(&revision_id).await {
            Ok(r) => r,
            Err(e) => {
                let why = format!(
                    "could not read revision {revision_id} to tell whether its run disrupts a node: {e}; nothing was applied"
                );
                self.fail_host(ri, hi, &why);
                return;
            }
        };
        let h = host.clone();
        let found = self
            .store
            .call(move |db| {
                Ok((
                    db.hosts().get(&h)?,
                    db.hosts().latest_facts(&h)?.map(|(f, _)| f),
                ))
            })
            .await;
        let (record, facts) = match found {
            Ok((Some(record), facts)) => (record, facts.unwrap_or(serde_json::Value::Null)),
            Ok((None, _)) => return,
            Err(e) => {
                let why = format!(
                    "could not read the host to tell whether its run disrupts a node: {e}; nothing was applied"
                );
                self.fail_host(ri, hi, &why);
                return;
            }
        };
        let ctx = crate::fleet::record_context(&record, &facts, &revision.fleet);
        let reasons = match &action {
            Some(a) => buckle_core::engine::maintenance::action_disruptions(
                &revision, &a.module, &a.action,
            ),
            None => plan
                .and_then(|p| serde_json::from_value::<buckle_core::plan::Plan>(p).ok())
                .map(|p| buckle_core::engine::maintenance::plan_disruptions(&p, &revision))
                .unwrap_or_default(),
        };
        let changes = !self.unchanged(ri, hi);
        let (node, aliases) = crate::node_ops::names(
            &host,
            self.dns_names.get(&host).map(String::as_str),
            facts["net"]["hostname"].as_str(),
        );
        let now = self.now();
        for (name, decl) in &revision.fleet.fleet.clusters {
            let Some(maintenance) = decl.drains() else {
                continue;
            };
            let is_node = crate::fleet::cluster_trust(&revision.fleet, None, name)
                .is_some_and(|t| t.node(&ctx).is_ok());
            if !is_node || !maintenance.drain.drains(changes, &reasons) {
                continue;
            }
            let reasons = match (maintenance.drain, reasons.is_empty()) {
                (buckle_core::model::DrainPolicy::Always, true) => {
                    vec!["the run changes the host (`maintenance.drain: always`)".to_owned()]
                }
                _ => reasons.clone(),
            };
            let deadline = maintenance.deadline.unwrap_or(DEFAULT_DEADLINE);
            let nodes = &mut self.live[ri].hosts[hi].runtime.nodes;
            let earlier = nodes.iter().position(|n| &n.cluster == name);
            // A node an earlier attempt drained (or is handing back) is
            // still the rollout's: it is drained again without reading it.
            let ours = earlier.is_some_and(|i| nodes[i].ours);
            let hold = NodeHold {
                cluster: name.clone(),
                state: NodeState::Draining,
                reasons,
                ours,
                node: node.clone(),
                aliases: aliases.clone(),
                revision: revision_id.clone(),
                deadline: maintenance.deadline.map(|d| d.as_secs()),
                since: now,
                until: Some(
                    now + chrono::Duration::from_std(deadline).unwrap_or_default() + DRAIN_GRACE,
                ),
                next_at: Some(now),
                started: false,
                op: None,
                summary: Some(format!("asking {name}'s steward to drain it")),
            };
            match earlier {
                Some(i) => nodes[i] = hold,
                None => nodes.push(hold),
            }
        }
        let draining: Vec<String> = self.live[ri].hosts[hi]
            .runtime
            .nodes
            .iter()
            .filter(|n| n.state == NodeState::Draining)
            .map(|n| n.cluster.clone())
            .collect();
        if !draining.is_empty() {
            let key = self.row_key(ri, hi);
            tracing::info!(buckle.host = %key, clusters = ?draining, "the run disrupts the host's node: draining it first");
        }
    }

    /// Sends node `ni` of row `hi` the operation it is due.
    async fn ask_steward(&mut self, ri: usize, hi: usize, ni: usize) {
        let now = self.now();
        let n = self.live[ri].hosts[hi].runtime.nodes[ni].clone();
        let op = match n.state {
            // Read first: a node already out of scheduling is not the
            // rollout's to hand back.
            NodeState::Draining if !n.ours || n.started => NodeOpKind::Status,
            NodeState::Draining => NodeOpKind::Drain,
            NodeState::Undraining => NodeOpKind::Undrain,
            _ => {
                self.live[ri].hosts[hi].runtime.nodes[ni].next_at = None;
                return;
            }
        };
        let pb_op = match op {
            NodeOpKind::Drain => pb::NodeOp::Drain,
            NodeOpKind::Status => pb::NodeOp::Status,
            NodeOpKind::Undrain => pb::NodeOp::Undrain,
        };
        let id = format!("{NODE_OP_PREFIX}{}", new_id());
        let deadline = n
            .deadline
            .filter(|_| op == NodeOpKind::Drain)
            .map(Duration::from_secs);
        let sent = self
            .node_operation(
                n.cluster.clone(),
                id.clone(),
                pb_op,
                n.node.clone(),
                n.aliases.clone(),
                String::new(),
                n.revision.clone(),
                deadline,
            )
            .await;
        match sent {
            Ok(steward) => {
                let hold = &mut self.live[ri].hosts[hi].runtime.nodes[ni];
                hold.op = Some(NodeOpWait {
                    id,
                    steward,
                    op,
                    sent_at: now,
                });
                hold.next_at = None;
            }
            // No steward (yet), or one that cannot act on nodes: asked
            // again until the wait gives up.
            Err(e) if e.kind == CommandErrorKind::Conflict => {
                let hold = &mut self.live[ri].hosts[hi].runtime.nodes[ni];
                hold.summary = Some(format!("{} Asking again.", e.error.description()));
                hold.next_at = Some(now + RETRY);
            }
            Err(e) => {
                let why = e.error.description().to_string();
                self.node_op_failed(ri, hi, ni, op, &why);
            }
        }
        self.touch_host(ri, hi);
    }

    /// A steward's answer to an operation a rollout sent. Only the steward
    /// it was sent to is heard.
    pub(super) fn on_node_operation(&mut self, from: &str, done: pb::NodeOperationDone) {
        if !done.op_id.starts_with(NODE_OP_PREFIX) {
            return;
        }
        let found = self.live.iter().enumerate().find_map(|(ri, l)| {
            l.hosts.iter().enumerate().find_map(|(hi, h)| {
                h.runtime
                    .nodes
                    .iter()
                    .position(|n| {
                        n.op.as_ref()
                            .is_some_and(|w| w.id == done.op_id && w.steward == from)
                    })
                    .map(|ni| (ri, hi, ni))
            })
        });
        let Some((ri, hi, ni)) = found else {
            tracing::debug!(from = %from, op = %done.op_id, "an answer to a node operation nobody waits for");
            return;
        };
        let Some(wait) = self.live[ri].hosts[hi].runtime.nodes[ni].op.take() else {
            return;
        };
        self.touch_host(ri, hi);
        match done.result {
            Some(result) if done.error.is_empty() => match wait.op {
                NodeOpKind::Drain | NodeOpKind::Status => {
                    self.on_drain_answer(ri, hi, ni, wait.op, &result)
                }
                NodeOpKind::Undrain => self.on_undrain_answer(ri, hi, ni, &result),
            },
            _ => {
                let why = if done.error.is_empty() {
                    format!("{from} answered without a result")
                } else {
                    format!("{from}: {}", done.error)
                };
                self.node_op_failed(ri, hi, ni, wait.op, &why);
            }
        }
        self.settle_nodes(ri, hi);
    }

    fn on_drain_answer(
        &mut self,
        ri: usize,
        hi: usize,
        ni: usize,
        op: NodeOpKind,
        r: &pb::NodeOperationResult,
    ) {
        let now = self.now();
        let gating = self.live[ri].hosts[hi].state == HostPhase::Draining;
        let hold = &mut self.live[ri].hosts[hi].runtime.nodes[ni];
        if hold.state != NodeState::Draining {
            return;
        }
        let cluster = hold.cluster.clone();
        hold.summary = (!r.summary.is_empty()).then(|| r.summary.clone());
        let unmanaged = |hold: &mut NodeHold, why: String| {
            hold.state = NodeState::Unmanaged;
            hold.ours = false;
            hold.since = now;
            hold.until = None;
            hold.next_at = None;
            hold.summary = Some(why);
        };
        if !hold.ours {
            // The read before the drain.
            let why = if !r.registered {
                Some(format!(
                    "{cluster} has no live node named {}: nothing to drain",
                    hold.node
                ))
            } else if !r.eligible || r.draining {
                Some(format!(
                    "{} was out of scheduling on {cluster} before the run; the rollout leaves it as it is",
                    hold.node
                ))
            } else {
                None
            };
            match why {
                Some(why) => {
                    unmanaged(hold, why.clone());
                    self.node_audit(ri, hi, "rollout.host.drain_skipped", &cluster, why);
                }
                None if gating => {
                    hold.ours = true;
                    hold.next_at = Some(now);
                }
                // The row moved on while its node was read.
                None => unmanaged(
                    hold,
                    "the host left the rollout before its node was drained".into(),
                ),
            }
            return;
        }
        if op == NodeOpKind::Drain {
            hold.started = true;
        }
        if r.draining && gating {
            hold.next_at = Some(now + DRAIN_POLL);
            return;
        }
        hold.state = NodeState::Drained;
        hold.since = now;
        hold.until = None;
        hold.next_at = None;
        let reasons = hold.reasons.join("; ");
        let message = match r.registered {
            true => {
                format!("its node is out of scheduling on {cluster} before the run ({reasons})")
            }
            false => format!(
                "{cluster} no longer has a live node named {}; nothing is left to drain",
                hold.node
            ),
        };
        self.node_audit(ri, hi, "rollout.host.drained", &cluster, message);
        if !gating {
            // Drained after the row moved on: it waits for a retry.
            let state = self.live[ri].hosts[hi].state;
            if matches!(
                state,
                HostPhase::Failed | HostPhase::Skipped | HostPhase::Missed
            ) {
                self.on_row_state(ri, hi, state);
            }
        }
    }

    fn on_undrain_answer(&mut self, ri: usize, hi: usize, ni: usize, r: &pb::NodeOperationResult) {
        let now = self.now();
        let hold = &mut self.live[ri].hosts[hi].runtime.nodes[ni];
        if hold.state != NodeState::Undraining {
            return;
        }
        let cluster = hold.cluster.clone();
        hold.state = if r.registered {
            NodeState::Undrained
        } else {
            NodeState::Unmanaged
        };
        hold.ours = false;
        hold.since = now;
        hold.until = None;
        hold.next_at = None;
        hold.summary = (!r.summary.is_empty()).then(|| r.summary.clone());
        let message = match r.registered {
            true => format!("its node is back in scheduling on {cluster}"),
            false => format!(
                "{cluster} has no live node named {} to hand back",
                hold.node
            ),
        };
        self.node_audit(ri, hi, "rollout.host.undrained", &cluster, message);
    }

    /// An operation on node `ni` of row `hi` failed (`why`). A drain fails
    /// the row before anything was applied, and hands back what it took; an
    /// undrain leaves the node drained, and fails a row waiting for it.
    fn node_op_failed(&mut self, ri: usize, hi: usize, ni: usize, op: NodeOpKind, why: &str) {
        let now = self.now();
        let state = self.live[ri].hosts[hi].state;
        let cluster = self.live[ri].hosts[hi].runtime.nodes[ni].cluster.clone();
        match op {
            NodeOpKind::Drain | NodeOpKind::Status => {
                self.live[ri].hosts[hi].runtime.nodes[ni].summary = Some(why.to_owned());
                if state != HostPhase::Draining {
                    return;
                }
                let error =
                    format!("could not drain its node on {cluster} ({why}); nothing was applied");
                self.live[ri].hosts[hi].runtime.error = Some(error.clone());
                // Nothing was applied: a node that left scheduling goes back.
                self.fail_host(ri, hi, &error);
            }
            NodeOpKind::Undrain => {
                let hold = &mut self.live[ri].hosts[hi].runtime.nodes[ni];
                hold.state = NodeState::LeftDrained;
                hold.ours = true;
                hold.since = now;
                hold.until = None;
                hold.next_at = None;
                hold.summary = Some(why.to_owned());
                self.node_audit(
                    ri,
                    hi,
                    "rollout.host.node_left_drained",
                    &cluster,
                    format!("its node could not be handed back to {cluster}: {why}"),
                );
                if state == HostPhase::Undraining {
                    let error = format!(
                        "applied and healthy, but its node could not be handed back to {cluster} ({why}); retry the host to hand it back"
                    );
                    self.live[ri].hosts[hi].runtime.error = Some(error.clone());
                    self.fail_host(ri, hi, &error);
                }
            }
        }
    }

    /// Lets row `hi` go on once its nodes are where it needs them: the
    /// Apply once every node it drains is out of scheduling, the healthy
    /// state (or next step) once every node is back. Returns whether the
    /// row moved.
    fn settle_nodes(&mut self, ri: usize, hi: usize) -> bool {
        let h = &self.live[ri].hosts[hi];
        let busy = |s: NodeState| h.runtime.nodes.iter().any(|n| n.state == s);
        match h.state {
            HostPhase::Draining if !h.runtime.node_check && !busy(NodeState::Draining) => {
                if !self.may_apply(ri, hi) {
                    // Paused (or aborted) while it drained: nothing is
                    // applied, so the node goes back until the row starts
                    // again.
                    self.hand_back(
                        ri,
                        hi,
                        "the rollout is not running; handing the node back to",
                    );
                    self.defer_host(
                        ri,
                        hi,
                        "the rollout is not running; the apply waits for it to resume",
                    );
                    return true;
                }
                self.live[ri].hosts[hi].runtime.drain_ready = true;
                self.send_apply(ri, hi);
                true
            }
            HostPhase::Undraining if !busy(NodeState::Undraining) => {
                self.finish_step(ri, hi);
                true
            }
            _ => false,
        }
    }

    /// Gives up on operations past their time: an unanswered one is asked
    /// again, and a drain or undrain past its `until` fails.
    pub(super) fn node_timeouts(&mut self, ri: usize, hi: usize) {
        let now = self.now();
        for ni in 0..self.live[ri].hosts[hi].runtime.nodes.len() {
            let n = &self.live[ri].hosts[hi].runtime.nodes[ni];
            let busy = matches!(n.state, NodeState::Draining | NodeState::Undraining);
            if busy && n.until.is_some_and(|u| u <= now) {
                let (op, why) = match n.state {
                    NodeState::Draining => (
                        NodeOpKind::Drain,
                        format!(
                            "the scheduler had not moved its work within {}",
                            humantime_secs(n.deadline.unwrap_or(DEFAULT_DEADLINE.as_secs()))
                        ),
                    ),
                    _ => (
                        NodeOpKind::Undrain,
                        format!(
                            "no steward of {} handed it back within {}",
                            n.cluster,
                            humantime_secs(UNDRAIN_TIMEOUT.num_seconds().max(0) as u64)
                        ),
                    ),
                };
                self.live[ri].hosts[hi].runtime.nodes[ni].op = None;
                self.node_op_failed(ri, hi, ni, op, &why);
                self.touch_host(ri, hi);
                continue;
            }
            if let Some(wait) = &n.op
                && wait.sent_at + ANSWER_TIMEOUT <= now
            {
                let summary = format!("{} did not answer; asking again", wait.steward);
                let hold = &mut self.live[ri].hosts[hi].runtime.nodes[ni];
                hold.op = None;
                hold.next_at = Some(now);
                hold.summary = Some(summary);
                self.touch_host(ri, hi);
            }
        }
    }

    /// The times row `h`'s nodes wait for.
    pub(super) fn node_deadlines(h: &RolloutHostRecord, mut consider: impl FnMut(DateTime<Utc>)) {
        for n in &h.runtime.nodes {
            if let Some(t) = n.next_at {
                consider(t);
            }
            if let Some(w) = &n.op {
                consider(w.sent_at + ANSWER_TIMEOUT);
            }
            if matches!(n.state, NodeState::Draining | NodeState::Undraining)
                && let Some(u) = n.until
            {
                consider(u);
            }
        }
    }

    /// Whether a finished rollout still hands a node back (it stays live
    /// until it has).
    pub(super) fn handing_back(l: &Live) -> bool {
        l.hosts.iter().any(|h| {
            h.runtime
                .nodes
                .iter()
                .any(|n| n.state == NodeState::Undraining)
        })
    }

    fn node_audit(&mut self, ri: usize, hi: usize, action: &str, cluster: &str, message: String) {
        let (key, host, workload, phase, id, trace) = {
            let live = &self.live[ri];
            let h = &live.hosts[hi];
            (
                view::row_key(h),
                h.host.clone(),
                h.workload.clone(),
                h.phase.clone(),
                live.record.id.clone(),
                live.record.runtime.trace_id.clone(),
            )
        };
        let node = self.live[ri].hosts[hi]
            .runtime
            .nodes
            .iter()
            .find(|n| n.cluster == cluster)
            .map(|n| (n.node.clone(), n.summary.clone(), n.reasons.clone()));
        let (node, summary, reasons) = node.unwrap_or_default();
        tracing::info!(buckle.rollout = %id, buckle.host = %key, buckle.target = %cluster, action, "{message}");
        self.audit(
            "controller",
            action,
            &id,
            format!("{key}: {message}"),
            serde_json::json!({
                "host": host, "workload": workload, "phase": phase, "cluster": cluster,
                "node": node, "summary": summary, "reasons": reasons,
            }),
            trace,
        );
    }
}

fn humantime_secs(secs: u64) -> String {
    humantime_serde::re::humantime::format_duration(Duration::from_secs(secs)).to_string()
}
