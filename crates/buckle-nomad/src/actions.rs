//! Implicit actions on a job, discovered from its spec: Restart, Stop and
//! Roll back on every job, Dispatch when it is parameterized, and one entry
//! per `action` block of its tasks (Nomad 1.7+). None are written in the
//! workload.

use crate::client::{Nomad, seg, with_query};
use buckle_core::Result;
use buckle_core::error::user;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// An action a job offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// Restart every running allocation in place, one at a time.
    Restart,
    /// Stop the job (its data stays).
    Stop,
    /// Re-register the newest stable version older than the current one.
    Revert,
    /// Dispatch a parameterized job.
    Dispatch,
    /// A task's `action` block.
    Task {
        group: String,
        task: String,
        name: String,
    },
}

impl Action {
    pub fn label(&self) -> String {
        match self {
            Action::Restart => "Restart".into(),
            Action::Stop => "Stop…".into(),
            Action::Revert => "Roll back…".into(),
            Action::Dispatch => "Dispatch…".into(),
            Action::Task { task, name, .. } => format!("{name} ({task})"),
        }
    }

    /// Whether the action needs a confirmation in the UI.
    pub fn destructive(&self) -> bool {
        matches!(self, Action::Stop | Action::Revert)
    }
}

/// The actions `job` (a job as `GET /v1/job/:id` returns it) offers.
pub fn discover(job: &Value) -> Vec<Action> {
    let mut out = vec![Action::Restart, Action::Stop];
    if job["Version"].as_u64().unwrap_or(0) > 0 {
        out.push(Action::Revert);
    }
    if job["ParameterizedJob"].is_object() {
        out.push(Action::Dispatch);
    }
    for group in job["TaskGroups"].as_array().into_iter().flatten() {
        for task in group["Tasks"].as_array().into_iter().flatten() {
            for action in task["Actions"].as_array().into_iter().flatten() {
                out.push(Action::Task {
                    group: group["Name"].as_str().unwrap_or_default().to_owned(),
                    task: task["Name"].as_str().unwrap_or_default().to_owned(),
                    name: action["Name"].as_str().unwrap_or_default().to_owned(),
                });
            }
        }
    }
    out
}

/// Runs `action` on job `id` in `ns`, returning a summary.
pub async fn run(nomad: &Nomad, ns: &str, id: &str, action: &Action) -> Result<String> {
    let job_path = format!("/v1/job/{}", seg(id));
    match action {
        Action::Stop => {
            nomad
                .delete(&with_query(&job_path, "namespace", ns))
                .await?;
            Ok(format!("stopped job {id}"))
        }
        Action::Restart => {
            let allocs = nomad
                .get::<Vec<Value>>(&with_query(
                    &format!("{job_path}/allocations"),
                    "namespace",
                    ns,
                ))
                .await?
                .unwrap_or_default();
            let running: Vec<&str> = allocs
                .iter()
                .filter(|a| a["ClientStatus"].as_str() == Some("running"))
                .filter_map(|a| a["ID"].as_str())
                .collect();
            for alloc in &running {
                let url = with_query(
                    &format!("/v1/client/allocation/{}/restart", seg(alloc)),
                    "namespace",
                    ns,
                );
                nomad
                    .write::<_, Value>("POST", &url, &json!({"AllTasks": true}))
                    .await?;
            }
            Ok(format!(
                "restarted {} allocation(s) of job {id}",
                running.len()
            ))
        }
        Action::Revert => {
            let versions = nomad
                .get::<Value>(&with_query(
                    &format!("{job_path}/versions"),
                    "namespace",
                    ns,
                ))
                .await?
                .unwrap_or(Value::Null);
            let list = versions["Versions"].as_array().cloned().unwrap_or_default();
            let current = list
                .iter()
                .filter_map(|v| v["Version"].as_u64())
                .max()
                .unwrap_or(0);
            let Some(stable) = list
                .iter()
                .filter(|v| v["Stable"].as_bool().unwrap_or(false))
                .filter_map(|v| v["Version"].as_u64())
                .filter(|v| *v < current)
                .max()
            else {
                return Err(user(
                    format!("Job {id} has no earlier stable version to roll back to."),
                    &["A version becomes stable when its deployment succeeds."],
                ));
            };
            let body = json!({"JobID": id, "JobVersion": stable, "EnforcePriorVersion": current});
            nomad
                .write::<_, Value>(
                    "POST",
                    &with_query(&format!("{job_path}/revert"), "namespace", ns),
                    &body,
                )
                .await?;
            Ok(format!("rolled job {id} back to version {stable}"))
        }
        Action::Dispatch => {
            let resp: Value = nomad
                .write(
                    "POST",
                    &with_query(&format!("{job_path}/dispatch"), "namespace", ns),
                    &json!({"JobID": id}),
                )
                .await?;
            Ok(format!(
                "dispatched {}",
                resp["DispatchedJobID"].as_str().unwrap_or(id)
            ))
        }
        Action::Task { name, .. } => Err(user(
            format!("The task action {name} of job {id} runs from Nomad for now."),
            &["Run it with `nomad action` or from Nomad's UI."],
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_discovered_from_the_spec() {
        let job = json!({
            "Version": 2,
            "ParameterizedJob": {"Payload": "optional"},
            "TaskGroups": [{"Name": "api", "Tasks": [{"Name": "atuin", "Actions": [{"Name": "vacuum"}]}]}],
        });
        let found = discover(&job);
        assert!(found.contains(&Action::Revert) && found.contains(&Action::Dispatch));
        assert!(found.contains(&Action::Task {
            group: "api".into(),
            task: "atuin".into(),
            name: "vacuum".into()
        }));
        assert_eq!(
            discover(&json!({"Version": 0})),
            vec![Action::Restart, Action::Stop]
        );
    }
}
