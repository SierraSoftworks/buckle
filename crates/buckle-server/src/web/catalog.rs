//! `GET /api/v1/catalog`: the kinds registered in this binary.

/// Builds the catalog from the core registries (the binary links the
/// standard library, which registers its kinds).
pub fn build(server_version: &str) -> buckle_api::Catalog {
    let platforms = |p: buckle_core::Platforms| p.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let mut tasks: Vec<_> = buckle_core::task::registry()
        .values()
        .map(|t| buckle_api::KindDescriptor {
            kind: t.kind().to_owned(),
            description: t.description().to_owned(),
            platforms: platforms(t.platforms()),
            locks: t.locks().iter().map(|l| l.to_string()).collect(),
            removal_default: Some(format!("{:?}", t.removal_default()).to_lowercase()),
            removal_caution: t.removal_caution(),
            schema: serde_json::to_value(t.schema()).unwrap_or_default(),
        })
        .collect();
    tasks.sort_by(|a, b| a.kind.cmp(&b.kind));
    let mut facts: Vec<_> = buckle_core::facts::registry()
        .values()
        .map(|f| buckle_api::KindDescriptor {
            kind: f.name().to_owned(),
            description: f.description().to_owned(),
            platforms: platforms(f.platforms()),
            locks: vec![],
            removal_default: None,
            removal_caution: false,
            schema: serde_json::Value::Null,
        })
        .collect();
    facts.sort_by(|a, b| a.kind.cmp(&b.kind));
    let mut health: Vec<_> = buckle_core::health::registry()
        .values()
        .map(|h| buckle_api::KindDescriptor {
            kind: h.kind().to_owned(),
            description: h.description().to_owned(),
            platforms: platforms(h.platforms()),
            locks: vec![],
            removal_default: None,
            removal_caution: false,
            schema: serde_json::to_value(h.schema()).unwrap_or_default(),
        })
        .collect();
    health.sort_by(|a, b| a.kind.cmp(&b.kind));
    buckle_api::Catalog {
        tasks,
        facts,
        health,
        server_version: server_version.to_owned(),
    }
}
