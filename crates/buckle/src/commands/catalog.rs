//! `buckle catalog`: the registered kinds, as text or as the JSON the server's
//! `/api/v1/catalog` endpoint also returns.

use clap::Args;

#[derive(Args, Debug, Clone)]
pub struct CatalogArgs {
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: CatalogArgs) -> buckle_core::Result<i32> {
    let catalog = build();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&catalog).expect("catalog serialises")
        );
    } else {
        println!("tasks ({}):", catalog.tasks.len());
        for t in &catalog.tasks {
            println!("  {:<22} {}", t.kind, t.description);
        }
        println!("facts ({}):", catalog.facts.len());
        for f in &catalog.facts {
            println!("  {:<22} {}", f.kind, f.description);
        }
        println!("health ({}):", catalog.health.len());
        for h in &catalog.health {
            println!("  {:<22} {}", h.kind, h.description);
        }
    }
    Ok(0)
}

pub fn build() -> buckle_api::Catalog {
    // Linking buckle-std registers the standard kinds; referencing it here keeps
    // the linker from discarding the crate.
    buckle_std::ensure_linked();
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
        server_version: crate::telemetry::version().to_owned(),
    }
}
