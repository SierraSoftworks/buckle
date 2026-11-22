//! Node operations in flight: a host's `NodeOperation` call waits here for
//! the answer of the cluster's steward.
//!
//! The request is opened before the controller sends it, so an answer can
//! never arrive unannounced; it counts only from the steward the controller
//! sent it to. Nothing is stored: a join token passes through memory once.

use buckle_proto::v1::NodeOperationResult;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::sync::oneshot;

/// How a steward answered: the result, or why the operation failed.
pub type Outcome = Result<NodeOperationResult, String>;

#[derive(Default)]
struct Pending {
    /// The steward the request went to, once the controller sent it.
    steward: Option<String>,
    /// Answers that arrived before the steward was known, by sender.
    early: Vec<(String, Outcome)>,
    reply: Option<oneshot::Sender<Outcome>>,
}

/// The node operations waiting for a steward's answer, by request id.
#[derive(Default)]
pub struct NodeOps {
    pending: Mutex<HashMap<String, Pending>>,
}

impl std::fmt::Debug for NodeOps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeOps").finish_non_exhaustive()
    }
}

impl NodeOps {
    /// Opens the request `id`; the receiver resolves with its answer.
    pub fn open(&self, id: &str) -> oneshot::Receiver<Outcome> {
        let (reply, answer) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(
                id.to_owned(),
                Pending {
                    reply: Some(reply),
                    ..Default::default()
                },
            );
        }
        answer
    }

    /// The controller sent `id` to `steward`: only its answer counts.
    pub fn sent_to(&self, id: &str, steward: &str) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let Some(p) = pending.get_mut(id) else {
            return;
        };
        p.steward = Some(steward.to_owned());
        let early = std::mem::take(&mut p.early);
        if let Some((_, outcome)) = early.into_iter().find(|(from, _)| from == steward)
            && let Some(reply) = p.reply.take()
        {
            let _ = reply.send(outcome);
        }
    }

    /// `from` answered `id`. Ignored unless `from` is the steward asked.
    pub fn answer(&self, id: &str, from: &str, outcome: Outcome) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let Some(p) = pending.get_mut(id) else {
            return;
        };
        match &p.steward {
            None => p.early.push((from.to_owned(), outcome)),
            Some(steward) if steward == from => {
                if let Some(reply) = p.reply.take() {
                    let _ = reply.send(outcome);
                }
            }
            Some(_) => {}
        }
    }

    /// Forgets `id` (answered, refused or timed out).
    pub fn close(&self, id: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(id);
        }
    }
}

/// What the scheduler may call `host`'s node: the name to act on first, and
/// the host's other verified names. A scheduler names a node after the
/// machine's hostname, which on a tailnet is the short name (the host's id)
/// or the DNS name (`dns_name`, as the server's own tailscaled gave it).
/// `reported` is the hostname the host says it has: it picks among the
/// verified names and never adds one.
pub fn names(host: &str, dns_name: Option<&str>, reported: Option<&str>) -> (String, Vec<String>) {
    let mut verified = vec![host.to_owned()];
    if let Some(dns) = dns_name.map(|d| d.trim_end_matches('.'))
        && !dns.is_empty()
        && !dns.eq_ignore_ascii_case(host)
    {
        verified.push(dns.to_owned());
    }
    let first = reported
        .map(|r| r.trim_end_matches('.'))
        .and_then(|r| verified.iter().position(|v| v.eq_ignore_ascii_case(r)))
        .unwrap_or(0);
    let node = verified.remove(first);
    (node, verified)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn done(summary: &str) -> Outcome {
        Ok(NodeOperationResult {
            summary: summary.into(),
            ..Default::default()
        })
    }

    /// Only the steward the request was sent to can answer it, whichever
    /// of "sent" and "answered" the server sees first.
    #[rstest]
    #[case::answer_after_send(false)]
    #[case::answer_before_send(true)]
    #[tokio::test]
    async fn only_the_steward_asked_answers(#[case] early: bool) {
        let ops = NodeOps::default();
        let mut answer = ops.open("op-1");
        if !early {
            ops.sent_to("op-1", "pi");
        }
        ops.answer("op-1", "pi", done("undrained"));
        ops.answer("op-1", "impostor", done("forged"));
        if early {
            ops.sent_to("op-1", "pi");
        }
        assert_eq!(answer.try_recv().unwrap().unwrap().summary, "undrained");
    }

    #[tokio::test]
    async fn an_answer_nobody_waits_for_is_dropped() {
        let ops = NodeOps::default();
        let mut answer = ops.open("op-1");
        ops.close("op-1");
        ops.answer("op-1", "pi", done("late"));
        assert!(answer.try_recv().is_err());
    }

    #[rstest::rstest]
    #[case::short_hostname(Some("pi.tail.ts.net"), Some("pi"), "pi", &["pi.tail.ts.net"])]
    #[case::hostname_is_the_dns_name(Some("pi.tail.ts.net."), Some("pi.tail.ts.net"), "pi.tail.ts.net", &["pi"])]
    #[case::nothing_reported(Some("pi.tail.ts.net"), None, "pi", &["pi.tail.ts.net"])]
    #[case::a_reported_name_is_never_added(Some("pi.tail.ts.net"), Some("nas"), "pi", &["pi.tail.ts.net"])]
    #[case::off_the_tailnet(None, Some("pi.example.com"), "pi", &[])]
    fn a_node_is_named_by_what_the_server_verified(
        #[case] dns_name: Option<&str>,
        #[case] reported: Option<&str>,
        #[case] node: &str,
        #[case] aliases: &[&str],
    ) {
        let (first, rest) = names("pi", dns_name, reported);
        assert_eq!(first, node);
        assert_eq!(rest, aliases);
    }
}
