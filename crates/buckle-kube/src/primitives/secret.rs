//! A workload's `secret` entry on Kubernetes: a Secret whose items may hold
//! Buckle secrets.
//!
//! Live and desired values are compared inside the executor through the
//! [`ApiSink`]; plans show plain values as written and everything else as
//! keyed fingerprints. Buckle owns the keys it writes (server-side apply
//! records them under its field manager): keys operators added stay, and a
//! key dropped from the workload is removed.

use super::{Context, Pending as StepPending, Step};
use crate::client::{Applied as Apply, seg};
use crate::fields;
use crate::index::{self, Entry};
use crate::object::{self, ObjectKey, Owner};
use base64::Engine;
use buckle_core::Result;
use buckle_core::error::user;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::sinks::ApiSink;
use buckle_core::secrets::{Redacted, Rendered, Sensitive};
use buckle_core::task::Applied;
use indexmap::IndexMap;
use serde_json::json;

/// One Secret and the items the workload sets on it.
#[derive(Debug, Clone)]
pub struct SecretSpec {
    /// Empty: the workload's namespace.
    pub namespace: String,
    pub name: String,
    /// The Secret's `type` (default `Opaque`).
    pub secret_type: Option<String>,
    pub items: IndexMap<String, Rendered>,
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    body: Sensitive<Vec<u8>>,
}

fn path(ns: &str, name: &str) -> String {
    format!("/api/v1/namespaces/{}/secrets/{}", seg(ns), seg(name))
}

fn shown(cx: &Context, value: &Rendered) -> String {
    match value {
        Rendered::Plain(text) => format!("{text:?}"),
        Rendered::Sensitive(_) => format!(
            "\u{ab}value@{}\u{bb}",
            ApiSink.fingerprint(&cx.fingerprints, value)
        ),
    }
}

pub(crate) async fn plan(cx: &Context, spec: &SecretSpec) -> Result<Step> {
    if spec.items.is_empty() {
        return Err(user(
            format!("The secret {} declares no items.", spec.name),
            &["List the Secret's keys under `items`."],
        ));
    }
    let ns = if spec.namespace.is_empty() {
        cx.namespace.clone()
    } else {
        spec.namespace.clone()
    };
    let key = ObjectKey::new("v1", "Secret", &ns, &spec.name);
    let secret_type = spec.secret_type.clone().unwrap_or_else(|| "Opaque".into());
    let live = cx.kube.get(&path(&ns, &spec.name)).await?;
    let owner = Owner::of(live.as_ref(), &cx.target);
    if let Owner::Other(t) = &owner {
        return Ok(Step::blocked(
            key.clone(),
            format!(
                "{key} is managed by the cluster target '{t}'; a workload of one target may not take over another's objects."
            ),
        ));
    }
    if let Some(live_type) = live.as_ref().and_then(|l| l["type"].as_str())
        && live_type != secret_type
    {
        return Ok(Step::blocked(
            key.clone(),
            format!(
                "{key} has type {live_type}, not {secret_type}; a Secret's type cannot change, so delete it first."
            ),
        ));
    }

    let decode = |v: &serde_json::Value| {
        let raw = v.as_str().unwrap_or_default();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(raw)
            .unwrap_or_default();
        ApiSink.receive(String::from_utf8_lossy(&bytes).into_owned())
    };
    let live_items: IndexMap<String, Sensitive<String>> = live
        .as_ref()
        .and_then(|l| l["data"].as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), decode(v))).collect())
        .unwrap_or_default();
    let owned_keys: Vec<String> = live
        .as_ref()
        .map(fields::buckle_owned)
        .and_then(|s| s["f:data"].as_object().cloned())
        .map(|m| {
            m.keys()
                .filter_map(|k| k.strip_prefix("f:").map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let mut lines = Vec::new();
    for (k, desired) in &spec.items {
        match live_items.get(k) {
            None => lines.push(format!("+ {k} = {}", shown(cx, desired))),
            Some(l) if !ApiSink.matches(desired, l) => lines.push(format!(
                "- {k} = \u{ab}value@{}\u{bb}\n+ {k} = {}",
                ApiSink.fingerprint_live(&cx.fingerprints, l),
                shown(cx, desired)
            )),
            Some(_) => {}
        }
    }
    for k in owned_keys.iter().filter(|k| !spec.items.contains_key(*k)) {
        if let Some(l) = live_items.get(k) {
            lines.push(format!(
                "- {k} = \u{ab}value@{}\u{bb}",
                ApiSink.fingerprint_live(&cx.fingerprints, l)
            ));
        }
    }

    let ours = owner == Owner::Us(object::label_value(&cx.module));
    let planned = if lines.is_empty() && ours {
        return Ok(Step::noop(key));
    } else if lines.is_empty() {
        Planned::change(Diff::Opaque {
            summary: format!("start managing {key} (its items already match)"),
        })
    } else if live.is_none() {
        Planned::change(Diff::Create {
            summary: format!("create {key} ({} items)", spec.items.len()),
            preview: Some(Redacted::plain(lines.join("\n") + "\n")),
        })
    } else {
        Planned::change(Diff::Update {
            summary: format!("update {key} ({} changed)", lines.len()),
            unified: Some(Redacted::plain(lines.join("\n") + "\n")),
        })
    };

    let mut envelope = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {"name": spec.name, "namespace": ns},
        "type": secret_type,
    });
    object::stamp(&mut envelope, &cx.target, &cx.module);
    if let Some(rv) = live
        .as_ref()
        .and_then(|l| l["metadata"]["resourceVersion"].as_str())
    {
        envelope["metadata"]["resourceVersion"] = json!(rv);
    }
    let body = ApiSink.json_body_encoded(
        envelope,
        "data",
        spec.items.iter().map(|(k, v)| (k.as_str(), v)),
        |text| base64::engine::general_purpose::STANDARD.encode(text),
    );
    let mut warnings = Vec::new();
    if owner == Owner::Unmanaged {
        warnings.push(format!(
            "adopts {key}, which exists without Buckle's labels"
        ));
    }
    Ok(Step {
        key,
        planned,
        warnings,
        pending: StepPending::Secret(Pending { body }),
    })
}

pub(crate) async fn apply(cx: &Context, key: &ObjectKey, p: &Pending) -> Result<Applied> {
    let entry = Entry {
        key: key.clone(),
        workload: cx.module.clone(),
    };
    index::update(&cx.kube, &cx.target, |e| index::upsert(e, entry.clone())).await?;
    match cx
        .kube
        .apply(
            &path(&key.namespace, &key.name),
            ApiSink.open(&p.body).to_vec(),
            false,
        )
        .await?
    {
        Apply::Object(_) => Ok(Applied::changed(format!("wrote {key}"))),
        Apply::Conflict(_) => Err(user(
            format!("{key} changed in the cluster after it was planned."),
            &["Plan again: the apply refuses to overwrite a change it has not shown."],
        )),
    }
}
