//! ApplyEvent backpressure (design §08, "Backpressure"): events are sent at
//! most once per 100 ms (10 per second per host). Events arriving faster are
//! buffered and coalesced per step, keeping only the latest event for each
//! step, module, handler, removal or check; the server stores the last event
//! per step anyway, and every event still goes to tracing. A failed or
//! blocked step's event is never replaced by a later event of the same step
//! (a rollback re-runs it). When the apply ends, up to one more second of
//! buffered events is flushed; after that only failed and blocked steps'
//! events still go out (at the same spacing) and the rest is dropped. The
//! ApplyResult that follows is never coalesced or cut: it carries the full
//! module and step breakdown and is the authoritative record of the run.

use crate::sink::ReportSink;
use crate::transport::Report;
use buckle_core::engine::ApplyEvent;
use buckle_core::plan::RevisionId;
use indexmap::IndexMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;

/// The minimum spacing between two ApplyEvent messages.
pub const EVENT_SPACING: Duration = Duration::from_millis(100);

/// The coalescing key of an event: later events with the same key replace
/// earlier ones that have not been sent yet.
pub fn coalesce_key(event: &ApplyEvent) -> String {
    match event {
        ApplyEvent::ModuleStarted { module } | ApplyEvent::ModuleFinished { module, .. } => {
            format!("module:{module}")
        }
        ApplyEvent::StepStarted {
            module, task_id, ..
        }
        | ApplyEvent::StepFinished {
            module, task_id, ..
        }
        | ApplyEvent::StepFailed {
            module, task_id, ..
        }
        | ApplyEvent::StepBlocked {
            module, task_id, ..
        }
        | ApplyEvent::StepSkipped {
            module, task_id, ..
        } => format!("step:{module}/{task_id}"),
        ApplyEvent::HandlerRun { module, handler } => format!("handler:{module}/{handler}"),
        ApplyEvent::RemovalRun { resource, .. } => format!("removal:{resource}"),
        ApplyEvent::HealthChecked { module, check, .. } => format!("health:{module}/{check}"),
        ApplyEvent::FactsRefreshed { .. } => "facts".to_owned(),
    }
}

/// Events a later event of the same step must not replace: a failure or a
/// block is what the reader needs to see even when the step runs again.
fn keeps_its_place(event: &ApplyEvent) -> bool {
    matches!(
        event,
        ApplyEvent::StepFailed { .. } | ApplyEvent::StepBlocked { .. }
    )
}

/// Forwards events from `events` to `sink` until the channel closes,
/// honouring [`EVENT_SPACING`]. Returns `(sent, dropped)`.
pub async fn forward(
    events: mpsc::Receiver<ApplyEvent>,
    sink: ReportSink,
    revision: RevisionId,
    plan_digest: String,
    spacing: Duration,
) -> (usize, usize) {
    forward_for(events, sink, revision, plan_digest, spacing, None).await
}

/// [`forward`] for the cluster `target` (`None`: the host itself).
pub async fn forward_for(
    mut events: mpsc::Receiver<ApplyEvent>,
    sink: ReportSink,
    revision: RevisionId,
    plan_digest: String,
    spacing: Duration,
    target: Option<String>,
) -> (usize, usize) {
    let mut pending: IndexMap<String, (chrono::DateTime<chrono::Utc>, ApplyEvent)> =
        IndexMap::new();
    let mut last_sent: Option<Instant> = None;
    let mut sent = 0usize;
    let mut superseded = 0usize;
    let mut unique = 0usize;

    let send = |at: chrono::DateTime<chrono::Utc>, event: ApplyEvent| {
        let sink = sink.clone();
        let report = Report::ApplyEvent {
            revision: revision.clone(),
            plan_digest: plan_digest.clone(),
            at,
            event,
        }
        .about(target.as_deref());
        async move { sink.send(report).await }
    };

    loop {
        let ready_at = last_sent.map(|t| t + spacing);
        let can_send_now = ready_at.is_none_or(|t| Instant::now() >= t);
        if can_send_now && let Some((_, (at, event))) = pending.shift_remove_index(0) {
            send(at, event).await;
            sent += 1;
            last_sent = Some(Instant::now());
            continue;
        }

        let wake = async {
            match (ready_at, pending.is_empty()) {
                (Some(t), false) => tokio::time::sleep_until(t).await,
                _ => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            biased;
            event = events.recv() => match event {
                Some(event) => {
                    tracing::debug!(event = ?event, "apply event");
                    let key = if keeps_its_place(&event) {
                        // Never replaced: under its own key.
                        unique += 1;
                        format!("{}#{unique}", coalesce_key(&event))
                    } else {
                        let key = coalesce_key(&event);
                        if pending.shift_remove(&key).is_some() {
                            superseded += 1;
                        }
                        key
                    };
                    pending.insert(key, (chrono::Utc::now(), event));
                }
                None => break,
            },
            _ = wake => {}
        }
    }

    // Final flush: at most one more second of events, then only failures and
    // blocks (few, and what a reader most needs), still spaced.
    let budget = (Duration::from_secs(1).as_millis() / spacing.as_millis().max(1)) as usize;
    let mut flushed = 0usize;
    let mut cut = 0usize;
    while let Some((_, (at, event))) = pending.shift_remove_index(0) {
        if flushed >= budget && !keeps_its_place(&event) {
            cut += 1;
            continue;
        }
        if let Some(t) = last_sent.map(|t| t + spacing) {
            tokio::time::sleep_until(t).await;
        }
        send(at, event).await;
        sent += 1;
        flushed += 1;
        last_sent = Some(Instant::now());
    }
    let dropped = superseded + cut;
    if dropped > 0 {
        tracing::debug!(
            sent,
            dropped,
            "apply events were coalesced; the apply result carries the full breakdown"
        );
    }
    (sent, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use buckle_core::engine::ModuleStatus;

    fn step(i: usize, finished: bool) -> ApplyEvent {
        if finished {
            ApplyEvent::StepFinished {
                module: "m".into(),
                task_id: format!("t{i}"),
                id: format!("file.copy[t{i}]"),
                kind: "file.copy".into(),
                changed: true,
                summary: "wrote".into(),
            }
        } else {
            ApplyEvent::StepStarted {
                module: "m".into(),
                task_id: format!("t{i}"),
                id: format!("file.copy[t{i}]"),
                kind: "file.copy".into(),
            }
        }
    }

    #[test]
    fn keys_group_events_of_the_same_step() {
        assert_eq!(coalesce_key(&step(1, false)), coalesce_key(&step(1, true)));
        assert_ne!(coalesce_key(&step(1, false)), coalesce_key(&step(2, false)));
        assert_eq!(
            coalesce_key(&ApplyEvent::ModuleStarted { module: "m".into() }),
            coalesce_key(&ApplyEvent::ModuleFinished {
                module: "m".into(),
                status: ModuleStatus::Applied
            })
        );
    }

    fn failed(i: usize) -> ApplyEvent {
        ApplyEvent::StepFailed {
            module: "m".into(),
            task_id: format!("t{i}"),
            id: format!("file.copy[t{i}]"),
            kind: "file.copy".into(),
            error: "boom".into(),
            error_detail: None,
        }
    }

    async fn forward_all(events: Vec<ApplyEvent>) -> (Vec<ApplyEvent>, usize) {
        let (tx, rx) = mpsc::channel(4096);
        let (report_tx, mut report_rx) = mpsc::channel(4096);
        let sink = ReportSink::default();
        sink.attach(report_tx);
        for e in events {
            tx.send(e).await.expect("send");
        }
        drop(tx);
        let (_, dropped) = forward(rx, sink, RevisionId::new("r"), "d".into(), EVENT_SPACING).await;
        let mut out = Vec::new();
        while let Ok(Report::ApplyEvent { event, .. }) = report_rx.try_recv() {
            out.push(event);
        }
        (out, dropped)
    }

    #[tokio::test(start_paused = true)]
    async fn failures_survive_the_end_of_a_burst() {
        let mut events = Vec::new();
        for i in 0..200 {
            events.push(step(i, false));
            events.push(step(i, true));
        }
        events.push(failed(500));
        let (sent, dropped) = forward_all(events).await;
        assert!(dropped > 0, "the burst was cut");
        assert!(
            sent.len() <= 12,
            "still about a second of events: {}",
            sent.len()
        );
        assert!(
            matches!(sent.last(), Some(ApplyEvent::StepFailed { .. })),
            "the failure is sent even past the flush budget: {sent:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_is_never_replaced_by_a_later_event_of_the_same_step() {
        // The rollback runs the same step again right after it failed.
        let (sent, _) = forward_all(vec![
            step(1, false),
            failed(1),
            step(1, false),
            step(1, true),
        ])
        .await;
        assert!(
            sent.iter()
                .any(|e| matches!(e, ApplyEvent::StepFailed { .. })),
            "{sent:?}"
        );
        assert!(matches!(sent.last(), Some(ApplyEvent::StepFinished { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_is_rate_limited_and_coalesced() {
        let (tx, rx) = mpsc::channel(1024);
        let (report_tx, mut report_rx) = mpsc::channel(1024);
        let sink = ReportSink::default();
        sink.attach(report_tx);
        let start = Instant::now();
        for i in 0..50 {
            tx.send(step(i, false)).await.expect("send");
            tx.send(step(i, true)).await.expect("send");
        }
        drop(tx);
        let (sent, dropped) =
            forward(rx, sink, RevisionId::new("r"), "d".into(), EVENT_SPACING).await;
        assert_eq!(sent + dropped, 100);
        let mut times = Vec::new();
        while let Ok(report) = report_rx.try_recv() {
            assert!(matches!(report, Report::ApplyEvent { .. }));
            times.push(Instant::now());
        }
        assert_eq!(times.len(), sent);
        assert!(
            sent <= 11,
            "a burst is cut to about a second of events, got {sent}"
        );
        assert!(Instant::now() - start <= Duration::from_millis(1100));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_events_all_get_through_in_order() {
        let (tx, rx) = mpsc::channel(16);
        let (report_tx, mut report_rx) = mpsc::channel(16);
        let sink = ReportSink::default();
        sink.attach(report_tx);
        let producer = tokio::spawn(async move {
            for i in 0..5 {
                tx.send(step(i, true)).await.expect("send");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });
        let (sent, dropped) =
            forward(rx, sink, RevisionId::new("r"), "d".into(), EVENT_SPACING).await;
        producer.await.expect("producer");
        assert_eq!((sent, dropped), (5, 0));
        let mut ids = Vec::new();
        while let Ok(Report::ApplyEvent {
            event: ApplyEvent::StepFinished { task_id, .. },
            ..
        }) = report_rx.try_recv()
        {
            ids.push(task_id);
        }
        assert_eq!(ids, vec!["t0", "t1", "t2", "t3", "t4"]);
    }
}
