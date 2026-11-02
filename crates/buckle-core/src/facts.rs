//! Fact providers describe the host. Each provider owns a subtree of the facts
//! document mounted at `host.facts.<NAME>`. A failing provider never aborts a
//! run: it keeps its last good value, is marked stale, and is retried on the
//! next collection. A provider that has never produced a value is
//! *unavailable*; selectors that read its facts are uncertain, and the planner
//! treats modules whose selection depends on them as indeterminate rather than
//! unselected (so their resources are never removed on a guess).

use crate::Platforms;
use crate::task::Cx;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;
use std::time::Duration;

#[async_trait]
pub trait FactProvider: Send + Sync + 'static {
    /// Mount point under `host.facts`, e.g. `os`, `tailscale`, `cloud.oci` (dots nest).
    const NAME: &'static str;
    const TTL: Duration = Duration::from_secs(300);
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "";

    /// Collects the subtree. Errors are logged; the previous value is kept.
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
    /// Providers whose most recent collection failed. Their subtree holds the
    /// last good value, or nothing when they never succeeded.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub stale: BTreeSet<String>,
}

impl Facts {
    pub fn new() -> Self {
        Self {
            tree: serde_json::Value::Object(Default::default()),
            collected_at: HashMap::new(),
            stale: BTreeSet::new(),
        }
    }

    /// Records that `name` failed to collect: its last good value (if any) is
    /// kept and it is marked stale. Its collection time is left alone, so it is
    /// retried on the next collection.
    pub fn mark_failed(&mut self, name: &str) {
        self.stale.insert(name.to_owned());
    }

    /// Providers whose last collection failed and that have no good value to
    /// fall back on: their facts are unknown, not absent.
    pub fn unavailable(&self) -> impl Iterator<Item = &str> {
        self.stale
            .iter()
            .filter(|name| !self.collected_at.contains_key(name.as_str()))
            .map(String::as_str)
    }

    /// Whether the dotted fact path `path` (below `host.facts.`) lies under
    /// an unavailable provider, compared case-insensitively.
    pub fn is_unavailable_path(&self, path: &str) -> bool {
        let path = path.to_ascii_lowercase();
        self.unavailable().any(|p| {
            let p = p.to_ascii_lowercase();
            path == p || path.starts_with(&format!("{p}.")) || p.starts_with(&format!("{path}."))
        })
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
        self.stale.remove(name);
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
                tracing::warn!(provider = provider.name(), error = %err.description(), "fact provider failed; keeping its last good value");
                facts.mark_failed(provider.name());
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
    use crate::system::{FilePerms, System};
    use crate::testing::{CxBuilder, FakeSystem};
    use std::path::Path;

    /// Mounts `host.facts.test.flaky_facts` from `/facts/flaky`; fails while
    /// that file says `fail`.
    struct FlakyFacts;

    #[async_trait]
    impl FactProvider for FlakyFacts {
        const NAME: &'static str = "test.flaky_facts";
        async fn collect(&self, cx: &Cx) -> crate::Result<serde_json::Value> {
            let content = cx
                .system
                .read(Path::new("/facts/flaky"))
                .await?
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            if content == "fail" {
                return Err(crate::error::system("IMDS timed out", &["Retry."]));
            }
            Ok(serde_json::json!({ "value": content }))
        }
    }
    crate::register_facts!(FlakyFacts);

    #[tokio::test]
    async fn a_failing_provider_keeps_its_last_good_value_and_is_retried() {
        let only = vec!["test.flaky_facts".to_string()];
        let sys = FakeSystem::new().with_file("/facts/flaky", "good", FilePerms::default());
        let cx = CxBuilder::new(sys.clone()).build();
        let mut facts = Facts::new();
        assert!(
            collect(&cx, &mut facts, Some(&only), false)
                .await
                .is_empty()
        );
        assert_eq!(facts.tree["test"]["flaky_facts"]["value"], "good");
        let first = facts.collected_at["test.flaky_facts"];

        sys.write_atomic(Path::new("/facts/flaky"), b"fail", &FilePerms::default())
            .await
            .unwrap();
        let errors = collect(&cx, &mut facts, Some(&only), true).await;
        assert_eq!(errors.len(), 1);
        assert_eq!(
            facts.tree["test"]["flaky_facts"]["value"], "good",
            "a failure never overwrites the last good value with null"
        );
        assert!(facts.stale.contains("test.flaky_facts"));
        assert_eq!(facts.collected_at["test.flaky_facts"], first);
        assert_eq!(
            facts.unavailable().count(),
            0,
            "a stale value is still a value"
        );

        sys.write_atomic(Path::new("/facts/flaky"), b"better", &FilePerms::default())
            .await
            .unwrap();
        collect(&cx, &mut facts, Some(&only), true).await;
        assert_eq!(facts.tree["test"]["flaky_facts"]["value"], "better");
        assert!(facts.stale.is_empty());
    }

    #[tokio::test]
    async fn a_provider_that_never_succeeded_is_unavailable() {
        let only = vec!["test.flaky_facts".to_string()];
        let sys = FakeSystem::new().with_file("/facts/flaky", "fail", FilePerms::default());
        let cx = CxBuilder::new(sys).build();
        let mut facts = Facts::new();
        collect(&cx, &mut facts, Some(&only), false).await;
        assert!(facts.tree.get("test").is_none(), "nothing is mounted");
        assert_eq!(
            facts.unavailable().collect::<Vec<_>>(),
            vec!["test.flaky_facts"]
        );
        assert!(facts.is_unavailable_path("test.flaky_facts.value"));
        assert!(facts.is_unavailable_path("TEST.Flaky_Facts"));
        assert!(facts.is_unavailable_path("test"));
        assert!(!facts.is_unavailable_path("test.flaky_factsx"));
        assert!(!facts.is_unavailable_path("os.family"));
    }

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
