//! `volumes`: dynamic host volumes and CSI volumes, created once by name.
//!
//! Host volumes are keyed by an id Nomad assigns, so existence by name is
//! the only safe check; a volume is never updated in place (its capacity
//! and placement are fixed at creation). Volumes carry no metadata, so the
//! target's index variable lists them.

use super::{Context, Kind, ObjectKey, Pending as StepPending, Step};
use crate::client::{seg, with_query};
use crate::hcl;
use crate::index::{self, Entry};
use crate::stamps;
use buckle_core::Result;
use buckle_core::error::user;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::Redacted;
use buckle_core::task::Applied;
use serde_json::{Map, Value, json};

/// A volume spec file, as `nomad volume create` reads it.
#[derive(Debug, Clone)]
pub struct VolumeSpec {
    pub file: String,
    pub hcl: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolumeType {
    Host,
    Csi,
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    ty: VolumeType,
    body: Value,
    /// An existing volume to list in the index without creating it.
    existing_id: Option<String>,
    entry: Entry,
}

struct Decoded {
    ty: VolumeType,
    namespace: String,
    name: String,
    api: Value,
}

fn decode(spec: &VolumeSpec) -> Result<Decoded> {
    let raw = hcl::decode(&spec.file, &spec.hcl)?;
    let s = |k: &str| raw[k].as_str().unwrap_or_default().to_owned();
    let ty = match s("type").as_str() {
        "host" => VolumeType::Host,
        "csi" => VolumeType::Csi,
        other => {
            return Err(user(
                format!("{} declares a volume of type '{other}'.", spec.file),
                &["Set `type = \"host\"` (dynamic host volume) or `type = \"csi\"`."],
            ));
        }
    };
    let name = s("name");
    if name.is_empty() {
        return Err(user(
            format!("{} does not name its volume.", spec.file),
            &["Set `name = \"…\"` in the file."],
        ));
    }
    let namespace = super::namespace_or_default(raw["namespace"].as_str());
    let list = |v: &Value| -> Vec<Value> {
        match v {
            Value::Array(a) => a.clone(),
            Value::Null => Vec::new(),
            other => vec![other.clone()],
        }
    };
    let capabilities: Vec<Value> = list(&raw["capability"])
        .iter()
        .map(|c| json!({"AccessMode": c["access_mode"], "AttachmentMode": c["attachment_mode"]}))
        .collect();
    let constraints: Vec<Value> = list(&raw["constraint"])
        .iter()
        .map(|c| {
            json!({
                "LTarget": c["attribute"],
                "RTarget": c["value"],
                "Operand": c["operator"].as_str().unwrap_or("="),
            })
        })
        .collect();
    let mut api = Map::new();
    api.insert("Name".into(), json!(name));
    api.insert("Namespace".into(), json!(namespace));
    api.insert("PluginID".into(), raw["plugin_id"].clone());
    api.insert("Parameters".into(), raw["parameters"].clone());
    api.insert("RequestedCapabilities".into(), json!(capabilities));
    let (min, max) = (
        hcl::bytes(&raw["capacity_min"]),
        hcl::bytes(&raw["capacity_max"]),
    );
    match ty {
        VolumeType::Host => {
            api.insert("NodePool".into(), raw["node_pool"].clone());
            api.insert("NodeID".into(), raw["node_id"].clone());
            api.insert("Constraints".into(), json!(constraints));
            api.insert("RequestedCapacityMinBytes".into(), json!(min));
            api.insert("RequestedCapacityMaxBytes".into(), json!(max));
        }
        VolumeType::Csi => {
            let id = raw["id"].as_str().unwrap_or(&name).to_owned();
            api.insert("ID".into(), json!(id));
            api.insert("RequestedCapacityMin".into(), json!(min));
            api.insert("RequestedCapacityMax".into(), json!(max));
            if raw["mount_options"].is_object() {
                api.insert(
                    "MountOptions".into(),
                    json!({
                        "FSType": raw["mount_options"]["fs_type"],
                        "MountFlags": raw["mount_options"]["mount_flags"],
                    }),
                );
            }
        }
    }
    api.retain(|_, v| !v.is_null());
    Ok(Decoded {
        ty,
        namespace,
        name,
        api: Value::Object(api),
    })
}

/// The id of the live volume called `name`, if any.
async fn find(cx: &Context, d: &Decoded) -> Result<Option<String>> {
    match d.ty {
        VolumeType::Host => {
            let list = cx
                .nomad
                .get::<Vec<Value>>(&with_query(
                    "/v1/volumes?type=host",
                    "namespace",
                    &d.namespace,
                ))
                .await?
                .unwrap_or_default();
            Ok(list
                .iter()
                .find(|v| v["Name"].as_str() == Some(&d.name))
                .and_then(|v| v["ID"].as_str().map(str::to_owned)))
        }
        VolumeType::Csi => {
            let id = d.api["ID"].as_str().unwrap_or_default();
            let url = with_query(
                &format!("/v1/volume/csi/{}", seg(id)),
                "namespace",
                &d.namespace,
            );
            Ok(cx.nomad.get::<Value>(&url).await?.map(|_| id.to_owned()))
        }
    }
}

pub(crate) async fn plan(cx: &Context, spec: &VolumeSpec) -> Result<Step> {
    let d = decode(spec)?;
    let key = ObjectKey::new(Kind::Volume, &d.namespace, &d.name);
    let live = find(cx, &d).await?;
    let idx = index::read(&cx.nomad, &cx.target).await?;
    let listed = idx.find(Kind::Volume, &d.namespace, &d.name);
    let digest = stamps::short_digest(&d.api);
    let entry = Entry {
        kind: Kind::Volume,
        namespace: d.namespace.clone(),
        name: d.name.clone(),
        module: cx.module.clone(),
        revision: listed
            .map(|e| e.revision.clone())
            .unwrap_or_else(|| cx.revision.clone()),
        digest,
        items: Vec::new(),
        id: live.clone(),
    };
    let planned = match (&live, listed) {
        (Some(id), Some(e)) if e.id.as_deref() == Some(id) && e.module == cx.module => {
            return Ok(Step::noop(key));
        }
        (Some(_), _) => Planned::change(Diff::Opaque {
            summary: format!("start managing {key} (it exists; volumes are created once)"),
        }),
        (None, _) => Planned::change(Diff::Create {
            summary: format!("create {key}"),
            preview: Some(Redacted::plain(spec.hcl.clone())),
        }),
    };
    let mut warnings = Vec::new();
    if live.is_some() && listed.is_some_and(|e| e.digest != entry.digest) {
        warnings.push(format!(
            "{key} differs from its spec; volumes are not updated in place (delete it to recreate)."
        ));
    }
    Ok(Step {
        key,
        planned,
        warnings,
        preview: Vec::new(),
        pending: StepPending::Volume(Pending {
            ty: d.ty,
            body: d.api,
            existing_id: live,
            entry,
        }),
    })
}

pub(crate) async fn apply(cx: &Context, key: &ObjectKey, p: &Pending) -> Result<Applied> {
    let mut entry = p.entry.clone();
    let summary = match &p.existing_id {
        Some(_) => format!("listed {key}"),
        None => {
            let ns = &key.namespace;
            let (url, body) = match p.ty {
                VolumeType::Host => (
                    with_query("/v1/volume/host/create", "namespace", ns),
                    json!({"Volume": p.body, "PolicyOverride": false}),
                ),
                VolumeType::Csi => (
                    with_query(
                        &format!(
                            "/v1/volume/csi/{}/create",
                            seg(p.body["ID"].as_str().unwrap_or_default())
                        ),
                        "namespace",
                        ns,
                    ),
                    json!({"Volumes": [p.body]}),
                ),
            };
            let resp: Value = cx.nomad.write("PUT", &url, &body).await?;
            entry.id = resp["Volume"]["ID"]
                .as_str()
                .or_else(|| resp["Volumes"][0]["ID"].as_str())
                .or_else(|| p.body["ID"].as_str())
                .map(str::to_owned);
            format!("created {key}")
        }
    };
    index::update(&cx.nomad, &cx.target, |e| index::upsert(e, entry.clone())).await?;
    Ok(Applied::changed(summary))
}

/// Deletes a volume no allocation uses; a volume in use is kept (and stays
/// listed) with a message saying why.
pub(crate) async fn remove(cx: &Context, key: &ObjectKey, id: Option<&str>) -> Result<Applied> {
    let ns = &key.namespace;
    let Some(id) = id else {
        return Ok(Applied::skipped(format!(
            "{key} has no recorded id; nothing deleted"
        )));
    };
    let host = with_query(&format!("/v1/volume/host/{}", seg(id)), "namespace", ns);
    let (live, delete) = match cx.nomad.get::<Value>(&host).await? {
        Some(v) => (Some(v), host.clone()),
        None => {
            let csi = with_query(&format!("/v1/volume/csi/{}", seg(id)), "namespace", ns);
            let v = cx.nomad.get::<Value>(&csi).await?;
            (
                v,
                with_query(
                    &format!("/v1/volume/csi/{}/delete", seg(id)),
                    "namespace",
                    ns,
                ),
            )
        }
    };
    if let Some(v) = &live {
        let in_use = ["Allocations", "ReadAllocs", "WriteAllocs"]
            .iter()
            .any(|k| match &v[*k] {
                Value::Array(a) => !a.is_empty(),
                Value::Object(o) => !o.is_empty(),
                _ => false,
            });
        if in_use {
            return Ok(Applied::skipped(format!(
                "kept {key}: an allocation still uses it"
            )));
        }
        cx.nomad.delete(&delete).await?;
    }
    index::update(&cx.nomad, &cx.target, |e| {
        index::forget(e, Kind::Volume, ns, &key.name)
    })
    .await?;
    Ok(Applied::changed(format!("deleted {key}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_volumes_decode_into_the_create_request() {
        let d = decode(&VolumeSpec {
            file: "paste.volume.hcl".into(),
            hcl: r#"
name         = "paste"
type         = "host"
plugin_id    = "mkdir"
capacity_min = "1GiB"
capability {
  access_mode     = "single-node-writer"
  attachment_mode = "file-system"
}
constraint {
  attribute = "${attr.kernel.name}"
  value     = "linux"
}
"#
            .into(),
        })
        .unwrap();
        assert_eq!(
            (d.ty, d.namespace.as_str(), d.name.as_str()),
            (VolumeType::Host, "default", "paste")
        );
        assert_eq!(d.api["PluginID"], "mkdir");
        assert_eq!(d.api["RequestedCapacityMinBytes"], 1u64 << 30);
        assert_eq!(
            d.api["RequestedCapabilities"][0]["AccessMode"],
            "single-node-writer"
        );
        assert_eq!(d.api["Constraints"][0]["Operand"], "=");
        assert_eq!(d.api["Constraints"][0]["LTarget"], "${attr.kernel.name}");
        assert!(d.api.get("RequestedCapacityMaxBytes").is_none());
    }

    #[rstest::rstest]
    #[case("name = \"x\"\ntype = \"nfs\"")]
    #[case("type = \"host\"")]
    fn invalid_volumes_are_rejected(#[case] hcl: &str) {
        assert!(
            decode(&VolumeSpec {
                file: "v.hcl".into(),
                hcl: hcl.into()
            })
            .is_err()
        );
    }
}
