//! An in-memory Kubernetes API for tests: discovery, objects, server-side
//! apply with field ownership (simplified: lists of objects merge by
//! `name`), dry runs, resourceVersion preconditions, merge patches and
//! label-selected lists. Controllers are simulated: an applied Deployment,
//! StatefulSet or DaemonSet reports its rollout as
//! [`FakeKube::rollout_outcome`] says.

use crate::transport::{Request, Response, Transport};
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// (group/version, kind, plural, namespaced) served from the start.
const BUILTIN: &[(&str, &str, &str, bool)] = &[
    ("v1", "Namespace", "namespaces", false),
    ("v1", "ConfigMap", "configmaps", true),
    ("v1", "Secret", "secrets", true),
    ("v1", "Service", "services", true),
    ("v1", "Pod", "pods", true),
    (
        "v1",
        "PersistentVolumeClaim",
        "persistentvolumeclaims",
        true,
    ),
    ("v1", "ServiceAccount", "serviceaccounts", true),
    ("apps/v1", "Deployment", "deployments", true),
    ("apps/v1", "StatefulSet", "statefulsets", true),
    ("apps/v1", "DaemonSet", "daemonsets", true),
    ("apps/v1", "ReplicaSet", "replicasets", true),
    ("batch/v1", "Job", "jobs", true),
    (
        "apiextensions.k8s.io/v1",
        "CustomResourceDefinition",
        "customresourcedefinitions",
        false,
    ),
    (
        "rbac.authorization.k8s.io/v1",
        "ClusterRoleBinding",
        "clusterrolebindings",
        false,
    ),
];

#[derive(Debug, Clone)]
struct Resource {
    gv: String,
    kind: String,
    plural: String,
    namespaced: bool,
}

#[derive(Debug, Default)]
struct State {
    version: u64,
    resources: Vec<Resource>,
    /// Objects by (gv, plural, namespace, name).
    objects: BTreeMap<(String, String, String, String), Value>,
    outcome: String,
    /// Status writes still to make before an apply.
    status_writes: usize,
    requests: Vec<(String, String)>,
    authorization: Vec<String>,
}

type Key = (String, String, String, String);

impl State {
    fn key_of(&self, api_version: &str, kind: &str, ns: &str, name: &str) -> Key {
        let r = self
            .resources
            .iter()
            .find(|r| r.gv == api_version && r.kind == kind)
            .expect("served kind");
        let ns = if r.namespaced { ns } else { "" };
        (r.gv.clone(), r.plural.clone(), ns.into(), name.into())
    }
}

fn write_status(s: &mut State, key: &Key, status: &Value) {
    s.version += 1;
    let rv = s.version.to_string();
    let obj = s.objects.get_mut(key).expect("object exists");
    merge(&mut obj["status"], status);
    obj["metadata"]["resourceVersion"] = json!(rv);
}

/// The fake. Cheap to clone; clones share state.
#[derive(Debug, Clone)]
pub struct FakeKube {
    state: Arc<Mutex<State>>,
    /// Woken by every write, for watch streams.
    changed: Arc<tokio::sync::Notify>,
}

impl Default for FakeKube {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeKube {
    /// A cluster with the `default` namespace.
    pub fn new() -> Self {
        let state = State {
            outcome: "complete".into(),
            resources: BUILTIN
                .iter()
                .map(|(gv, kind, plural, ns)| Resource {
                    gv: (*gv).into(),
                    kind: (*kind).into(),
                    plural: (*plural).into(),
                    namespaced: *ns,
                })
                .collect(),
            ..Default::default()
        };
        let fake = Self {
            state: Arc::new(Mutex::new(state)),
            changed: Arc::new(tokio::sync::Notify::new()),
        };
        fake.insert(
            json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "default"}}),
        );
        fake
    }

    /// A client over this fake.
    pub fn client(&self) -> crate::Kube {
        crate::Kube::with_transport(Arc::new(self.clone()))
    }

    /// How rollouts end: `complete`, `stuck` (progress deadline exceeded),
    /// `crashloop` (complete, but a pod is in CrashLoopBackOff), `pending`,
    /// `unschedulable` (pending, with a pod no node fits).
    pub fn rollout_outcome(&self, outcome: &str) {
        self.state.lock().unwrap().outcome = outcome.into();
    }

    /// Every request, as (method, path).
    pub fn requests(&self) -> Vec<(String, String)> {
        self.state.lock().unwrap().requests.clone()
    }

    /// The `Authorization` header of each request (empty when none).
    pub fn authorizations(&self) -> Vec<String> {
        self.state.lock().unwrap().authorization.clone()
    }

    /// A stored object.
    pub fn object(&self, api_version: &str, kind: &str, ns: &str, name: &str) -> Option<Value> {
        let s = self.state.lock().unwrap();
        let r = s
            .resources
            .iter()
            .find(|r| r.gv == api_version && r.kind == kind)?;
        let ns = if r.namespaced { ns } else { "" };
        s.objects
            .get(&(r.gv.clone(), r.plural.clone(), ns.into(), name.into()))
            .cloned()
    }

    /// Stores an object as another tool would (no Buckle ownership).
    pub fn insert(&self, obj: Value) {
        let mut s = self.state.lock().unwrap();
        s.version += 1;
        let rv = s.version.to_string();
        let Some(r) = s
            .resources
            .iter()
            .find(|r| {
                Some(r.gv.as_str()) == obj["apiVersion"].as_str()
                    && Some(r.kind.as_str()) == obj["kind"].as_str()
            })
            .cloned()
        else {
            panic!("the fake does not serve {}", obj["kind"]);
        };
        let mut obj = obj;
        obj["metadata"]["resourceVersion"] = json!(rv);
        obj["metadata"]["uid"] = json!(format!("uid-{rv}"));
        if obj["metadata"]["generation"].is_null() {
            obj["metadata"]["generation"] = json!(1);
        }
        let ns = if r.namespaced {
            obj["metadata"]["namespace"]
                .as_str()
                .unwrap_or("default")
                .to_owned()
        } else {
            String::new()
        };
        let name = obj["metadata"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        s.objects.insert((r.gv, r.plural, ns, name), obj);
        self.changed.notify_waiters();
    }

    /// Edits a stored object out of band, as `manager` (`kubectl edit`):
    /// the fields it touches move to that manager.
    pub fn edit(
        &self,
        api_version: &str,
        kind: &str,
        ns: &str,
        name: &str,
        manager: &str,
        patch: Value,
    ) {
        let mut s = self.state.lock().unwrap();
        s.version += 1;
        let rv = s.version.to_string();
        let r = s
            .resources
            .iter()
            .find(|r| r.gv == api_version && r.kind == kind)
            .cloned()
            .expect("served kind");
        let ns = if r.namespaced { ns } else { "" };
        let key = (r.gv, r.plural, ns.to_owned(), name.to_owned());
        let obj = s.objects.get_mut(&key).expect("object exists");
        let set = fieldset(&patch);
        merge(obj, &patch);
        obj["metadata"]["resourceVersion"] = json!(rv);
        bump_generation(obj);
        let mut managed: Vec<Value> = obj["metadata"]["managedFields"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for m in managed.iter_mut() {
            subtract(&mut m["fieldsV1"], &set);
        }
        managed.push(json!({"manager": manager, "operation": "Update", "fieldsV1": set}));
        obj["metadata"]["managedFields"] = Value::Array(managed);
        self.changed.notify_waiters();
    }

    /// Writes a controller's status, as the status subresource does: the
    /// resourceVersion moves, the spec, generation and field owners do not.
    pub fn write_status(&self, api_version: &str, kind: &str, ns: &str, name: &str, status: Value) {
        let mut s = self.state.lock().unwrap();
        let key = s.key_of(api_version, kind, ns, name);
        write_status(&mut s, &key, &status);
        self.changed.notify_waiters();
    }

    /// Has a controller write status to the object just before each of the
    /// next `n` server-side applies (dry runs included) under a
    /// resourceVersion, so each one's precondition fails (a Deployment
    /// reconciling while Buckle plans or applies).
    pub fn status_writes_before_apply(&self, n: usize) {
        self.state.lock().unwrap().status_writes = n;
    }

    fn handle(&self, req: &Request) -> Response {
        let mut s = self.state.lock().unwrap();
        s.requests.push((req.method.to_owned(), req.path.clone()));
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        s.authorization.push(auth);
        let (path, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
        let q: BTreeMap<&str, String> = query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| {
                (
                    k,
                    percent_encoding::percent_decode_str(v)
                        .decode_utf8_lossy()
                        .into_owned(),
                )
            })
            .collect();
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        // Discovery.
        let gv_len = if parts.first() == Some(&"api") { 2 } else { 3 };
        if parts.len() == gv_len {
            let gv = parts[1..].join("/");
            let resources: Vec<Value> = s
                .resources
                .iter()
                .filter(|r| r.gv == gv)
                .map(|r| json!({"name": r.plural, "kind": r.kind, "namespaced": r.namespaced}))
                .collect();
            if resources.is_empty() {
                return status(
                    404,
                    "NotFound",
                    "the server could not find the requested resource",
                );
            }
            return ok(json!({"groupVersion": gv, "resources": resources}));
        }
        let gv = parts[1..gv_len].join("/");
        let rest = &parts[gv_len..];
        let (ns, rest) = if rest.first() == Some(&"namespaces") && rest.len() >= 3 {
            (percent(rest[1]), &rest[2..])
        } else {
            (String::new(), rest)
        };
        let Some(r) = s
            .resources
            .iter()
            .find(|r| r.gv == gv && Some(&r.plural.as_str()) == rest.first())
            .cloned()
        else {
            return status(404, "NotFound", "no such resource type");
        };
        let name = rest.get(1).map(|n| percent(n));
        let key = |name: &str| (r.gv.clone(), r.plural.clone(), ns.clone(), name.to_owned());
        let body: Value = req
            .body
            .as_ref()
            .and_then(|b| serde_json::from_slice(b).ok())
            .unwrap_or(Value::Null);

        match (req.method, name) {
            ("GET", None) => {
                let selector = q.get("labelSelector").cloned().unwrap_or_default();
                let items: Vec<Value> = s
                    .objects
                    .iter()
                    .filter(|((g, p, n, _), _)| {
                        g == &r.gv && p == &r.plural && (ns.is_empty() || n == &ns)
                    })
                    .filter(|(_, o)| selected(o, &selector))
                    .map(|(_, o)| o.clone())
                    .collect();
                ok(json!({"items": items}))
            }
            ("GET", Some(name)) => match s.objects.get(&key(&name)) {
                Some(o) => ok(o.clone()),
                None => status(404, "NotFound", "not found"),
            },
            ("DELETE", Some(name)) => match s.objects.remove(&key(&name)) {
                Some(_) => ok(json!({"status": "Success"})),
                None => status(404, "NotFound", "not found"),
            },
            ("POST", None) => {
                let name = body["metadata"]["name"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                if s.objects.contains_key(&key(&name)) {
                    return status(409, "AlreadyExists", "already exists");
                }
                s.version += 1;
                let mut obj = body;
                obj["metadata"]["resourceVersion"] = json!(s.version.to_string());
                s.objects.insert(key(&name), obj.clone());
                ok(obj)
            }
            ("PUT", Some(name)) => {
                let Some(live) = s.objects.get(&key(&name)) else {
                    return status(404, "NotFound", "not found");
                };
                if body["metadata"]["resourceVersion"] != live["metadata"]["resourceVersion"] {
                    return status(409, "Conflict", "the object has been modified");
                }
                s.version += 1;
                let mut obj = body;
                obj["metadata"]["resourceVersion"] = json!(s.version.to_string());
                s.objects.insert(key(&name), obj.clone());
                ok(obj)
            }
            ("PATCH", Some(name)) if req.content_type == Some(crate::client::MERGE_PATCH) => {
                s.version += 1;
                let rv = s.version.to_string();
                let Some(obj) = s.objects.get_mut(&key(&name)) else {
                    return status(404, "NotFound", "not found");
                };
                merge(obj, &body);
                obj["metadata"]["resourceVersion"] = json!(rv);
                bump_generation(obj);
                ok(obj.clone())
            }
            ("PATCH", Some(name)) => {
                let dry = q.contains_key("dryRun");
                if r.namespaced
                    && !s.objects.contains_key(&(
                        "v1".into(),
                        "namespaces".into(),
                        String::new(),
                        ns.clone(),
                    ))
                {
                    return status(404, "NotFound", &format!("namespaces \"{ns}\" not found"));
                }
                let manager = q.get("fieldManager").cloned().unwrap_or_default();
                if s.status_writes > 0
                    && body["metadata"]["resourceVersion"].is_string()
                    && s.objects.contains_key(&key(&name))
                {
                    s.status_writes -= 1;
                    let n = s.status_writes;
                    write_status(&mut s, &key(&name), &json!({"reconciles": n}));
                }
                let live = s.objects.get(&key(&name)).cloned();
                if let Some(rv) = body["metadata"]["resourceVersion"].as_str()
                    && live
                        .as_ref()
                        .and_then(|l| l["metadata"]["resourceVersion"].as_str())
                        != Some(rv)
                {
                    return status(
                        409,
                        "Conflict",
                        "the object has been modified; please apply your changes to the latest version",
                    );
                }
                let mut intent = body.clone();
                if let Some(m) = intent["metadata"].as_object_mut() {
                    m.remove("resourceVersion");
                }
                let new_set = fieldset(&intent);
                let mut obj = live.clone().unwrap_or_else(|| json!({}));
                let mut managed: Vec<Value> = obj["metadata"]["managedFields"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                let old_set = managed
                    .iter()
                    .find(|m| m["manager"].as_str() == Some(&manager) && m["operation"] == "Apply")
                    .map(|m| m["fieldsV1"].clone())
                    .unwrap_or(json!({}));
                prune(&mut obj, &old_set, &new_set);
                merge(&mut obj, &intent);
                defaults(&mut obj);
                managed.retain(|m| {
                    !(m["manager"].as_str() == Some(&manager) && m["operation"] == "Apply")
                });
                for m in managed.iter_mut() {
                    subtract(&mut m["fieldsV1"], &new_set);
                }
                managed.push(json!({"manager": manager, "operation": "Apply", "apiVersion": r.gv, "fieldsV1": new_set}));
                obj["metadata"]["managedFields"] = Value::Array(managed);
                let changed = live.as_ref().is_none_or(|l| {
                    let mut a = l.clone();
                    let mut b = obj.clone();
                    a["metadata"]
                        .as_object_mut()
                        .map(|m| m.remove("managedFields"));
                    b["metadata"]
                        .as_object_mut()
                        .map(|m| m.remove("managedFields"));
                    a != b
                });
                if live.is_none() {
                    obj["metadata"]["generation"] = json!(1);
                    obj["metadata"]["uid"] = json!(format!("uid-{name}"));
                } else if changed && live.as_ref().map(|l| &l["spec"]) != Some(&obj["spec"]) {
                    bump_generation(&mut obj);
                }
                if dry {
                    return ok(obj);
                }
                if changed {
                    s.version += 1;
                    obj["metadata"]["resourceVersion"] = json!(s.version.to_string());
                }
                let outcome = s.outcome.clone();
                simulate(&mut obj, &outcome);
                if r.kind == "CustomResourceDefinition" {
                    obj["status"] =
                        json!({"conditions": [{"type": "Established", "status": "True"}]});
                    let group = obj["spec"]["group"].as_str().unwrap_or_default().to_owned();
                    let kind = obj["spec"]["names"]["kind"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let plural = obj["spec"]["names"]["plural"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned();
                    let namespaced = obj["spec"]["scope"].as_str() != Some("Cluster");
                    for v in obj["spec"]["versions"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                    {
                        let gv = format!("{group}/{}", v["name"].as_str().unwrap_or("v1"));
                        if !s.resources.iter().any(|x| x.gv == gv && x.kind == kind) {
                            s.resources.push(Resource {
                                gv,
                                kind: kind.clone(),
                                plural: plural.clone(),
                                namespaced,
                            });
                        }
                    }
                }
                let pod_status = match outcome.as_str() {
                    "crashloop" => Some(
                        json!({"containerStatuses": [{"name": "app", "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}),
                    ),
                    "unschedulable" => Some(
                        json!({"phase": "Pending", "conditions": [{"type": "PodScheduled", "status": "False", "reason": "Unschedulable", "message": "0/1 nodes are available: 1 Insufficient memory."}]}),
                    ),
                    _ => None,
                };
                if let Some(pod_status) = pod_status.filter(|_| r.kind == "Deployment") {
                    let labels = obj["spec"]["selector"]["matchLabels"].clone();
                    s.version += 1;
                    let pod = json!({"apiVersion": "v1", "kind": "Pod",
                        "metadata": {"name": format!("{name}-abc"), "namespace": ns, "labels": labels},
                        "status": pod_status});
                    s.objects.insert(
                        (
                            "v1".into(),
                            "pods".into(),
                            ns.clone(),
                            format!("{name}-abc"),
                        ),
                        pod,
                    );
                }
                s.objects.insert(key(&name), obj.clone());
                ok(obj)
            }
            _ => status(405, "MethodNotAllowed", "not supported by the fake"),
        }
    }
}

#[async_trait]
impl Transport for FakeKube {
    async fn send(&self, request: Request) -> buckle_core::Result<Response> {
        let response = self.handle(&request);
        if request.method != "GET" {
            self.changed.notify_waiters();
        }
        Ok(response)
    }

    /// A watch: one `MODIFIED` event (of the collection's first object, or
    /// an empty object) per write to the fake, as newline-delimited JSON,
    /// until the stream is dropped. What changed is not filtered: a watcher
    /// re-reads what it cares about.
    async fn stream(&self, request: Request) -> buckle_core::Result<crate::transport::ByteStream> {
        if !request.path.contains("watch=true") {
            return Err(buckle_core::error::system(
                "the fake streams watches only",
                &[],
            ));
        }
        self.state
            .lock()
            .unwrap()
            .requests
            .push((request.method.to_owned(), request.path.clone()));
        let fake = self.clone();
        let seen = fake.state.lock().unwrap().version;
        Ok(Box::pin(futures::stream::unfold(
            (fake, seen),
            |(fake, mut seen)| async move {
                loop {
                    let version = {
                        let notified = fake.changed.notified();
                        let version = fake.state.lock().unwrap().version;
                        if version == seen {
                            notified.await;
                            continue;
                        }
                        version
                    };
                    seen = version;
                    let event = json!({"type": "MODIFIED", "object": {"metadata": {"resourceVersion": version.to_string()}}});
                    let mut line = serde_json::to_vec(&event).unwrap();
                    line.push(b'\n');
                    return Some((Ok(bytes::Bytes::from(line)), (fake, seen)));
                }
            },
        )))
    }
}

fn percent(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn ok(v: Value) -> Response {
    Response {
        status: 200,
        body: serde_json::to_vec(&v).unwrap(),
    }
}

fn status(code: u16, reason: &str, message: &str) -> Response {
    Response {
        status: code,
        body: serde_json::to_vec(
            &json!({"kind": "Status", "reason": reason, "message": message, "code": code}),
        )
        .unwrap(),
    }
}

fn selected(obj: &Value, selector: &str) -> bool {
    selector
        .split(',')
        .filter(|s| !s.is_empty())
        .all(|kv| match kv.split_once('=') {
            Some((k, v)) => obj["metadata"]["labels"][k].as_str() == Some(v),
            None => false,
        })
}

fn bump_generation(obj: &mut Value) {
    let g = obj["metadata"]["generation"].as_i64().unwrap_or(0);
    obj["metadata"]["generation"] = json!(g + 1);
}

/// Server defaults Buckle never owns.
fn defaults(obj: &mut Value) {
    if obj["kind"] == "Deployment"
        && obj["spec"].is_object()
        && obj["spec"]["progressDeadlineSeconds"].is_null()
    {
        obj["spec"]["progressDeadlineSeconds"] = json!(600);
    }
    if obj["kind"] == "Secret" && obj["type"].is_null() {
        obj["type"] = json!("Opaque");
    }
}

fn simulate(obj: &mut Value, outcome: &str) {
    let kind = obj["kind"].as_str().unwrap_or_default().to_owned();
    if !matches!(kind.as_str(), "Deployment" | "StatefulSet" | "DaemonSet") {
        return;
    }
    let generation = obj["metadata"]["generation"].clone();
    let replicas = obj["spec"]["replicas"].as_i64().unwrap_or(1);
    obj["status"] = match outcome {
        "stuck" => {
            json!({"observedGeneration": generation, "replicas": replicas, "updatedReplicas": 0,
            "conditions": [{"type": "Progressing", "status": "False", "reason": "ProgressDeadlineExceeded", "message": "ReplicaSet has timed out progressing"}]})
        }
        "pending" | "unschedulable" => {
            json!({"observedGeneration": generation, "replicas": replicas, "updatedReplicas": 0})
        }
        _ => {
            json!({"observedGeneration": generation, "replicas": replicas, "updatedReplicas": replicas,
            "availableReplicas": replicas, "readyReplicas": replicas,
            "desiredNumberScheduled": replicas, "updatedNumberScheduled": replicas, "numberAvailable": replicas})
        }
    };
}

/// The field set an applied intent owns: objects field by field, lists of
/// named objects by name, anything else whole.
fn fieldset(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, child) in m {
                if k == "apiVersion" || k == "kind" {
                    continue;
                }
                out.insert(format!("f:{k}"), fieldset(child));
            }
            Value::Object(out)
        }
        Value::Array(items) if items.iter().all(|i| i["name"].is_string()) && !items.is_empty() => {
            let mut out = Map::new();
            for item in items {
                let mut sub = fieldset(item);
                sub.as_object_mut().map(|o| o.insert(".".into(), json!({})));
                out.insert(format!("k:{}", json!({"name": item["name"]})), sub);
            }
            Value::Object(out)
        }
        _ => json!({}),
    }
}

/// Removes from `set` the paths `other` covers.
fn subtract(set: &mut Value, other: &Value) {
    let (Some(s), Some(o)) = (set.as_object_mut(), other.as_object()) else {
        return;
    };
    for (k, sub) in o {
        let leaf = sub.as_object().is_none_or(|m| m.keys().all(|k| k == "."));
        if leaf {
            s.remove(k);
        } else if let Some(child) = s.get_mut(k) {
            subtract(child, sub);
        }
    }
}

/// Drops from `obj` the fields `old` owned that `new` does not.
fn prune(obj: &mut Value, old: &Value, new: &Value) {
    let Some(old) = old.as_object() else { return };
    let empty = Map::new();
    let new = new.as_object().unwrap_or(&empty);
    for (k, sub) in old {
        if k == "." {
            continue;
        }
        match new.get(k) {
            None => {
                if let Some(name) = k.strip_prefix("f:") {
                    obj.as_object_mut().map(|o| o.remove(name));
                } else if let Some(keys) = k.strip_prefix("k:") {
                    let want: Value = serde_json::from_str(keys).unwrap_or(Value::Null);
                    if let Some(items) = obj.as_array_mut() {
                        items.retain(|i| i["name"] != want["name"]);
                    }
                }
            }
            Some(nsub) => {
                if let Some(name) = k.strip_prefix("f:") {
                    if let Some(child) = obj.get_mut(name) {
                        prune(child, sub, nsub);
                    }
                } else if let Some(keys) = k.strip_prefix("k:") {
                    let want: Value = serde_json::from_str(keys).unwrap_or(Value::Null);
                    if let Some(item) = obj
                        .as_array_mut()
                        .and_then(|items| items.iter_mut().find(|i| i["name"] == want["name"]))
                    {
                        prune(item, sub, nsub);
                    }
                }
            }
        }
    }
}

/// Deep merge: objects field by field, lists of named objects by name.
fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    b.remove(k);
                    continue;
                }
                match b.get_mut(k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (Value::Array(b), Value::Array(p))
            if p.iter().all(|i| i["name"].is_string())
                && b.iter().all(|i| i["name"].is_string()) =>
        {
            let mut out = Vec::new();
            for item in p {
                let mut merged = b
                    .iter()
                    .find(|x| x["name"] == item["name"])
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                merge(&mut merged, item);
                out.push(merged);
            }
            for item in b.iter() {
                if !p.iter().any(|x| x["name"] == item["name"]) {
                    out.push(item.clone());
                }
            }
            *b = out;
        }
        (b, p) => *b = p.clone(),
    }
}
