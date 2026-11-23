//! The local protocol's messages to and from the types either end works
//! with. Structured documents travel as JSON, as they do to the server.

use super::{Choice, Event, Request, Status};
use crate::transport::convert::error_detail_to_proto;
use buckle_core::error::{ErrorKind, ErrorReport};
use buckle_core::plan::RevisionId;
use buckle_proto::v1 as pb;

fn json<T: serde::Serialize>(value: &T) -> Vec<u8> {
    serde_json::to_vec(value).unwrap_or_default()
}

fn parse<T: serde::de::DeserializeOwned>(what: &str, bytes: &[u8]) -> buckle_core::Result<T> {
    serde_json::from_slice(bytes).map_err(|e| {
        buckle_core::error::wrap_system(
            e,
            format!("The agent sent {what} this buckle cannot read."),
            &["Run the same Buckle version as the agent: `buckle --version`."],
        )
    })
}

fn optional<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    (!bytes.is_empty())
        .then(|| serde_json::from_slice(bytes).ok())
        .flatten()
}

fn error_detail_from_proto(detail: &pb::ErrorDetail) -> ErrorReport {
    ErrorReport {
        kind: if detail.kind == "system" {
            ErrorKind::System
        } else {
            ErrorKind::User
        },
        message: detail.message.clone(),
        causes: detail.causes.clone(),
        advice: detail.advice.clone(),
    }
}

fn revision(id: &str) -> Option<RevisionId> {
    (!id.is_empty()).then(|| RevisionId::new(id))
}

fn id(revision: &Option<RevisionId>) -> String {
    revision.as_ref().map(|r| r.0.clone()).unwrap_or_default()
}

impl From<Choice> for pb::LocalRevision {
    fn from(choice: Choice) -> Self {
        match choice {
            Choice::Applied => pb::LocalRevision::Applied,
            Choice::Assigned => pb::LocalRevision::Assigned,
        }
    }
}

fn choice(raw: i32) -> Choice {
    match pb::LocalRevision::try_from(raw) {
        Ok(pb::LocalRevision::Assigned) => Choice::Assigned,
        _ => Choice::Applied,
    }
}

pub(super) fn plan_request(r: pb::LocalPlanRequest) -> Request {
    Request::Plan {
        revision: choice(r.revision),
        select: Some(r.select).filter(|s| !s.is_empty()),
    }
}

pub(super) fn health_request(r: pb::LocalHealthRequest) -> Request {
    Request::Health {
        revision: choice(r.revision),
        modules: r.modules,
    }
}

pub(super) fn event_to_proto(event: &Event) -> pb::LocalEvent {
    use pb::local_event::Event as E;
    let event = match event {
        Event::Queued { ahead } => E::Queued(pb::LocalQueued {
            ahead: ahead.clone(),
        }),
        Event::Started {
            run_id,
            revision,
            applied,
            assigned,
        } => E::Started(pb::LocalStarted {
            run_id: run_id.clone(),
            revision: revision.0.clone(),
            applied_revision: id(applied),
            assigned_revision: id(assigned),
        }),
        Event::Plan { plan, facts } => E::Plan(pb::LocalPlan {
            plan_json: json(plan),
            facts_json: json(facts),
        }),
        Event::Health(report) => E::Health(pb::LocalHealth {
            report_json: json(report),
        }),
        Event::Done { exit_code, error } => E::Done(pb::LocalDone {
            exit_code: *exit_code,
            error: error.as_ref().map(error_detail_to_proto),
        }),
    };
    pb::LocalEvent { event: Some(event) }
}

/// `None` for an event this buckle does not know (a newer agent's).
pub(super) fn event_from_proto(event: pb::LocalEvent) -> buckle_core::Result<Option<Event>> {
    use pb::local_event::Event as E;
    Ok(Some(match event.event {
        None => return Ok(None),
        Some(E::Queued(q)) => Event::Queued { ahead: q.ahead },
        Some(E::Started(s)) => Event::Started {
            run_id: s.run_id,
            revision: RevisionId::new(s.revision),
            applied: revision(&s.applied_revision),
            assigned: revision(&s.assigned_revision),
        },
        Some(E::Plan(p)) => Event::Plan {
            plan: Box::new(parse("a plan", &p.plan_json)?),
            facts: optional(&p.facts_json).unwrap_or_default(),
        },
        Some(E::Health(h)) => Event::Health(parse("a health report", &h.report_json)?),
        Some(E::Done(d)) => Event::Done {
            exit_code: d.exit_code,
            error: d.error.as_ref().map(error_detail_from_proto),
        },
    }))
}

pub(super) fn status_to_proto(status: &Status) -> pb::LocalStatus {
    pb::LocalStatus {
        host_id: status.host_id.clone(),
        agent_version: status.agent_version.clone(),
        server: status.server.clone().unwrap_or_default(),
        connected: status.connected,
        host_status: status.host_status.clone().unwrap_or_default(),
        applied_revision: id(&status.applied),
        assigned_revision: id(&status.assigned),
        stable_revision: id(&status.stable),
        running: status.running.clone().unwrap_or_default(),
        queued: status.queued.clone(),
        paused_modules: status.paused_modules.clone(),
        rolled_back_modules: status.rolled_back_modules.clone(),
        run_json: status.run.as_ref().map(json).unwrap_or_default(),
        outcome_json: status.outcome.as_ref().map(json).unwrap_or_default(),
    }
}

pub(super) fn status_from_proto(status: pb::LocalStatus) -> Status {
    let text = |s: String| Some(s).filter(|s| !s.is_empty());
    Status {
        host_id: status.host_id,
        agent_version: status.agent_version,
        server: text(status.server),
        connected: status.connected,
        host_status: text(status.host_status),
        applied: revision(&status.applied_revision),
        assigned: revision(&status.assigned_revision),
        stable: revision(&status.stable_revision),
        running: text(status.running),
        queued: status.queued,
        paused_modules: status.paused_modules,
        rolled_back_modules: status.rolled_back_modules,
        run: optional(&status.run_json),
        outcome: optional(&status.outcome_json),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::plan_with_changes;

    #[test]
    fn a_run_s_events_cross_the_socket_whole() {
        let plan = plan_with_changes(&RevisionId::new("rev1"), 2);
        let events = [
            Event::Queued {
                ahead: vec!["apply rev1".into()],
            },
            Event::Started {
                run_id: "run-1".into(),
                revision: RevisionId::new("rev1"),
                applied: Some(RevisionId::new("rev1")),
                assigned: None,
            },
            Event::Plan {
                plan: Box::new(plan.clone()),
                facts: serde_json::json!({"os": {"family": "debian"}}),
            },
            Event::Done {
                exit_code: 2,
                error: Some(
                    buckle_core::error::ErrorReport::user("refused").with_advice(["Try later."]),
                ),
            },
        ];
        let back: Vec<Event> = events
            .iter()
            .map(|e| {
                event_from_proto(event_to_proto(e))
                    .expect("parses")
                    .expect("known")
            })
            .collect();
        assert!(matches!(&back[0], Event::Queued { ahead } if ahead == &["apply rev1"]));
        assert!(matches!(
            &back[1],
            Event::Started { revision, applied: Some(_), assigned: None, .. } if revision.0 == "rev1"
        ));
        assert!(
            matches!(&back[2], Event::Plan { plan: p, facts } if p.digest == plan.digest && facts["os"]["family"] == "debian")
        );
        assert!(matches!(
            &back[3],
            Event::Done { exit_code: 2, error: Some(e) } if e.message == "refused" && e.advice == ["Try later."]
        ));
    }

    #[test]
    fn an_event_from_a_newer_agent_is_skipped_and_a_broken_plan_is_an_error() {
        assert!(
            event_from_proto(pb::LocalEvent { event: None })
                .expect("tolerated")
                .is_none()
        );
        let broken = pb::LocalEvent {
            event: Some(pb::local_event::Event::Plan(pb::LocalPlan {
                plan_json: b"{nope".to_vec(),
                facts_json: Vec::new(),
            })),
        };
        assert!(event_from_proto(broken).is_err());
    }
}
