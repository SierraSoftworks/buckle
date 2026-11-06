//! An in-memory Nomad for tests: enough of the HTTP API for the primitives
//! to plan, apply, detect orphans and remove, with the index semantics that
//! matter (job modify indexes, variable check-and-set).
//!
//! It does not schedule anything. `/v1/jobs/parse` reads the job spec as
//! JSON (a job as the parse endpoint would return it); a job's deployment
//! ends in [`FakeNomad::deployment_outcome`].

use crate::transport::{Request, Response, Transport};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
struct State {
    index: u64,
    jobs: BTreeMap<(String, String), Value>,
    /// Job submissions by (namespace, id, version).
    submissions: BTreeMap<(String, String, u64), Value>,
    vars: BTreeMap<(String, String), (BTreeMap<String, String>, u64)>,
    policies: BTreeMap<String, Value>,
    namespaces: BTreeMap<String, Value>,
    pools: BTreeMap<String, Value>,
    volumes: Vec<Value>,
    outcome: String,
    /// Reads of the blocked evaluation left before it completes, after a
    /// registration whose placement fails (`usize::MAX`: never).
    blocked_polls: Option<usize>,
    /// Jobs whose deployments stay `running` until released.
    held: std::collections::BTreeSet<(String, String)>,
    requests: Vec<(String, String)>,
    /// The `X-Nomad-Token` of each request (empty when none).
    tokens: Vec<String>,
    /// The JWTs presented to `/v1/acl/login`.
    logins: Vec<String>,
    /// Running allocations per job (default one), named `alloc-1`, ….
    allocs: BTreeMap<(String, String), usize>,
    /// How task actions end, by action name: output and exit code
    /// (default: `ran <action> in <alloc>`, 0).
    task_actions: BTreeMap<String, (String, i64)>,
}

/// The fake. Cheap to clone; clones share state.
#[derive(Debug, Clone)]
pub struct FakeNomad {
    state: Arc<Mutex<State>>,
    /// Woken by every write, for blocking queries (`?index=`).
    changed: Arc<tokio::sync::Notify>,
}

impl Default for FakeNomad {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeNomad {
    pub fn new() -> Self {
        let state = State {
            outcome: "successful".into(),
            ..Default::default()
        };
        let fake = Self {
            state: Arc::new(Mutex::new(state)),
            changed: Arc::new(tokio::sync::Notify::new()),
        };
        fake.state.lock().unwrap().namespaces.insert(
            "default".into(),
            json!({"Name": "default", "ModifyIndex": 1}),
        );
        fake
    }

    /// A client over this fake.
    pub fn client(&self) -> crate::Nomad {
        crate::Nomad::with_transport(Arc::new(self.clone()))
    }

    /// How the next deployments end: `successful`, `failed`, `cancelled`.
    pub fn deployment_outcome(&self, outcome: &str) {
        let mut s = self.state.lock().unwrap();
        s.outcome = outcome.into();
        s.index += 1;
        drop(s);
        self.changed.notify_waiters();
    }

    /// Fails the placement of the next registrations: their evaluation
    /// completes with a port collision and a blocked evaluation, which stays
    /// `blocked` for `polls` reads and then completes (it placed).
    pub fn block_placement(&self, polls: usize) {
        self.state.lock().unwrap().blocked_polls = Some(polls);
    }

    /// Keeps the deployment of `ns`/`id` running until [`Self::release`].
    pub fn hold(&self, ns: &str, id: &str) {
        let mut s = self.state.lock().unwrap();
        s.held.insert((ns.into(), id.into()));
        s.index += 1;
        drop(s);
        self.changed.notify_waiters();
    }

    /// Lets a held deployment end in the configured outcome.
    pub fn release(&self, ns: &str, id: &str) {
        let mut s = self.state.lock().unwrap();
        s.held.remove(&(ns.into(), id.into()));
        s.index += 1;
        drop(s);
        self.changed.notify_waiters();
    }

    /// Runs `n` allocations of job `ns`/`id` (one by default).
    pub fn allocations(&self, ns: &str, id: &str, n: usize) {
        self.state
            .lock()
            .unwrap()
            .allocs
            .insert((ns.into(), id.into()), n);
    }

    /// Makes task action `action` print `output` and exit with `code`.
    pub fn task_action(&self, action: &str, output: &str, code: i64) {
        self.state
            .lock()
            .unwrap()
            .task_actions
            .insert(action.into(), (output.into(), code));
    }

    /// A stored job, as `GET /v1/job/:id` returns it.
    pub fn job(&self, ns: &str, id: &str) -> Option<Value> {
        self.state
            .lock()
            .unwrap()
            .jobs
            .get(&(ns.into(), id.into()))
            .cloned()
    }

    /// Edits a stored job out of band (an operator's `nomad job run`).
    pub fn edit_job(&self, ns: &str, id: &str, edit: impl FnOnce(&mut Value)) {
        let mut s = self.state.lock().unwrap();
        s.index += 1;
        let index = s.index;
        if let Some(job) = s.jobs.get_mut(&(ns.into(), id.into())) {
            edit(job);
            job["JobModifyIndex"] = json!(index);
            job["Version"] = json!(job["Version"].as_u64().unwrap_or(0) + 1);
        }
        self.changed.notify_waiters();
    }

    /// The submission registered with a job's current version.
    pub fn submission(&self, ns: &str, id: &str) -> Option<Value> {
        let s = self.state.lock().unwrap();
        let version = s.jobs.get(&(ns.into(), id.into()))?["Version"].as_u64()?;
        s.submissions.get(&(ns.into(), id.into(), version)).cloned()
    }

    /// Registers a new version of a job out of band, with its submission
    /// (nomad-auto-update re-rendering it with new variable values).
    pub fn reregister(&self, ns: &str, id: &str, job: Value, submission: Value) {
        let mut s = self.state.lock().unwrap();
        s.index += 1;
        let index = s.index;
        let key = (ns.to_owned(), id.to_owned());
        let version = s
            .jobs
            .get(&key)
            .map_or(0, |j| j["Version"].as_u64().unwrap_or(0) + 1);
        let mut stored = job;
        stored["Version"] = json!(version);
        stored["JobModifyIndex"] = json!(index);
        stored["Stop"] = json!(false);
        s.jobs.insert(key, stored);
        s.submissions
            .insert((ns.to_owned(), id.to_owned(), version), submission);
        self.changed.notify_waiters();
    }

    /// A stored variable's items.
    pub fn variable(&self, ns: &str, path: &str) -> Option<BTreeMap<String, String>> {
        self.state
            .lock()
            .unwrap()
            .vars
            .get(&(ns.into(), path.into()))
            .map(|(i, _)| i.clone())
    }

    /// Writes a variable out of band.
    pub fn put_variable(&self, ns: &str, path: &str, items: &[(&str, &str)]) {
        let mut s = self.state.lock().unwrap();
        s.index += 1;
        let index = s.index;
        s.vars.insert(
            (ns.into(), path.into()),
            (
                items
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect(),
                index,
            ),
        );
        self.changed.notify_waiters();
    }

    /// A stored policy, namespace or node pool (`policy`, `namespace`, `pool`).
    pub fn entity(&self, kind: &str, name: &str) -> Option<Value> {
        let s = self.state.lock().unwrap();
        match kind {
            "policy" => s.policies.get(name).cloned(),
            "namespace" => s.namespaces.get(name).cloned(),
            "pool" => s.pools.get(name).cloned(),
            _ => None,
        }
    }

    /// Stored host volumes.
    pub fn volumes(&self) -> Vec<Value> {
        self.state.lock().unwrap().volumes.clone()
    }

    /// Every request seen, as (method, path) pairs.
    pub fn requests(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().requests.clone()
    }

    /// The `X-Nomad-Token` each request carried, in order (empty: none).
    pub fn tokens(&self) -> Vec<String> {
        self.state.lock().unwrap().tokens.clone()
    }

    /// The JWTs presented to `POST /v1/acl/login`. A login answers with the
    /// ACL token `acl:<jwt>`.
    pub fn logins(&self) -> Vec<String> {
        self.state.lock().unwrap().logins.clone()
    }

    /// Every request body seen, for leak checks.
    fn handle(&self, req: &Request) -> (u16, Value) {
        let mut s = self.state.lock().unwrap();
        s.requests.push((req.method.into(), req.path.clone()));
        let token = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("X-Nomad-Token"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        s.tokens.push(token);
        let (path, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
        let q = |k: &str| -> Option<String> {
            query
                .split('&')
                .find_map(|kv| kv.strip_prefix(k)?.strip_prefix('='))
                .map(|v| {
                    percent_encoding::percent_decode_str(v)
                        .decode_utf8_lossy()
                        .into_owned()
                })
        };
        let ns = q("namespace").unwrap_or_else(|| "default".into());
        let body: Value = req
            .body
            .as_ref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null);
        let parts: Vec<String> = path
            .trim_start_matches("/v1/")
            .split('/')
            .map(|p| {
                percent_encoding::percent_decode_str(p)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        // Reads leave the index alone, as Nomad's do: a blocking query
        // returns only once something was written.
        if req.method != "GET" {
            s.index += 1;
        }
        let index = s.index;
        let not_found = (404, Value::String("not found".into()));

        match (req.method, parts.as_slice()) {
            ("GET", ["status", "leader"]) => (200, json!("127.0.0.1:4647")),
            // The fleet-wide lists a steward long polls: the fake keeps no
            // allocations or deployments of its own beyond each job's.
            ("GET", ["allocations"]) | ("GET", ["deployments"]) => (200, json!([])),
            ("POST", ["jobs", "parse"]) => {
                // The fake's "HCL" is JSON; `${var.name}` takes a string
                // variable's value from the var file.
                let mut hcl = body["JobHCL"].as_str().unwrap_or("").to_owned();
                for line in body["Variables"].as_str().unwrap_or("").lines() {
                    if let Some((name, value)) = line.split_once(" = ")
                        && let Ok(Value::String(v)) = serde_json::from_str::<Value>(value)
                    {
                        hcl = hcl.replace(&format!("${{var.{name}}}"), &v.replace("$${", "${"));
                    }
                }
                let Ok(mut job) = serde_json::from_str::<Value>(&hcl) else {
                    return (400, Value::String("error parsing job".into()));
                };
                if job["Namespace"].is_null() {
                    job["Namespace"] = json!("default");
                }
                (200, job)
            }
            ("GET", ["jobs"]) => {
                let list = s
                    .jobs
                    .values()
                    .map(|j| json!({"ID": j["ID"], "Namespace": j["Namespace"], "Meta": j["Meta"], "Stop": j["Stop"]}))
                    .collect();
                (200, Value::Array(list))
            }
            ("POST", ["jobs"]) => {
                let job = body["Job"].clone();
                let key = (
                    ns.clone(),
                    job["ID"].as_str().unwrap_or_default().to_owned(),
                );
                let current = s
                    .jobs
                    .get(&key)
                    .and_then(|j| j["JobModifyIndex"].as_u64())
                    .unwrap_or(0);
                if body["EnforceIndex"].as_bool() == Some(true)
                    && body["JobModifyIndex"].as_u64() != Some(current)
                {
                    return (
                        400,
                        Value::String(format!(
                            "Enforcing job modify index {}: job exists with conflicting job modify index: {current}",
                            body["JobModifyIndex"]
                        )),
                    );
                }
                let version = s
                    .jobs
                    .get(&key)
                    .map(|j| j["Version"].as_u64().unwrap_or(0) + 1)
                    .unwrap_or(0);
                let mut stored = job;
                stored["Version"] = json!(version);
                stored["JobModifyIndex"] = json!(index);
                stored["Stop"] = json!(false);
                if !body["Submission"].is_null() {
                    s.submissions.insert(
                        (key.0.clone(), key.1.clone(), version),
                        body["Submission"].clone(),
                    );
                }
                s.jobs.insert(key, stored);
                (
                    200,
                    json!({"EvalID": format!("eval-{index}"), "JobModifyIndex": index}),
                )
            }
            ("GET", ["evaluation", "eval-blocked"]) => match s.blocked_polls {
                Some(n) if n > 0 => {
                    s.blocked_polls = Some(n.saturating_sub(usize::from(n != usize::MAX)));
                    (200, json!({"Status": "blocked"}))
                }
                _ => {
                    s.blocked_polls = None;
                    (200, json!({"Status": "complete"}))
                }
            },
            ("GET", ["evaluation", _]) if s.blocked_polls.is_some() => (
                200,
                json!({
                    "Status": "complete",
                    "BlockedEval": "eval-blocked",
                    "FailedTGAllocs": {"api": {"NodesEvaluated": 1, "DimensionExhausted": {"network: port collision": 1}}},
                }),
            ),
            ("GET", ["evaluation", _]) => (200, json!({"Status": "complete"})),
            ("POST", ["job", id, "plan"]) => {
                let live = s.jobs.get(&(ns.clone(), (*id).to_owned()));
                let comparable = |j: &Value| {
                    let mut j = j.clone();
                    if let Some(m) = j.as_object_mut() {
                        for k in ["Version", "JobModifyIndex", "Stop"] {
                            m.remove(k);
                        }
                    }
                    j
                };
                let (ty, modify) = match live {
                    None => ("Added", 0),
                    Some(l)
                        if comparable(l) == comparable(&body["Job"])
                            && l["Stop"] != json!(true) =>
                    {
                        ("None", l["JobModifyIndex"].as_u64().unwrap_or(0))
                    }
                    Some(l) => ("Edited", l["JobModifyIndex"].as_u64().unwrap_or(0)),
                };
                (
                    200,
                    json!({
                        "Diff": {"Type": ty, "ID": id, "Fields": [], "Objects": [], "TaskGroups": []},
                        "JobModifyIndex": modify,
                        "Annotations": {"DesiredTGUpdates": {"api": {"Place": u64::from(ty == "Added"), "Ignore": u64::from(ty == "None")}}},
                    }),
                )
            }
            ("GET", ["job", id, "submission"]) => {
                let version = q("version")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0);
                match s.submissions.get(&(ns.clone(), (*id).to_owned(), version)) {
                    Some(sub) => (200, sub.clone()),
                    None => not_found,
                }
            }
            ("GET", ["job", id, "deployment"]) => match s.jobs.get(&(ns.clone(), (*id).to_owned()))
            {
                Some(j) => (
                    200,
                    json!({
                        "Status": if s.held.contains(&(ns.clone(), (*id).to_owned())) { "running" } else { s.outcome.as_str() },
                        "JobModifyIndex": j["JobModifyIndex"],
                        "JobVersion": j["Version"],
                        "TaskGroups": {"api": {"HealthyAllocs": 1, "DesiredTotal": 1}},
                    }),
                ),
                None => (200, Value::Null),
            },
            ("GET", ["job", id, "allocations"]) => {
                let n = s
                    .allocs
                    .get(&(ns.clone(), (*id).to_owned()))
                    .copied()
                    .unwrap_or(1);
                match s.jobs.get(&(ns.clone(), (*id).to_owned())) {
                    Some(j) if j["Stop"] != json!(true) => {
                        let group = j["TaskGroups"][0]["Name"].as_str().unwrap_or("api");
                        (
                            200,
                            Value::Array(
                                (1..=n)
                                    .map(|i| json!({"ID": format!("alloc-{i}"), "TaskGroup": group, "JobVersion": j["Version"], "DesiredStatus": "run", "ClientStatus": "running"}))
                                    .collect(),
                            ),
                        )
                    }
                    _ => (200, json!([])),
                }
            }
            ("GET", ["allocation", _, "checks"]) => (200, Value::Null),
            ("POST", ["client", "allocation", _, "restart"]) => (200, json!({})),
            ("POST", ["job", id, "dispatch"]) => {
                match s.jobs.get(&(ns.clone(), (*id).to_owned())) {
                    Some(_) => (
                        200,
                        json!({"DispatchedJobID": format!("{id}/dispatch-{index}"), "EvalID": format!("eval-{index}")}),
                    ),
                    None => not_found,
                }
            }
            ("GET", ["job", id]) => match s.jobs.get(&(ns.clone(), (*id).to_owned())) {
                Some(j) => (200, j.clone()),
                None => not_found,
            },
            ("DELETE", ["job", id]) => {
                let key = (ns.clone(), (*id).to_owned());
                if q("purge").as_deref() == Some("true") {
                    s.jobs.remove(&key);
                } else if let Some(j) = s.jobs.get_mut(&key) {
                    j["Stop"] = json!(true);
                }
                (200, json!({"EvalID": "eval-stop"}))
            }
            ("GET", ["var", rest @ ..]) => match s.vars.get(&(ns.clone(), rest.join("/"))) {
                Some((items, mi)) => (
                    200,
                    json!({"Namespace": ns, "Path": rest.join("/"), "Items": items, "ModifyIndex": mi}),
                ),
                None => not_found,
            },
            ("PUT", ["var", rest @ ..]) => {
                let key = (ns.clone(), rest.join("/"));
                let current = s.vars.get(&key).map(|(_, i)| *i).unwrap_or(0);
                if let Some(cas) = q("cas")
                    && cas.parse::<u64>().ok() != Some(current)
                {
                    let items = s.vars.get(&key).map(|(i, _)| i.clone()).unwrap_or_default();
                    return (409, json!({"Items": items, "ModifyIndex": current}));
                }
                let items: BTreeMap<String, String> =
                    serde_json::from_value(body["Items"].clone()).unwrap_or_default();
                s.vars.insert(key, (items, index));
                (200, json!({"ModifyIndex": index}))
            }
            ("DELETE", ["var", rest @ ..]) => {
                s.vars.remove(&(ns.clone(), rest.join("/")));
                (200, Value::Null)
            }
            ("GET", ["acl", "policies"]) => {
                (200, Value::Array(s.policies.values().cloned().collect()))
            }
            ("GET", ["acl", "policy", name]) => match s.policies.get(*name) {
                Some(p) => (200, p.clone()),
                None => not_found,
            },
            ("POST", ["acl", "policy", name]) => {
                let mut p = body.clone();
                p["ModifyIndex"] = json!(index);
                s.policies.insert((*name).to_owned(), p);
                (200, Value::Null)
            }
            ("DELETE", ["acl", "policy", name]) => {
                s.policies.remove(*name);
                (200, Value::Null)
            }
            ("GET", ["namespaces"]) => {
                (200, Value::Array(s.namespaces.values().cloned().collect()))
            }
            ("GET", ["namespace", name]) => match s.namespaces.get(*name) {
                Some(n) => (200, n.clone()),
                None => not_found,
            },
            ("POST", ["namespace", name]) => {
                let mut n = body.clone();
                n["ModifyIndex"] = json!(index);
                s.namespaces.insert((*name).to_owned(), n);
                (200, Value::Null)
            }
            ("DELETE", ["namespace", name]) => {
                s.namespaces.remove(*name);
                (200, Value::Null)
            }
            ("GET", ["node", "pools"]) => (200, Value::Array(s.pools.values().cloned().collect())),
            ("GET", ["node", "pool", name]) => match s.pools.get(*name) {
                Some(p) => (200, p.clone()),
                None => not_found,
            },
            ("PUT", ["node", "pools"]) => {
                let mut p = body.clone();
                p["ModifyIndex"] = json!(index);
                let name = p["Name"].as_str().unwrap_or_default().to_owned();
                s.pools.insert(name, p);
                (200, Value::Null)
            }
            ("DELETE", ["node", "pool", name]) => {
                s.pools.remove(*name);
                (200, Value::Null)
            }
            ("GET", ["volumes"]) => (
                200,
                Value::Array(
                    s.volumes
                        .iter()
                        .filter(|v| v["Namespace"] == json!(ns))
                        .cloned()
                        .collect(),
                ),
            ),
            ("PUT", ["volume", "host", "create"]) => {
                let mut v = body["Volume"].clone();
                v["ID"] = json!(format!("vol-{index}"));
                v["Allocations"] = json!([]);
                s.volumes.push(v.clone());
                (200, json!({"Volume": v}))
            }
            ("GET", ["volume", "host", id]) => {
                match s.volumes.iter().find(|v| v["ID"] == json!(id)) {
                    Some(v) => (200, v.clone()),
                    None => not_found,
                }
            }
            ("DELETE", ["volume", "host", id]) => {
                s.volumes.retain(|v| v["ID"] != json!(id));
                (200, Value::Null)
            }
            ("POST", ["acl", "login"]) => {
                let jwt = body["LoginToken"].as_str().unwrap_or_default().to_owned();
                s.logins.push(jwt.clone());
                (
                    200,
                    json!({
                        "AccessorID": format!("accessor-{index}"),
                        "SecretID": format!("acl:{jwt}"),
                        "ExpirationTime": (chrono::Utc::now() + chrono::Duration::minutes(15)).to_rfc3339(),
                    }),
                )
            }
            ("GET", ["quotas"]) => not_found,
            _ => not_found,
        }
    }
}

/// A task action's socket: the frames the fake answers with, in order.
struct FakeSocket {
    frames: std::collections::VecDeque<String>,
}

#[async_trait]
impl crate::transport::Socket for FakeSocket {
    async fn send(&mut self, _text: String) -> buckle_core::Result<()> {
        Ok(())
    }

    async fn recv(&mut self) -> buckle_core::Result<Option<String>> {
        Ok(self.frames.pop_front())
    }
}

#[async_trait]
impl Transport for FakeNomad {
    /// `GET /v1/job/:id/action`: the action's output on stdout (base64, as
    /// Nomad frames it), then its exit; a 404 for an unknown job or an
    /// allocation it does not run.
    async fn socket(
        &self,
        request: Request,
    ) -> buckle_core::Result<Box<dyn crate::transport::Socket>> {
        use base64::Engine as _;
        tokio::task::yield_now().await;
        let mut s = self.state.lock().unwrap();
        s.requests.push(("WS".into(), request.path.clone()));
        let ns = query_value(&request.path, "namespace").unwrap_or_else(|| "default".into());
        let action = query_value(&request.path, "action").unwrap_or_default();
        let alloc = query_value(&request.path, "allocID").unwrap_or_default();
        let id = request
            .path
            .split('?')
            .next()
            .and_then(|p| p.strip_prefix("/v1/job/"))
            .and_then(|p| p.strip_suffix("/action"))
            .map(|p| {
                percent_encoding::percent_decode_str(p)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        let runs = s
            .allocs
            .get(&(ns.clone(), id.clone()))
            .copied()
            .unwrap_or(1);
        let known = s.jobs.contains_key(&(ns, id.clone()))
            && (1..=runs).any(|i| alloc == format!("alloc-{i}"));
        if !known {
            return Err(buckle_core::error::user(
                format!("Nomad refused the WebSocket (404): job {id} runs no allocation {alloc}"),
                &[],
            ));
        }
        let (output, code) = s
            .task_actions
            .get(&action)
            .cloned()
            .unwrap_or_else(|| (format!("ran {action} in {alloc}\n"), 0));
        let data = base64::engine::general_purpose::STANDARD.encode(output);
        Ok(Box::new(FakeSocket {
            frames: [
                json!({"stdout": {"data": data}}).to_string(),
                "{}".to_owned(),
                json!({"exited": true, "result": {"exit_code": code}}).to_string(),
            ]
            .into(),
        }))
    }

    async fn send(&self, request: Request) -> buckle_core::Result<Response> {
        // Let concurrent callers interleave, as they would over a network.
        tokio::task::yield_now().await;
        // A blocking query (`?index=N&wait=D`) waits for a write past N, or
        // for `wait` (default 5m), before answering.
        if request.method == "GET"
            && let Some(min_index) =
                query_value(&request.path, "index").and_then(|v| v.parse::<u64>().ok())
        {
            let wait = query_value(&request.path, "wait")
                .and_then(|w| parse_wait(&w))
                .unwrap_or(std::time::Duration::from_secs(300));
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                let notified = self.changed.notified();
                if self.state.lock().unwrap().index > min_index {
                    break;
                }
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep_until(deadline) => break,
                }
            }
        }
        let (status, body) = self.handle(&request);
        if request.method != "GET" {
            self.changed.notify_waiters();
        }
        let index = self.state.lock().unwrap().index.max(1);
        Ok(Response {
            status,
            headers: vec![("X-Nomad-Index".into(), index.to_string())],
            body: match body {
                Value::String(s) if status >= 400 => s.into_bytes(),
                other => serde_json::to_vec(&other).unwrap_or_default(),
            },
        })
    }
}

fn query_value(path: &str, key: &str) -> Option<String> {
    let (_, query) = path.split_once('?')?;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key)?.strip_prefix('='))
        .map(|v| {
            percent_encoding::percent_decode_str(v)
                .decode_utf8_lossy()
                .into_owned()
        })
}

/// Nomad's `wait` syntax: `50s`, `5m`, `1500ms`.
fn parse_wait(w: &str) -> Option<std::time::Duration> {
    let (digits, unit) = w
        .trim()
        .split_at(w.trim().find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = digits.parse().ok()?;
    Some(match unit {
        "ms" => std::time::Duration::from_millis(n),
        "s" => std::time::Duration::from_secs(n),
        "m" => std::time::Duration::from_secs(n * 60),
        "h" => std::time::Duration::from_secs(n * 3600),
        _ => return None,
    })
}

/// A wiremock server and a client pointed at it, for client-level tests.
#[cfg(test)]
pub(crate) async fn mock() -> (wiremock::MockServer, crate::Nomad) {
    let server = wiremock::MockServer::start().await;
    let nomad = crate::Nomad::connect(&server.uri(), None).unwrap();
    (server, nomad)
}
