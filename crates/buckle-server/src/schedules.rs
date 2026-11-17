//! Scheduled actions: each occurrence of a stable revision's
//! `schedules/<name>.yaml` starts an action rollout, as an operator's Run
//! action… would, tagged with the schedule.
//!
//! An occurrence that finds a rollout under way waits for it, for up to the
//! schedule's `window`, and is skipped after that; so is one the server was
//! down for longer than that. Only a schedule's latest occurrence is ever
//! started, so a long outage catches up at most once. A schedule first seen
//! after an occurrence was due never starts that one.

use crate::clock::Clock;
use crate::rollout::{CommandErrorKind, ControllerHandle};
use crate::source::cache::RevisionCache;
use crate::store::Store;
use buckle_api::{ScheduleDetail, ScheduleInfo, ScheduleOccurrence, ScheduleTarget};
use buckle_core::model::{ScheduleApproval, ScheduleDecl};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

/// How many occurrences each schedule keeps.
const HISTORY: usize = 100;
/// The longest the scheduler sleeps, so a newly stable revision's schedules
/// are picked up.
const TICK: chrono::TimeDelta = chrono::TimeDelta::seconds(30);

fn since_key(name: &str) -> String {
    format!("schedule/{name}/since")
}

fn history_key(name: &str) -> String {
    format!("schedule/{name}/history")
}

/// One occurrence as kept: the rollout it started, or why it started none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Occurrence {
    pub due_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// What to do about a schedule now.
#[derive(Debug, PartialEq, Eq)]
pub enum Due {
    /// Nothing is due.
    Idle,
    /// The occurrence at this time is to start.
    Start(DateTime<Utc>),
    /// The occurrence at this time is past its window: skip it.
    Expired(DateTime<Utc>),
}

/// Whether `decl`'s latest occurrence (up to `now`) is still to be handled:
/// it is after both the last one handled and when the schedule was first
/// seen. Past its `window`, it is skipped.
pub fn due(
    decl: &ScheduleDecl,
    now: DateTime<Utc>,
    since: DateTime<Utc>,
    handled: Option<DateTime<Utc>>,
) -> Due {
    let Ok(parsed) = decl.parsed() else {
        return Due::Idle;
    };
    let Some(at) = parsed.latest_until(now) else {
        return Due::Idle;
    };
    if at <= since || handled.is_some_and(|h| at <= h) {
        return Due::Idle;
    }
    let window = chrono::Duration::from_std(decl.window).unwrap_or(chrono::TimeDelta::MAX);
    if now - at > window {
        Due::Expired(at)
    } else {
        Due::Start(at)
    }
}

/// Starts each schedule's occurrences, and answers for them.
pub struct Scheduler {
    store: Store,
    revisions: Arc<RevisionCache>,
    controller: ControllerHandle,
    clock: Arc<dyn Clock>,
    /// Occurrences waiting for a rollout under way: name → (due, why).
    waiting: Mutex<HashMap<String, (DateTime<Utc>, String)>>,
}

impl Scheduler {
    pub fn new(
        store: Store,
        revisions: Arc<RevisionCache>,
        controller: ControllerHandle,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            revisions,
            controller,
            clock,
            waiting: Mutex::default(),
        }
    }

    /// Starts occurrences as they fall due until `cancel`.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            let now = self.clock.now();
            let next = match self.tick(now).await {
                Ok(next) => next,
                Err(e) => {
                    tracing::warn!(error = %e, "the scheduled actions could not be checked");
                    None
                }
            };
            let wake = next.map_or(now + TICK, |n| n.min(now + TICK));
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.clock.sleep_until(wake) => {}
            }
        }
    }

    /// Handles every schedule's due occurrence; returns when the next one
    /// falls due.
    pub async fn tick(&self, now: DateTime<Utc>) -> buckle_core::Result<Option<DateTime<Utc>>> {
        let Some(rev) = crate::views::stable_revision(&self.store, &self.revisions).await else {
            return Ok(None);
        };
        let mut next: Option<DateTime<Utc>> = None;
        for (name, decl) in &rev.schedules {
            let (since, handled) = self.state(name, now).await?;
            match due(decl, now, since, handled) {
                Due::Idle => {}
                Due::Expired(at) => {
                    let why = self
                        .waiting
                        .lock()
                        .unwrap()
                        .remove(name)
                        .filter(|(due, _)| *due == at)
                        .map(|(_, why)| {
                            format!("Not started within its {} window: {why}", window_text(decl))
                        })
                        .unwrap_or_else(|| {
                            format!(
                                "Not started within its {} window: the server was not running.",
                                window_text(decl)
                            )
                        });
                    tracing::info!(schedule = %name, due = %at, reason = %why, "scheduled action skipped");
                    self.record(
                        name,
                        Occurrence {
                            due_at: at,
                            rollout_id: None,
                            skipped: Some(why),
                        },
                    )
                    .await?;
                }
                Due::Start(at) => self.start(name, decl, at).await?,
            }
            if let Ok(p) = decl.parsed()
                && let Some(n) = p.next_after(now)
            {
                next = Some(next.map_or(n, |m| m.min(n)));
            }
        }
        self.waiting
            .lock()
            .unwrap()
            .retain(|name, _| rev.schedules.contains_key(name));
        Ok(next)
    }

    async fn start(
        &self,
        name: &str,
        decl: &ScheduleDecl,
        at: DateTime<Utc>,
    ) -> buckle_core::Result<()> {
        let Some((module, workload)) = decl.target() else {
            return Ok(());
        };
        let body = buckle_api::StartActionRollout {
            module: module.to_owned(),
            action: decl.action.clone(),
            workload,
            selector: decl.select.as_ref().map(|s| s.raw().to_owned()),
            note: decl.note.clone(),
            instances: None,
        };
        let origin = crate::actions::Origin {
            schedule: Some(name.to_owned()),
            auto_approve: decl.approval == ScheduleApproval::Auto,
        };
        let actor = format!("schedule:{name}");
        let result = crate::actions::start_rollout(
            &self.store,
            &self.revisions,
            &self.controller,
            &actor,
            body,
            origin,
        )
        .await;
        match result {
            Ok(id) => {
                self.waiting.lock().unwrap().remove(name);
                tracing::info!(schedule = %name, due = %at, rollout = %id, "scheduled action started");
                self.record(
                    name,
                    Occurrence {
                        due_at: at,
                        rollout_id: Some(id),
                        skipped: None,
                    },
                )
                .await
            }
            // A rollout under way, or a failure of the server's own: try
            // again on the next tick, while the window lasts.
            Err(e)
                if matches!(
                    e.kind,
                    CommandErrorKind::Conflict | CommandErrorKind::Internal
                ) =>
            {
                let why = e.error.description().to_string();
                self.waiting
                    .lock()
                    .unwrap()
                    .insert(name.to_owned(), (at, why));
                Ok(())
            }
            Err(e) => {
                self.waiting.lock().unwrap().remove(name);
                let why = e.error.description().to_string();
                tracing::info!(schedule = %name, due = %at, reason = %why, "scheduled action skipped");
                self.record(
                    name,
                    Occurrence {
                        due_at: at,
                        rollout_id: None,
                        skipped: Some(why),
                    },
                )
                .await
            }
        }
    }

    /// When the schedule was first seen (now, the first time), and its
    /// latest occurrence handled.
    async fn state(
        &self,
        name: &str,
        now: DateTime<Utc>,
    ) -> buckle_core::Result<(DateTime<Utc>, Option<DateTime<Utc>>)> {
        let name = name.to_owned();
        self.store
            .call(move |db| {
                let kv = db.kv();
                let since = match kv
                    .get(&since_key(&name))?
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                {
                    Some(s) => s.with_timezone(&Utc),
                    None => {
                        kv.set(&since_key(&name), &now.to_rfc3339())?;
                        now
                    }
                };
                let handled = history(&kv, &name)?.first().map(|o| o.due_at);
                Ok((since, handled))
            })
            .await
    }

    async fn record(&self, name: &str, occurrence: Occurrence) -> buckle_core::Result<()> {
        let name = name.to_owned();
        self.store
            .call(move |db| {
                let kv = db.kv();
                let mut list = history(&kv, &name)?;
                list.insert(0, occurrence);
                list.truncate(HISTORY);
                let json = crate::store::to_json(&list)?;
                kv.set(&history_key(&name), &json)
            })
            .await
    }

    /// Every schedule of the stable revision, as the Schedules page lists it.
    pub async fn list(&self) -> buckle_core::Result<Vec<ScheduleInfo>> {
        let Some(rev) = crate::views::stable_revision(&self.store, &self.revisions).await else {
            return Ok(Vec::new());
        };
        let offered = crate::actions::fleet(&self.store, &self.revisions, None).await?;
        let mut out = Vec::new();
        for (name, decl) in &rev.schedules {
            let history = self.history(name, 1).await?;
            out.push(self.info(name, decl, &offered, history.into_iter().next()));
        }
        Ok(out)
    }

    /// One schedule, the targets it picks now, and its occurrences.
    pub async fn detail(&self, name: &str) -> buckle_core::Result<Option<ScheduleDetail>> {
        let Some(rev) = crate::views::stable_revision(&self.store, &self.revisions).await else {
            return Ok(None);
        };
        let Some(decl) = rev.schedules.get(name) else {
            return Ok(None);
        };
        let offered =
            crate::actions::fleet(&self.store, &self.revisions, decl.select.as_ref()).await?;
        let history = self.history(name, HISTORY).await?;
        let info = self.info(name, decl, &offered, history.first().cloned());
        let picked: Vec<String> = offered
            .iter()
            .find(|f| matches_action(f, decl))
            .map(|f| f.targets.clone())
            .unwrap_or_default();
        let latest = history
            .iter()
            .find_map(|o| o.rollout.as_ref().map(|r| r.id.clone()));
        let targets = self
            .store
            .call(move |db| {
                let rows: Vec<buckle_api::RolloutHost> = match latest {
                    Some(id) => match db.rollouts().get(&id)? {
                        Some(r) => {
                            let hosts = db.rollouts().hosts(&id)?;
                            crate::rollout::view::detail(&r, &hosts, &[], &[])
                                .phases
                                .into_iter()
                                .flat_map(|p| p.hosts)
                                .collect()
                        }
                        None => Vec::new(),
                    },
                    None => Vec::new(),
                };
                let mut out = Vec::new();
                let mut ids = picked;
                for row in &rows {
                    if !ids.contains(&row.host) {
                        ids.push(row.host.clone());
                    }
                }
                for id in ids {
                    let kind = db
                        .hosts()
                        .target(&id)?
                        .map_or(buckle_api::TargetKind::Host, |h| h.kind);
                    let last = rows.iter().find(|r| r.host == id).cloned();
                    out.push(ScheduleTarget { id, kind, last });
                }
                Ok(out)
            })
            .await?;
        Ok(Some(ScheduleDetail {
            schedule: info,
            targets,
            history,
        }))
    }

    fn info(
        &self,
        name: &str,
        decl: &ScheduleDecl,
        offered: &[buckle_api::FleetAction],
        last: Option<ScheduleOccurrence>,
    ) -> ScheduleInfo {
        let now = self.clock.now();
        let parsed = decl.parsed().ok();
        let (module, workload) = decl.target().unwrap_or(("", false));
        let waiting = self.waiting.lock().unwrap().get(name).cloned();
        // An occurrence waiting for a rollout under way is the latest.
        let last = match waiting {
            Some((due_at, why)) if last.as_ref().is_none_or(|l| l.due_at < due_at) => {
                Some(ScheduleOccurrence {
                    due_at,
                    rollout: None,
                    skipped: None,
                    waiting: Some(why),
                })
            }
            _ => last,
        };
        ScheduleInfo {
            name: name.to_owned(),
            description: decl.description.clone(),
            cron: decl.cron.clone(),
            cron_text: parsed.as_ref().map(|p| p.describe()).unwrap_or_default(),
            timezone: decl.timezone.clone(),
            module: module.to_owned(),
            action: decl.action.clone(),
            label: offered
                .iter()
                .find(|f| matches_action(f, decl))
                .map(|f| f.action.label.clone()),
            workload,
            selector: decl.select.as_ref().map(|s| s.raw().to_owned()),
            approval: match decl.approval {
                ScheduleApproval::Phases => "phases",
                ScheduleApproval::Auto => "auto",
            }
            .into(),
            window_seconds: decl.window.as_secs(),
            next_at: parsed.and_then(|p| p.next_after(now)),
            last,
        }
    }

    /// The schedule's latest `limit` occurrences, newest first, each with
    /// its rollout.
    async fn history(
        &self,
        name: &str,
        limit: usize,
    ) -> buckle_core::Result<Vec<ScheduleOccurrence>> {
        let name = name.to_owned();
        let list = self
            .store
            .call(move |db| {
                let mut out = Vec::new();
                for o in history(&db.kv(), &name)?.into_iter().take(limit) {
                    let rollout = match &o.rollout_id {
                        Some(id) => db.rollouts().get(id)?.map(|r| -> buckle_core::Result<_> {
                            let hosts = db.rollouts().hosts(id)?;
                            Ok(crate::rollout::view::summary(&r, &hosts))
                        }),
                        None => None,
                    }
                    .transpose()?;
                    out.push(ScheduleOccurrence {
                        due_at: o.due_at,
                        rollout,
                        skipped: o.skipped,
                        waiting: None,
                    });
                }
                Ok(out)
            })
            .await?;
        Ok(list)
    }
}

fn matches_action(f: &buckle_api::FleetAction, decl: &ScheduleDecl) -> bool {
    decl.target().is_some_and(|(m, w)| {
        f.workload == w && f.action.module == m && f.action.name == decl.action
    })
}

fn window_text(decl: &ScheduleDecl) -> String {
    humantime_serde::re::humantime::format_duration(decl.window).to_string()
}

fn history(kv: &crate::store::kv::Kv<'_>, name: &str) -> buckle_core::Result<Vec<Occurrence>> {
    Ok(kv
        .get(&history_key(name))?
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    /// Daily at 04:00 UTC, with an hour's window.
    fn daily() -> ScheduleDecl {
        ScheduleDecl::parse("cron: '0 4 * * *'\nmodule: m\naction: a\n").unwrap()
    }

    #[test]
    fn only_the_latest_unhandled_occurrence_in_its_window_starts() {
        let d = daily();
        let since = at("2026-11-01T00:00:00Z");
        let cases = [
            // Due a minute ago, never handled.
            (
                "2026-11-17T04:01:00Z",
                None,
                Due::Start(at("2026-11-17T04:00:00Z")),
            ),
            // Already handled.
            (
                "2026-11-17T04:01:00Z",
                Some("2026-11-17T04:00:00Z"),
                Due::Idle,
            ),
            // Past the hour's window: skipped.
            (
                "2026-11-17T05:30:00Z",
                Some("2026-11-16T04:00:00Z"),
                Due::Expired(at("2026-11-17T04:00:00Z")),
            ),
            // Down for days: only the latest counts.
            (
                "2026-11-17T04:10:00Z",
                Some("2026-11-12T04:00:00Z"),
                Due::Start(at("2026-11-17T04:00:00Z")),
            ),
        ];
        for (now, handled, want) in cases {
            assert_eq!(due(&d, at(now), since, handled.map(at)), want, "at {now}");
        }
    }

    #[test]
    fn a_new_schedule_never_starts_what_was_due_before_it() {
        let d = daily();
        let now = at("2026-11-17T04:30:00Z");
        assert_eq!(due(&d, now, at("2026-11-17T04:20:00Z"), None), Due::Idle);
        assert_eq!(
            due(
                &d,
                at("2026-11-18T04:00:30Z"),
                at("2026-11-17T04:20:00Z"),
                None
            ),
            Due::Start(at("2026-11-18T04:00:00Z"))
        );
    }
}
