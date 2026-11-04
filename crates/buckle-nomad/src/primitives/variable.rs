//! `variables`: Nomad variables whose items may hold secrets.
//!
//! Live and desired values are compared inside the executor through the
//! [`ApiSink`]; the plan shows plain values as written and everything else
//! as keyed fingerprints. Buckle owns the items it declares: items operators
//! added to the same path stay, and an item dropped from the workload is
//! removed (the index variable remembers which items Buckle wrote).

use super::{Context, Kind, ObjectKey, Pending as StepPending, Step};
use crate::client::{self, with_query};
use crate::index::{self, Entry};
use crate::stamps;
use buckle_core::Result;
use buckle_core::error::user;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::sinks::ApiSink;
use buckle_core::secrets::{Redacted, Rendered};
use buckle_core::task::Applied;
use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::BTreeMap;

/// One variable path and the items the workload sets on it.
#[derive(Debug, Clone)]
pub struct VariableSpec {
    pub namespace: String,
    pub path: String,
    /// Rendered item values: [`Rendered::Sensitive`] when a value referenced
    /// a secret.
    pub items: IndexMap<String, Rendered>,
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    /// The variable's modify index at plan time; 0 when it must not exist.
    cas: u64,
    /// Every item the write sends: the workload's, and the live ones it keeps.
    items: IndexMap<String, Rendered>,
    entry: Entry,
}

#[derive(Deserialize)]
struct Live {
    #[serde(rename = "Items", default)]
    items: BTreeMap<String, String>,
    #[serde(rename = "ModifyIndex", default)]
    modify_index: u64,
}

fn url(ns: &str, path: &str) -> String {
    with_query(&format!("/v1/var/{}", client::path(path)), "namespace", ns)
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

pub(crate) async fn plan(cx: &Context, spec: &VariableSpec) -> Result<Step> {
    if spec.items.is_empty() {
        return Err(user(
            format!("The variable {} declares no items.", spec.path),
            &["Nomad variables need at least one item."],
        ));
    }
    let ns = super::namespace_or_default(Some(&spec.namespace));
    let key = ObjectKey::new(Kind::Variable, &ns, &spec.path);
    let live = cx.nomad.get::<Live>(&url(&ns, &spec.path)).await?;
    let idx = index::read(&cx.nomad, &cx.target).await?;
    let previous = idx.find(Kind::Variable, &ns, &spec.path);

    let (cas, live_items) = match live {
        Some(l) => (
            l.modify_index,
            l.items
                .into_iter()
                .map(|(k, v)| (k, ApiSink.receive(v)))
                .collect::<IndexMap<_, _>>(),
        ),
        None => (0, IndexMap::new()),
    };

    let mut lines = Vec::new();
    for (k, desired) in &spec.items {
        match live_items.get(k) {
            None => lines.push(format!("+ {k} = {}", shown(cx, desired))),
            Some(l) if !ApiSink.matches(desired, l) => lines.push(format!(
                "~ {k} = {} (was \u{ab}value@{}\u{bb})",
                shown(cx, desired),
                ApiSink.fingerprint_live(&cx.fingerprints, l)
            )),
            Some(_) => {}
        }
    }
    let dropped: Vec<&String> = previous
        .map(|p| {
            p.items
                .iter()
                .filter(|i| !spec.items.contains_key(*i))
                .collect()
        })
        .unwrap_or_default();
    for k in &dropped {
        if let Some(l) = live_items.get(*k) {
            lines.push(format!(
                "- {k} (was \u{ab}value@{}\u{bb})",
                ApiSink.fingerprint_live(&cx.fingerprints, l)
            ));
        }
    }

    let fingerprints: Vec<(String, String)> = spec
        .items
        .iter()
        .map(|(k, v)| (k.clone(), ApiSink.fingerprint(&cx.fingerprints, v).as_hex()))
        .collect();
    let digest = stamps::short_digest(&fingerprints);
    let unchanged_entry = previous.is_some_and(|p| p.digest == digest && p.module == cx.module);
    let entry = Entry {
        kind: Kind::Variable,
        namespace: ns.clone(),
        name: spec.path.clone(),
        module: cx.module.clone(),
        revision: match previous {
            Some(p) if unchanged_entry => p.revision.clone(),
            _ => cx.revision.clone(),
        },
        digest,
        items: spec.items.keys().cloned().collect(),
        id: None,
    };

    let mut items: IndexMap<String, Rendered> = live_items
        .into_iter()
        .filter(|(k, _)| !spec.items.contains_key(k) && !dropped.contains(&k))
        .map(|(k, v)| (k, Rendered::Sensitive(v)))
        .collect();
    items.extend(spec.items.iter().map(|(k, v)| (k.clone(), v.clone())));

    let planned = if lines.is_empty() && unchanged_entry {
        Planned::NoOp
    } else if lines.is_empty() {
        Planned::change(Diff::Opaque {
            summary: format!("start managing {key} (its items already match)"),
        })
    } else if cas == 0 {
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
    if !planned.is_change() {
        return Ok(Step::noop(key));
    }
    Ok(Step {
        key,
        planned,
        warnings: Vec::new(),
        preview: Vec::new(),
        pending: StepPending::Variable(Pending { cas, items, entry }),
    })
}

pub(crate) async fn apply(cx: &Context, key: &ObjectKey, p: &Pending) -> Result<Applied> {
    let target = with_query(&url(&key.namespace, &key.name), "cas", &p.cas.to_string());
    let body = ApiSink.json_body(
        serde_json::json!({"Namespace": key.namespace, "Path": key.name}),
        "Items",
        p.items.iter().map(|(k, v)| (k.as_str(), v)),
    );
    let resp = cx
        .nomad
        .send("PUT", &target, Some(ApiSink.open(&body).to_vec()))
        .await?;
    if resp.status == 409 {
        return Err(user(
            format!("{key} changed in the cluster after it was planned."),
            &["Plan again: the apply refuses to overwrite a change it has not shown."],
        ));
    }
    cx.nomad.ok("PUT", &target, resp)?;
    let entry = p.entry.clone();
    index::update(&cx.nomad, &cx.target, |e| index::upsert(e, entry.clone())).await?;
    Ok(Applied::changed(format!("wrote {key}")))
}

pub(crate) async fn remove(cx: &Context, key: &ObjectKey) -> Result<Applied> {
    cx.nomad.delete(&url(&key.namespace, &key.name)).await?;
    index::update(&cx.nomad, &cx.target, |e| {
        index::forget(e, Kind::Variable, &key.namespace, &key.name)
    })
    .await?;
    Ok(Applied::changed(format!("deleted {key}")))
}
