//! Fact providers describe the host. Each provider owns a subtree of the facts
//! document mounted at `host.facts.<NAME>`; a failing provider yields `null`
//! under its name and never aborts a run.

use crate::Platforms;
use crate::task::Cx;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

#[async_trait]
pub trait FactProvider: Send + Sync + 'static {
    /// Mount point under `host.facts`, e.g. `os`, `tailscale`, `cloud.oci` (dots nest).
    const NAME: &'static str;
    const TTL: Duration = Duration::from_secs(300);
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "";

    /// Collects the subtree. Errors are logged and recorded as `null`.
    async fn collect(&self, cx: &Cx) -> crate::Result<serde_json::Value>;
}

#[async_trait]
pub trait DynFactProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn ttl(&self) -> Duration;
    fn platforms(&self) -> Platforms;
    fn description(&self) -> &'static str;
    async fn collect(&self, cx: &Cx) -> crate::Result<serde_json::Value>;
}

#[async_trait]
impl<P: FactProvider> DynFactProvider for P {
    fn name(&self) -> &'static str {
        P::NAME
    }
    fn ttl(&self) -> Duration {
        P::TTL
    }
    fn platforms(&self) -> Platforms {
        P::PLATFORMS
    }
    fn description(&self) -> &'static str {
        P::DESCRIPTION
    }
    async fn collect(&self, cx: &Cx) -> crate::Result<serde_json::Value> {
        FactProvider::collect(self, cx).await
    }
}

pub struct FactRegistration(pub &'static dyn DynFactProvider);

impl FactRegistration {
    pub const fn new<P: DynFactProvider>(p: &'static P) -> Self {
        Self(p)
    }
}

inventory::collect!(FactRegistration);

pub fn registry() -> &'static HashMap<&'static str, &'static dyn DynFactProvider> {
    static REGISTRY: LazyLock<HashMap<&'static str, &'static dyn DynFactProvider>> =
        LazyLock::new(|| {
            let mut map = HashMap::new();
            for reg in inventory::iter::<FactRegistration> {
                if map.insert(reg.0.name(), reg.0).is_some() {
                    panic!("two fact providers are registered as '{}'", reg.0.name());
                }
            }
            map
        });
    &REGISTRY
}

/// A collected facts document with per-provider timestamps for TTL handling.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Facts {
    pub tree: serde_json::Value,
    #[serde(default)]
    pub collected_at: HashMap<String, chrono::DateTime<chrono::Utc>>,
}

impl Facts {
    pub fn new() -> Self {
        Self {
            tree: serde_json::Value::Object(Default::default()),
            collected_at: HashMap::new(),
        }
    }

    /// Mounts `value` at the dotted `name`, creating intermediate objects.
    pub fn mount(
        &mut self,
        name: &str,
        value: serde_json::Value,
        at: chrono::DateTime<chrono::Utc>,
    ) {
        let mut current = &mut self.tree;
        let parts: Vec<&str> = name.split('.').collect();
        for (i, part) in parts.iter().enumerate() {
            if !current.is_object() {
                *current = serde_json::Value::Object(Default::default());
            }
            let map = current.as_object_mut().expect("ensured object");
            if i == parts.len() - 1 {
                map.insert((*part).to_owned(), value);
                break;
            }
            current = map
                .entry(*part)
                .or_insert_with(|| serde_json::Value::Object(Default::default()));
        }
        self.collected_at.insert(name.to_owned(), at);
    }

    /// Whether a provider's subtree is older than its TTL (or missing).
    pub fn is_stale(&self, name: &str, ttl: Duration, now: chrono::DateTime<chrono::Utc>) -> bool {
        match self.collected_at.get(name) {
            Some(at) => (now - *at).to_std().map(|age| age > ttl).unwrap_or(true),
            None => true,
        }
    }
}

/// Collects (or refreshes) the named providers into `facts`. `only` restricts to
/// specific providers (for `facts.provides` refreshes); `None` means all
/// registered providers supporting the current platform whose TTL expired.
pub async fn collect(
    cx: &Cx,
    facts: &mut Facts,
    only: Option<&[String]>,
    force: bool,
) -> Vec<FactError> {
    let now = cx.system.now();
    let platform = cx.platform();
    let mut errors = Vec::new();
    let mut providers: Vec<&'static dyn DynFactProvider> = registry()
        .values()
        .copied()
        .filter(|p| p.platforms().contains(platform))
        .filter(|p| only.is_none_or(|names| names.iter().any(|n| n == p.name())))
        .collect();
    providers.sort_by_key(|p| p.name());
    for provider in providers {
        if !force && !facts.is_stale(provider.name(), provider.ttl(), now) {
            continue;
        }
        let span = tracing::info_span!("facts.collect", provider = provider.name());
        let _enter = span.enter();
        match provider.collect(cx).await {
            Ok(value) => facts.mount(provider.name(), value, now),
            Err(err) => {
                tracing::warn!(provider = provider.name(), error = %err.description(), "fact provider failed; recording null");
                facts.mount(provider.name(), serde_json::Value::Null, now);
                errors.push(FactError {
                    provider: provider.name().to_owned(),
                    message: err.description(),
                });
            }
        }
    }
    errors
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactError {
    pub provider: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_nests_dotted_names_and_tracks_time() {
        let mut facts = Facts::new();
        let t0 = chrono::Utc::now();
        facts.mount("os", serde_json::json!({"family": "debian"}), t0);
        facts.mount("cloud.oci", serde_json::json!({"region": "eu"}), t0);
        assert_eq!(facts.tree["os"]["family"], "debian");
        assert_eq!(facts.tree["cloud"]["oci"]["region"], "eu");
        assert!(!facts.is_stale(
            "os",
            Duration::from_secs(60),
            t0 + chrono::Duration::seconds(30)
        ));
        assert!(facts.is_stale(
            "os",
            Duration::from_secs(60),
            t0 + chrono::Duration::seconds(90)
        ));
        assert!(facts.is_stale("missing", Duration::from_secs(60), t0));
    }

    #[test]
    fn mount_replaces_a_scalar_with_an_object_when_nesting_deeper() {
        let mut facts = Facts::new();
        let t0 = chrono::Utc::now();
        facts.mount("cloud", serde_json::Value::Null, t0);
        facts.mount("cloud.azure", serde_json::json!({"sku": "B2s"}), t0);
        assert_eq!(facts.tree["cloud"]["azure"]["sku"], "B2s");
    }
}
