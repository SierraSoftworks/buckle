//! A manifest object, applied with server-side apply under the `buckle`
//! field manager.
//!
//! The plan is the API server's own: a dry-run apply returns the object as
//! it would be, and the fields Buckle owns (before or after) are compared
//! between the live object and that result, so defaults and other
//! controllers' fields never show as changes. The apply sends the planned
//! intent with the live `resourceVersion`, so an object that moved after the
//! plan is refused rather than overwritten.

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
}

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

    adopt_all(&spec.ignore, &mut intent, &live);
    if let Some(rv) = live["metadata"]["resourceVersion"].as_str() {
        intent["metadata"]["resourceVersion"] = json!(rv);
    }
    let dry = dry_run(cx, &key, &path, &intent).await?;
    let mut set = fields::buckle_owned(&live);
    fields::union(&mut set, &fields::buckle_owned(&dry));
    let before = shown(
        cx,
        &key,
        fields::extract(&live, &set).unwrap_or(Value::Null),
    );
    let after = shown(cx, &key, fields::extract(&dry, &set).unwrap_or(Value::Null));
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
        }),
    })
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
        pending: StepPending::Manifest(Pending { path, body: intent }),
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

async fn dry_run(cx: &Context, key: &ObjectKey, path: &str, intent: &Value) -> Result<Value> {
    let body = serde_json::to_vec(intent).unwrap_or_default();
    match cx.kube.apply(path, body, true).await? {
        Apply::Object(v) => Ok(v),
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
    let applied = match cx
        .kube
        .apply(
            &path,
            serde_json::to_vec(&p.body).unwrap_or_default(),
            false,
        )
        .await?
    {
        Apply::Object(v) => v,
        Apply::Conflict(_) => {
            return Err(user(
                format!("{key} changed in the cluster after it was planned."),
                &["Plan again: the apply refuses to overwrite a change it has not shown."],
            ));
        }
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

/// Polls `path` until `state` reports done, failed, or the rollout timeout.
async fn wait(
    cx: &Context,
    key: &ObjectKey,
    path: &str,
    state: impl Fn(&Value) -> Rollout,
) -> Result<String> {
    let started = tokio::time::Instant::now();
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
            }
        }
        if started.elapsed() >= cx.rollout_timeout {
            return Err(user(
                format!(
                    "{key} was still rolling out after {}s ({last}).",
                    cx.rollout_timeout.as_secs()
                ),
                &["The rollout continues in the cluster; plan again once it settles."],
            ));
        }
        tokio::time::sleep(cx.poll).await;
    }
}
