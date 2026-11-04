//! `namespaces`, `node_pools` and `resources`: cluster-wide entities whose
//! spec files are HCL that Nomad's CLI parses client-side. Buckle decodes
//! them into the API's JSON, compares the fields the spec sets with the live
//! object and upserts through the entity's endpoint.

use super::{Context, Kind, ObjectKey, Orphan, Pending as StepPending, Step};
use crate::client::seg;
use crate::diff;
use crate::hcl::{self, Shape};
use crate::stamps::{self, Owner, Stamps};
use buckle_core::Result;
use buckle_core::error::user;
use buckle_core::plan::{Diff, Planned};
use buckle_core::secrets::Redacted;
use buckle_core::task::Applied;
use serde_json::Value;

/// The entity kinds a spec file can declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Namespace,
    NodePool,
    /// `resources: [{ type: quota, file: … }]` (Nomad Enterprise).
    Quota,
}

impl EntityKind {
    /// The `resources` types Buckle supports.
    pub const RESOURCE_TYPES: &'static [&'static str] = &["quota"];

    /// The entity kind of a `resources` entry's `type`.
    pub fn resource(ty: &str) -> Result<EntityKind> {
        match ty {
            "quota" => Ok(EntityKind::Quota),
            other => Err(user(
                format!("'{other}' is not a Nomad resource type Buckle can apply."),
                &[
                    "Supported resource types: quota. Namespaces, node pools, volumes, variables and policies have keys of their own.",
                ],
            )),
        }
    }

    pub fn kind(self) -> Kind {
        match self {
            EntityKind::Namespace => Kind::Namespace,
            EntityKind::NodePool => Kind::NodePool,
            EntityKind::Quota => Kind::Resource,
        }
    }

    fn shape(self) -> Shape {
        match self {
            EntityKind::Namespace => Shape {
                renames: &[
                    ("node_pool_config", "NodePoolConfiguration"),
                    ("vault", "VaultConfiguration"),
                    ("consul", "ConsulConfiguration"),
                ],
                opaque: &["meta"],
                lists: &[],
            },
            EntityKind::NodePool => Shape {
                renames: &[
                    ("scheduler_config", "SchedulerConfiguration"),
                    ("memory_oversubscription", "MemoryOversubscriptionEnabled"),
                ],
                opaque: &["meta"],
                lists: &[],
            },
            EntityKind::Quota => Shape {
                renames: &[
                    ("limit", "Limits"),
                    ("memory", "MemoryMB"),
                    ("memory_max", "MemoryMaxMB"),
                ],
                opaque: &[],
                lists: &["limit"],
            },
        }
    }

    fn get_url(self, name: &str) -> String {
        match self {
            EntityKind::Namespace => format!("/v1/namespace/{}", seg(name)),
            EntityKind::NodePool => format!("/v1/node/pool/{}", seg(name)),
            EntityKind::Quota => format!("/v1/quota/{}", seg(name)),
        }
    }

    fn write(self, name: &str) -> (&'static str, String) {
        match self {
            EntityKind::Namespace => ("POST", format!("/v1/namespace/{}", seg(name))),
            EntityKind::NodePool => ("PUT", "/v1/node/pools".into()),
            EntityKind::Quota => ("POST", "/v1/quota".into()),
        }
    }

    fn list_url(self) -> &'static str {
        match self {
            EntityKind::Namespace => "/v1/namespaces",
            EntityKind::NodePool => "/v1/node/pools",
            EntityKind::Quota => "/v1/quotas",
        }
    }

    /// Whether stamps go in `Meta` (else in the description).
    fn has_meta(self) -> bool {
        !matches!(self, EntityKind::Quota)
    }

    fn of(kind: Kind) -> Option<EntityKind> {
        match kind {
            Kind::Namespace => Some(EntityKind::Namespace),
            Kind::NodePool => Some(EntityKind::NodePool),
            Kind::Resource => Some(EntityKind::Quota),
            _ => None,
        }
    }
}

/// A namespace, node pool or resource spec file.
#[derive(Debug, Clone)]
pub struct EntitySpec {
    pub kind: EntityKind,
    pub file: String,
    pub hcl: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Pending {
    kind: EntityKind,
    body: Value,
    /// The live object's modify index at plan time; `None` when absent.
    modify_index: Option<u64>,
}

/// Decodes the spec into the API object and its name.
pub(crate) fn decode(spec: &EntitySpec) -> Result<(String, Value)> {
    let raw = hcl::decode(&spec.file, &spec.hcl)?;
    let raw = match spec.kind {
        // `node_pool "pis" { … }`: the label is the name.
        EntityKind::NodePool => {
            let pools = raw.get("node_pool").and_then(Value::as_object);
            match pools.map(|p| p.iter().collect::<Vec<_>>()).as_deref() {
                Some([(name, body)]) => {
                    let mut body = (*body).clone();
                    if let Some(map) = body.as_object_mut() {
                        map.insert("name".into(), Value::String((*name).clone()));
                    }
                    body
                }
                _ => {
                    return Err(user(
                        format!(
                            "{} must declare exactly one node_pool \"name\" {{ … }} block.",
                            spec.file
                        ),
                        &["One node pool per file, as `nomad node pool apply` reads it."],
                    ));
                }
            }
        }
        _ => raw,
    };
    let api = hcl::to_api(&raw, &spec.kind.shape());
    let name = api["Name"].as_str().unwrap_or_default().to_owned();
    if name.is_empty() {
        return Err(user(
            format!(
                "{} does not name its {}.",
                spec.file,
                spec.kind.kind().label()
            ),
            &["Set `name = \"…\"` in the file."],
        ));
    }
    Ok((name, api))
}

pub(crate) async fn plan(cx: &Context, spec: &EntitySpec) -> Result<Step> {
    let (name, mut desired) = decode(spec)?;
    let kind = spec.kind;
    let key = ObjectKey::new(kind.kind(), "", &name);
    let live = cx.nomad.get_indexed::<Value>(&kind.get_url(&name)).await?;
    let live_stamps = live.as_ref().and_then(|l| read_stamps(kind, &l.value));
    let owner = Owner::of(live_stamps, live.is_some(), &cx.target);
    if let Some(reason) = owner.conflict(&key.to_string()) {
        return Ok(Step::blocked(key, reason));
    }
    let digest = stamps::short_digest(&desired);
    let next = owner.next(&cx.target, &cx.module, &cx.revision, &digest);
    if kind.has_meta() {
        next.stamp_meta(&mut desired);
    } else {
        let text = desired["Description"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        desired["Description"] = Value::String(next.describe(&text));
    }

    let mut warnings = Vec::new();
    if owner == Owner::Unmanaged {
        warnings.push(format!(
            "{key} exists and is not managed by Buckle; applying adopts it."
        ));
    }
    let (planned, modify_index) = match &live {
        None => (
            Planned::change(Diff::Create {
                summary: format!("create {key}"),
                preview: Some(Redacted::plain(
                    serde_json::to_string_pretty(&desired).unwrap_or_default(),
                )),
            }),
            None,
        ),
        Some(l) => {
            let changes = diff::field_changes(&desired, &l.value, &["Meta"]);
            if changes.is_empty() {
                return Ok(Step::noop(key));
            }
            let index = l.value["ModifyIndex"].as_u64().unwrap_or(l.index);
            (
                Planned::change(Diff::Update {
                    summary: format!("update {key} ({} fields)", changes.len()),
                    unified: Some(Redacted::plain(changes.join("\n") + "\n")),
                }),
                Some(index),
            )
        }
    };
    Ok(Step {
        key,
        planned,
        warnings,
        preview: Vec::new(),
        pending: StepPending::Entity(Pending {
            kind,
            body: desired,
            modify_index,
        }),
    })
}

fn read_stamps(kind: EntityKind, live: &Value) -> Option<Stamps> {
    if kind.has_meta() {
        Stamps::from_meta(live.get("Meta"))
    } else {
        Stamps::from_description(live["Description"].as_str().unwrap_or_default()).1
    }
}

pub(crate) async fn apply(cx: &Context, key: &ObjectKey, p: &Pending) -> Result<Applied> {
    let live = cx.nomad.get::<Value>(&p.kind.get_url(&key.name)).await?;
    let now = live
        .as_ref()
        .map(|l| l["ModifyIndex"].as_u64().unwrap_or(0));
    if now != p.modify_index {
        return Err(user(
            format!("{key} changed in the cluster after it was planned."),
            &["Plan again: the apply refuses to overwrite a change it has not shown."],
        ));
    }
    let (method, url) = p.kind.write(&key.name);
    cx.nomad.write::<_, Value>(method, &url, &p.body).await?;
    Ok(Applied::changed(match p.modify_index {
        None => format!("created {key}"),
        Some(_) => format!("updated {key}"),
    }))
}

pub(crate) async fn remove(cx: &Context, key: &ObjectKey) -> Result<Applied> {
    let Some(kind) = EntityKind::of(key.kind) else {
        return Ok(Applied::skipped(format!("{key} is not an entity")));
    };
    cx.nomad.delete(&kind.get_url(&key.name)).await?;
    Ok(Applied::changed(format!("deleted {key}")))
}

/// The entities stamped by `cx.target`. Quotas are Enterprise-only, so a
/// cluster without them (404 or an error) has none.
pub(crate) async fn owned(cx: &Context) -> Result<Vec<Orphan>> {
    let mut out = Vec::new();
    for kind in [
        EntityKind::Namespace,
        EntityKind::NodePool,
        EntityKind::Quota,
    ] {
        let list = match cx.nomad.get::<Vec<Value>>(kind.list_url()).await {
            Ok(list) => list.unwrap_or_default(),
            Err(_) if kind == EntityKind::Quota => Vec::new(),
            Err(e) => return Err(e),
        };
        for item in list {
            if let Some(s) = read_stamps(kind, &item).filter(|s| s.target == cx.target) {
                out.push(Orphan {
                    key: ObjectKey::new(kind.kind(), "", item["Name"].as_str().unwrap_or_default()),
                    module: s.module,
                    id: None,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(kind: EntityKind, hcl: &str) -> EntitySpec {
        EntitySpec {
            kind,
            file: "x.hcl".into(),
            hcl: hcl.into(),
        }
    }

    #[test]
    fn node_pools_take_their_name_from_the_label() {
        let (name, api) = decode(&spec(
            EntityKind::NodePool,
            r#"node_pool "pis" {
  description = "Raspberry Pi clients"
  meta { arch = "arm64" }
  scheduler_config { memory_oversubscription = "enabled" }
}"#,
        ))
        .unwrap();
        assert_eq!(name, "pis");
        assert_eq!(
            api,
            json!({
                "Name": "pis",
                "Description": "Raspberry Pi clients",
                "Meta": {"arch": "arm64"},
                "SchedulerConfiguration": {"MemoryOversubscriptionEnabled": "enabled"},
            })
        );
    }

    #[test]
    fn quotas_hold_limits_as_a_list() {
        let (_, api) = decode(&spec(
            EntityKind::Quota,
            r#"name = "team-a"
limit {
  region = "global"
  region_limit {
    cpu    = 2500
    memory = 1000
  }
}"#,
        ))
        .unwrap();
        assert_eq!(
            api["Limits"][0]["RegionLimit"],
            json!({"CPU": 2500, "MemoryMB": 1000})
        );
    }

    #[rstest::rstest]
    #[case(EntityKind::Namespace, "description = \"x\"")]
    #[case(EntityKind::NodePool, "node_pool \"a\" {}\nnode_pool \"b\" {}")]
    fn specs_without_a_single_name_are_rejected(#[case] kind: EntityKind, #[case] hcl: &str) {
        assert!(decode(&spec(kind, hcl)).is_err());
    }

    #[test]
    fn unknown_resource_types_list_the_supported_ones() {
        let err = EntityKind::resource("sentinel_policy").unwrap_err();
        assert!(
            buckle_core::error::pretty(&err)
                .to_string()
                .contains("quota")
        );
    }
}
