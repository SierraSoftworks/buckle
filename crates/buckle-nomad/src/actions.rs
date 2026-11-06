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
        Action::Dispatch => dispatch(nomad, ns, id, &Default::default(), None).await,
        Action::Task { name, .. } => Err(user(
            format!("The task action {name} of job {id} runs in an allocation."),
            &["Run it with `run_task`, naming the allocation."],
        )),
    }
}

/// The longest a task action may run before Buckle gives up on it.
pub const TASK_ACTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);

/// How often a running action's socket is told the caller is still there,
/// as Nomad's own client does.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(10);

/// What a task action did in one allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRun {
    pub exit_code: i64,
    /// Its stdout and stderr as they arrived, the last `limit` bytes kept.
    pub output: String,
}

/// The running allocations of `group` of job `id` in `ns`, by id.
pub async fn running_allocations(
    nomad: &Nomad,
    ns: &str,
    id: &str,
    group: &str,
) -> Result<Vec<String>> {
    let allocs = nomad
        .get::<Vec<Value>>(&with_query(
            &format!("/v1/job/{}/allocations", seg(id)),
            "namespace",
            ns,
        ))
        .await?
        .unwrap_or_default();
    Ok(allocs
        .iter()
        .filter(|a| a["ClientStatus"].as_str() == Some("running"))
        .filter(|a| a["TaskGroup"].as_str() == Some(group))
        .filter_map(|a| a["ID"].as_str().map(str::to_owned))
        .collect())
}

/// Whether the registered version of job `id` defines `action` on `task`.
pub async fn defines(nomad: &Nomad, ns: &str, id: &str, task: &str, action: &str) -> Result<bool> {
    let job = nomad
        .get::<Value>(&with_query(
            &format!("/v1/job/{}", seg(id)),
            "namespace",
            ns,
        ))
        .await?;
    Ok(job.is_some_and(|job| {
        discover(&job)
            .iter()
            .any(|a| matches!(a, Action::Task { task: t, name, .. } if t == task && name == action))
    }))
}

/// Runs task `action` of `task` in allocation `alloc` of job `id` (in
/// `ns`) through Nomad's action endpoint, with no stdin and no TTY, and
/// waits for it to exit. `on_line` sees each complete line of output as it
/// arrives; the output kept is its last `limit` bytes.
#[allow(clippy::too_many_arguments)]
pub async fn run_task(
    nomad: &Nomad,
    ns: &str,
    id: &str,
    task: &str,
    action: &str,
    alloc: &str,
    limit: usize,
    on_line: &mut (dyn FnMut(&str) + Send),
) -> Result<TaskRun> {
    let mut path = format!("/v1/job/{}/action", seg(id));
    for (k, v) in [
        ("namespace", ns),
        ("action", action),
        ("allocID", alloc),
        ("task", task),
        ("tty", "false"),
    ] {
        path = with_query(&path, k, v);
    }
    let mut socket = nomad.socket(&path).await?;
    socket
        .send(json!({"stdin": {"close": true}}).to_string())
        .await?;
    let mut output = Tail::new(limit);
    let mut line = Vec::new();
    let run = async {
        let mut heartbeat = tokio::time::interval(HEARTBEAT);
        heartbeat.tick().await;
        loop {
            let frame = tokio::select! {
                frame = socket.recv() => frame?,
                _ = heartbeat.tick() => {
                    socket.send("{}".to_owned()).await?;
                    continue;
                }
            };
            let Some(frame) = frame else {
                return Err(buckle_core::error::system(
                    format!(
                        "Nomad closed the action {action} in allocation {} before it exited.",
                        short(alloc)
                    ),
                    &["Check the allocation's events in Nomad: it may have stopped."],
                ));
            };
            let frame: Value = serde_json::from_str(&frame).unwrap_or(Value::Null);
            if frame["exited"].as_bool() == Some(true) {
                return Ok(frame["result"]["exit_code"].as_i64().unwrap_or(0));
            }
            for stream in ["stdout", "stderr"] {
                let Some(data) = frame[stream]["data"].as_str() else {
                    continue;
                };
                use base64::Engine as _;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap_or_default();
                output.push(&bytes);
                for b in bytes {
                    if b == b'\n' {
                        on_line(String::from_utf8_lossy(&line).trim_end());
                        line.clear();
                    } else {
                        line.push(b);
                    }
                }
            }
        }
    };
    let exit_code = tokio::time::timeout(TASK_ACTION_TIMEOUT, run)
        .await
        .map_err(|_| {
            user(
                format!(
                    "The action {action} in allocation {} ran past {}m.",
                    short(alloc),
                    TASK_ACTION_TIMEOUT.as_secs() / 60
                ),
                &["It may still be running in the allocation; check it in Nomad."],
            )
        })??;
    if !line.is_empty() {
        on_line(String::from_utf8_lossy(&line).trim_end());
    }
    Ok(TaskRun {
        exit_code,
        output: output.text(),
    })
}

/// An allocation's id as Nomad's CLI shows it: the first eight characters.
pub fn short(alloc: &str) -> &str {
    alloc.get(..8).unwrap_or(alloc)
}

/// The last `limit` bytes of a stream.
struct Tail {
    limit: usize,
    bytes: std::collections::VecDeque<u8>,
    dropped: bool,
}

impl Tail {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            bytes: std::collections::VecDeque::new(),
            dropped: false,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        while self.bytes.len() > self.limit {
            self.bytes.pop_front();
            self.dropped = true;
        }
    }

    fn text(&self) -> String {
        let (a, b) = self.bytes.as_slices();
        let text = String::from_utf8_lossy(&[a, b].concat()).into_owned();
        if !self.dropped {
            return text;
        }
        // The first line kept is cut short; start at the next.
        let rest = text
            .split_once('\n')
            .map_or(text.as_str(), |(_, rest)| rest);
        format!("… (earlier output dropped)\n{rest}")
    }
}

/// Dispatches the parameterized job `id` in `ns` with `meta` and an
/// optional `payload`, returning a summary naming the dispatched job.
pub async fn dispatch(
    nomad: &Nomad,
    ns: &str,
    id: &str,
    meta: &std::collections::BTreeMap<String, String>,
    payload: Option<&str>,
) -> Result<String> {
    use base64::Engine as _;
    let mut body = json!({"JobID": id});
    if !meta.is_empty() {
        body["Meta"] = json!(meta);
    }
    if let Some(payload) = payload {
        body["Payload"] = json!(base64::engine::general_purpose::STANDARD.encode(payload));
    }
    let resp: Value = nomad
        .write(
            "POST",
            &with_query(&format!("/v1/job/{}/dispatch", seg(id)), "namespace", ns),
            &body,
        )
        .await?;
    Ok(format!(
        "dispatched {}",
        resp["DispatchedJobID"].as_str().unwrap_or(id)
    ))
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

    /// A fake running job `paste` whose `web` task offers `stats`.
    async fn fake_with_action() -> crate::testing::FakeNomad {
        let fake = crate::testing::FakeNomad::new();
        let job = json!({"ID": "paste", "Name": "paste", "Type": "service", "TaskGroups": [{"Name": "web", "Count": 2,
            "Tasks": [{"Name": "web", "Actions": [{"Name": "stats", "Command": "/stats"}]}]}]});
        fake.client()
            .write::<_, Value>("POST", "/v1/jobs", &json!({"Job": job}))
            .await
            .unwrap();
        fake
    }

    #[tokio::test]
    async fn a_task_action_streams_its_lines_and_reports_its_exit() {
        let fake = fake_with_action().await;
        fake.task_action("stats", "one\ntwo\nlast", 3);
        let mut lines = Vec::new();
        let run = run_task(
            &fake.client(),
            "default",
            "paste",
            "web",
            "stats",
            "alloc-1",
            1024,
            &mut |l| lines.push(l.to_owned()),
        )
        .await
        .unwrap();
        assert_eq!(run.exit_code, 3);
        assert_eq!(run.output, "one\ntwo\nlast");
        assert_eq!(lines, ["one", "two", "last"]);
        let (_, path) = fake
            .requests()
            .into_iter()
            .find(|(m, _)| m == "WS")
            .unwrap();
        for part in [
            "/v1/job/paste/action?",
            "action=stats",
            "allocID=alloc-1",
            "task=web",
            "tty=false",
        ] {
            assert!(path.contains(part), "{path}");
        }
    }

    #[tokio::test]
    async fn running_allocations_and_defined_actions_come_from_the_job() {
        let fake = fake_with_action().await;
        fake.allocations("default", "paste", 2);
        let nomad = fake.client();
        assert_eq!(
            running_allocations(&nomad, "default", "paste", "web")
                .await
                .unwrap(),
            ["alloc-1", "alloc-2"]
        );
        assert!(
            running_allocations(&nomad, "default", "paste", "db")
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            defines(&nomad, "default", "paste", "web", "stats")
                .await
                .unwrap()
        );
        assert!(
            !defines(&nomad, "default", "paste", "web", "vacuum")
                .await
                .unwrap()
        );
        assert!(
            !defines(&nomad, "default", "gone", "web", "stats")
                .await
                .unwrap()
        );
    }

    #[test]
    fn output_keeps_its_tail_from_a_whole_line() {
        let mut tail = Tail::new(8);
        tail.push(b"abc\n");
        assert_eq!(tail.text(), "abc\n");
        tail.push(b"defgh\nij");
        assert_eq!(tail.text(), "… (earlier output dropped)\nij");
    }
}
