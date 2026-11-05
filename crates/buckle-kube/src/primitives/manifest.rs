//! A manifest object, applied with server-side apply under the `buckle`
//! field manager.
//!
//! The plan is the API server's own: a dry-run apply returns the object as
//! it would be, and the fields Buckle owns (before or after) are compared
//! between the live object and that result, so defaults and other
//! controllers' fields never show as changes. The apply sends the planned
//! intent with the live `resourceVersion`. When the object moved after the
//! plan (a controller writing status does that constantly), the diff is
//! taken again: the apply goes ahead under the new `resourceVersion` when
//! the fields Buckle owns still show the planned change, and is refused
//! otherwise rather than overwriting a change it has not shown.

use super::{Context, Pending as StepPending, Step, yaml};
use crate::client::Applied as Apply;
use crate::fields;
use crate::health::{self, Rollout};
use crate::index::{self, Entry};
use crate::object::{self, ObjectKey, Owner};
use buckle_core::Result;
use buckle_core::error::user;
use buckle_core::fieldpath::{FieldPath, adopt_all};
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::Redacted;
use buckle_core::secrets::sinks::ApiSink;
use buckle_core::task::Applied;
use serde_json::{Value, json};

/// One object of a manifest file.
#[derive(Debug, Clone)]
pub struct ManifestSpec {
    /// The file it came from, for messages (`grafana.yaml`).
    pub file: String,
    pub object: Value,
    /// Fields another tool owns (`spec.replicas` under an autoscaler).
    pub ignore: Vec<FieldPath>,
}

impl ManifestSpec {
    pub fn new(file: impl Into<String>, object: Value) -> Self {
        Self {
            file: file.into(),
            object,
            ignore: Vec::new(),
        }
    }

    /// A ConfigMap holding `items` (a workload's `config` entry): plain
    /// values, shown in diffs as written.
    pub fn config_map(
        name: &str,
        namespace: Option<&str>,
        items: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        let data: serde_json::Map<String, Value> = items
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        let mut metadata = json!({"name": name});
        if let Some(ns) = namespace.filter(|n| !n.is_empty()) {
            metadata["namespace"] = json!(ns);
        }
        Self::new(
            format!("config {name}"),
            json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": metadata, "data": data}),
        )
    }

    /// The object's key as written (namespace not yet defaulted).
    pub fn key(&self) -> Option<ObjectKey> {
        ObjectKey::of(&self.object)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    /// The object's path; `None` when its type is served only after an
    /// earlier step (a CRD) applies.
    path: Option<String>,
    /// The intent as applied: stamped, with the live resourceVersion.
    body: Value,
    /// The owned-field diff an update was planned with, compared again
    /// when the object moved before the apply; `None` for a creation.
    planned: Option<OwnedDiff>,
    /// The spec's ignored fields, adopted again from the moved object.
    ignore: Vec<FieldPath>,
}

/// The fields Buckle owns on an object, live (`before`) and as the dry run
/// says they would be (`after`), as plans show them.
#[derive(Debug, Clone, PartialEq)]
struct OwnedDiff {
    before: Value,
    after: Value,
}

/// How often a plan or apply takes the diff again after the object moved:
/// a Deployment's controller can write status more than once while it
/// reconciles.
const CONFLICT_RETRIES: usize = 3;

/// A namespace a workload's `namespace` names, created when missing.
pub fn namespace(name: &str) -> ManifestSpec {
    ManifestSpec::new(
        "workload.yaml namespace",
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": name}}),
    )
}

pub(crate) async fn plan(cx: &Context, spec: &ManifestSpec, created: &[ObjectKey]) -> Result<Step> {
    let Some(mut key) = spec.key() else {
        return Err(user(
            format!(
                "An object of {} has no apiVersion, kind or name.",
                spec.file
            ),
            &["Every object needs apiVersion, kind and metadata.name."],
        ));
    };
    let mut intent = spec.object.clone();
    let resource = cx.kube.resource(&key.api_version, &key.kind).await?;
    let namespaced = match &resource {
        Some(r) => r.namespaced,
        // A custom resource whose CRD this apply creates: namespaced unless
        // the CRD says otherwise.
        None => crd_scope(created, &key).ok_or_else(|| {
            user(
                format!(
                    "The cluster does not serve {} {} ({} in {}).",
                    key.api_version, key.kind, key.name, spec.file
                ),
                &["Check the apiVersion and kind, or list the CustomResourceDefinition before it."],
            )
        })?,
    };
    if namespaced {
        if key.namespace.is_empty() {
            key.namespace = cx.namespace.clone();
        }
        intent["metadata"]["namespace"] = json!(key.namespace);
    } else {
        key.namespace.clear();
        if let Some(m) = intent["metadata"].as_object_mut() {
            m.remove("namespace");
        }
    }
    object::stamp(&mut intent, &cx.target, &cx.module);

    let Some(resource) = resource else {
        return Ok(create(
            cx,
            key,
            None,
            intent,
            vec![
                "planned without a dry run: its CustomResourceDefinition is created by this apply"
                    .into(),
            ],
        ));
    };
    let path = resource.path(&key.namespace, Some(&key.name));
    let live = cx.kube.get(&path).await?;
    let owner = Owner::of(live.as_ref(), &cx.target);
    if let Owner::Other(t) = &owner {
        return Ok(Step::blocked(
            key.clone(),
            format!(
                "{key} is managed by the cluster target '{t}'; a workload of one target may not take over another's objects."
            ),
        ));
    }
    let mut warnings = Vec::new();
    if owner == Owner::Unmanaged {
        warnings.push(format!(
            "adopts {key}, which exists without Buckle's labels"
        ));
    }

    let Some(live) = live else {
        let fresh_namespace = !key.namespace.is_empty()
            && created
                .iter()
                .any(|c| c.kind == "Namespace" && c.name == key.namespace);
        if fresh_namespace {
            return Ok(create(cx, key, Some(path), intent, Vec::new()));
        }
        // The dry run validates the object (admission, schema) now.
        dry_run(cx, &key, &path, &intent).await?;
        return Ok(create(cx, key, Some(path), intent, warnings));
    };

    // A controller's status write between the read and the dry run moves
    // the resourceVersion: read again rather than fail the plan.
    let mut live = live;
    let mut retries = 0;
    let diff = loop {
        rebase(&mut intent, &spec.ignore, &live);
        if let Some(d) = owned_diff(cx, &key, &path, &live, &intent).await? {
            break d;
        }
        retries += 1;
        match cx.kube.get(&path).await? {
            Some(l) if retries <= CONFLICT_RETRIES => live = l,
            _ => {
                return Err(user(
                    format!("{key} could not be planned: it kept changing while it was read."),
                    &["Plan again."],
                ));
            }
        }
    };
    let OwnedDiff { before, after } = diff.clone();
    let ours = owner == Owner::Us(object::label_value(&cx.module));

    let planned = if before == after && ours {
        return Ok(Step::noop(key));
    } else if before == after {
        Planned::change(Diff::Opaque {
            summary: format!("start managing {key} (it already matches)"),
        })
    } else {
        let paths = fields::changed_paths(&before, &after);
        let mut summary = format!("update {key}");
        if !paths.is_empty() {
            summary.push_str(": ");
            summary.push_str(&paths.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
            if paths.len() > 3 {
                summary.push_str(&format!(" (+{} more)", paths.len() - 3));
            }
        }
        Planned::change(Diff::unified(
            summary,
            &Redacted::plain(yaml(&before)),
            &Redacted::plain(yaml(&after)),
        ))
    };
    Ok(Step {
        key,
        planned,
        warnings,
        pending: StepPending::Manifest(Pending {
            path: Some(path),
            body: intent,
            planned: Some(diff),
            ignore: spec.ignore.clone(),
        }),
    })
}

/// Points `intent` at `live`: its ignored fields take the live values and
/// it carries the live `resourceVersion`.
fn rebase(intent: &mut Value, ignore: &[FieldPath], live: &Value) {
    adopt_all(ignore, intent, live);
    if let Some(rv) = live["metadata"]["resourceVersion"].as_str() {
        intent["metadata"]["resourceVersion"] = json!(rv);
    }
}

/// The diff `intent` makes to `live` over the fields Buckle owns on either
/// side; `None` when `live` moved before the dry run.
async fn owned_diff(
    cx: &Context,
    key: &ObjectKey,
    path: &str,
    live: &Value,
    intent: &Value,
) -> Result<Option<OwnedDiff>> {
    let Some(dry) = dry_run(cx, key, path, intent).await? else {
        return Ok(None);
    };
    let mut set = fields::buckle_owned(live);
    fields::union(&mut set, &fields::buckle_owned(&dry));
    let before = shown(cx, key, fields::extract(live, &set).unwrap_or(Value::Null));
    let after = shown(cx, key, fields::extract(&dry, &set).unwrap_or(Value::Null));
    Ok(Some(OwnedDiff { before, after }))
}

fn create(
    cx: &Context,
    key: ObjectKey,
    path: Option<String>,
    intent: Value,
    warnings: Vec<String>,
) -> Step {
    let mut preview = intent.clone();
    if let Some(labels) = preview["metadata"]["labels"].as_object_mut() {
        labels.remove(object::TARGET_LABEL);
        labels.remove(object::WORKLOAD_LABEL);
        if labels.is_empty() {
            preview["metadata"]
                .as_object_mut()
                .map(|m| m.remove("labels"));
        }
    }
    let preview = shown(cx, &key, preview);
    Step {
        planned: Planned::change(Diff::Create {
            summary: format!("create {key}"),
            preview: Some(Redacted::plain(yaml(&preview))),
        }),
        key,
        warnings,
        pending: StepPending::Manifest(Pending {
            path,
            body: intent,
            planned: None,
            ignore: Vec::new(),
        }),
    }
}

/// `None` unless `created` holds a CRD for `key`'s group and kind; then
/// whether it is namespaced. Without the CRD's body the scope defaults to
/// namespaced.
fn crd_scope(created: &[ObjectKey], key: &ObjectKey) -> Option<bool> {
    created
        .iter()
        .any(|c| {
            c.kind == "CustomResourceDefinition" && c.name.ends_with(&format!(".{}", key.group()))
        })
        .then_some(true)
}

/// The object as `intent` would leave it; `None` when it moved since
/// `intent`'s resourceVersion was read.
async fn dry_run(
    cx: &Context,
    key: &ObjectKey,
    path: &str,
    intent: &Value,
) -> Result<Option<Value>> {
    let body = serde_json::to_vec(intent).unwrap_or_default();
    match cx.kube.apply(path, body, true).await? {
        Apply::Object(v) => Ok(Some(v)),
        Apply::Conflict(_) if !intent["metadata"]["resourceVersion"].is_null() => Ok(None),
        Apply::Conflict(m) => Err(user(
            format!("{key} could not be planned: {m}"),
            &["Plan again; the object changed while it was read."],
        )),
    }
}

/// An object as plans show it: volatile metadata gone, and a Secret
/// manifest's values as fingerprints.
fn shown(cx: &Context, key: &ObjectKey, mut obj: Value) -> Value {
    fields::strip_volatile(&mut obj);
    if key.kind == "Secret" && key.group().is_empty() {
        for field in ["data", "stringData"] {
            if let Some(map) = obj[field].as_object_mut() {
                for v in map.values_mut() {
                    let text = ApiSink.receive(v.as_str().unwrap_or_default().to_owned());
                    *v = json!(format!(
                        "\u{ab}value@{}\u{bb}",
                        ApiSink.fingerprint_live(&cx.fingerprints, &text)
                    ));
                }
            }
        }
    }
    obj
}

pub(crate) async fn apply(cx: &Context, key: &ObjectKey, p: &Pending) -> Result<Applied> {
    let path = match &p.path {
        Some(path) => path.clone(),
        None => {
            cx.kube.refresh_discovery();
            cx.kube
                .resource_of(key)
                .await?
                .path(&key.namespace, Some(&key.name))
        }
    };
    let entry = Entry {
        key: key.clone(),
        workload: cx.module.clone(),
    };
    // Indexed first, so an object that applies is never missed as an orphan.
    index::update(&cx.kube, &cx.target, |e| index::upsert(e, entry.clone())).await?;
    let mut body = p.body.clone();
    let mut retries = 0;
    let applied = loop {
        let sent = cx
            .kube
            .apply(&path, serde_json::to_vec(&body).unwrap_or_default(), false)
            .await?;
        // A conflict alone does not mean the plan is stale: any write bumps
        // the resourceVersion, a controller's status among them.
        if let Apply::Object(v) = sent {
            break v;
        }
        body = loop {
            retries += 1;
            if retries > CONFLICT_RETRIES {
                return Err(moved(key));
            }
            match replan(cx, key, &path, p).await? {
                Replan::Same(rebased) => break rebased,
                Replan::MovedAgain => continue,
                Replan::Changed => return Err(moved(key)),
            }
        };
    };
    if key.kind == "CustomResourceDefinition" {
        cx.kube.refresh_discovery();
        wait(cx, key, &path, health::established).await?;
        return Ok(Applied::changed(format!("applied {key}")));
    }
    if health::is_rollout(key) {
        let generation = applied["metadata"]["generation"].as_i64().unwrap_or(0);
        let summary = wait(cx, key, &path, move |obj| {
            health::rollout(obj, Some(generation))
        })
        .await?;
        return Ok(Applied::changed(format!("applied {key}: {summary}")));
    }
    Ok(Applied::changed(format!("applied {key}")))
}

/// The diff of a moved object, taken again.
enum Replan {
    /// The fields Buckle owns show the planned diff: the intent, rebased on
    /// the object as it is now.
    Same(Value),
    /// The object moved again before the diff was taken.
    MovedAgain,
    /// The diff is not the one planned (or the object is gone).
    Changed,
}

async fn replan(cx: &Context, key: &ObjectKey, path: &str, p: &Pending) -> Result<Replan> {
    let Some(planned) = &p.planned else {
        return Ok(Replan::Changed);
    };
    let Some(live) = cx.kube.get(path).await? else {
        return Ok(Replan::Changed);
    };
    let mut intent = p.body.clone();
    rebase(&mut intent, &p.ignore, &live);
    let Some(mut diff) = owned_diff(cx, key, path, &live, &intent).await? else {
        return Ok(Replan::MovedAgain);
    };
    // Ignored fields belong to another tool (an autoscaler's replicas): they
    // may move freely, and the rebased intent keeps their new values.
    adopt_all(&p.ignore, &mut diff.before, &planned.before);
    adopt_all(&p.ignore, &mut diff.after, &planned.after);
    Ok(if &diff == planned {
        Replan::Same(intent)
    } else {
        Replan::Changed
    })
}

fn moved(key: &ObjectKey) -> buckle_core::Error {
    user(
        format!("{key} changed in the cluster after it was planned."),
        &["Plan again: the apply refuses to overwrite a change it has not shown."],
    )
}

/// Polls `path` until `state` reports done, failed, or the rollout timeout;
/// a controller also fails once its pods stay unplaceable for the placement
/// timeout.
async fn wait(
    cx: &Context,
    key: &ObjectKey,
    path: &str,
    state: impl Fn(&Value) -> Rollout,
) -> Result<String> {
    let started = tokio::time::Instant::now();
    let mut unplaced_since: Option<tokio::time::Instant> = None;
    let mut last = String::new();
    loop {
        let obj = cx.kube.get(path).await?.unwrap_or(Value::Null);
        match state(&obj) {
            Rollout::Done(s) => return Ok(s),
            Rollout::Failed(why) => {
                return Err(user(
                    format!("{key} did not roll out: {why}"),
                    &[
                        "Check the pods' events (`kubectl describe`), fix the manifest and apply again; roll back to restore the previous revision.",
                    ],
                ));
            }
            Rollout::Progressing(s) => {
                if s != last {
                    (cx.progress)(&format!("{key}: {s}"));
                    last = s;
                }
                if health::is_rollout(key) {
                    let unplaced = match health::pods(&cx.kube, key, &obj).await? {
                        Some(pods) => health::unplaced_pods(&pods),
                        None => Vec::new(),
                    };
                    if unplaced.is_empty() {
                        unplaced_since = None;
                    } else if unplaced_since
                        .get_or_insert_with(tokio::time::Instant::now)
                        .elapsed()
                        >= cx.placement_timeout
                    {
                        return Err(user(
                            format!(
                                "{key} could not be placed for {}s: {}",
                                cx.placement_timeout.as_secs(),
                                unplaced.join("; ")
                            ),
                            &[
                                "Free capacity, fix the pods' node selectors, affinities or volumes, or raise the workload's `timeouts.placement`.",
                            ],
                        ));
                    }
                }
            }
        }
        if started.elapsed() >= cx.rollout_timeout {
            return Err(user(
                format!(
                    "{key} was still rolling out after {}s ({last}).",
                    cx.rollout_timeout.as_secs()
                ),
                &[
                    "The rollout continues in the cluster; plan again once it settles, or raise the workload's `timeouts.rollout`.",
                ],
            ));
        }
        tokio::time::sleep(cx.poll).await;
    }
}
