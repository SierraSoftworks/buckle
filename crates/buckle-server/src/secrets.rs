//! Server-side secret resolution, scoped per host (design §06).
//!
//! The server resolves `fleet.secrets` with its own providers and sends each
//! host only the secrets referenced by the modules (and vars overlays) its
//! labels, facts and groups select.

use crate::config::SecretsConfig;
use buckle_core::engine::Revision;
use buckle_core::secrets::providers::{EnvProvider, ExecProvider, FileProvider};
use buckle_core::secrets::sinks::TransportSink;
use buckle_core::secrets::{InstallKey, Resolver, SecretStore};
use buckle_core::selectors::HostContext;
use buckle_proto::v1::{SecretBundle, SecretValue};
use regex::Regex;
use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock};

static SECRET_REF: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r"secrets\.([A-Za-z0-9_]+)").ok());

/// Resolves and scopes secrets for hosts.
#[derive(Clone)]
pub struct ServerSecrets {
    resolver: Arc<Resolver>,
}

impl std::fmt::Debug for ServerSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServerSecrets(..)")
    }
}

impl ServerSecrets {
    /// Builds a resolver from `secrets.providers` in server.yaml.
    pub fn new(config: &SecretsConfig, key: InstallKey) -> Self {
        let mut resolver = Resolver::new(key);
        if let Some(file) = &config.providers.file {
            resolver = resolver.with_provider(FileProvider {
                root: file.root.clone(),
            });
        }
        if config.providers.env.is_some() {
            resolver = resolver.with_provider(EnvProvider);
        }
        if let Some(exec) = &config.providers.exec {
            tracing::warn!(
                allow = ?exec.allow,
                "the exec secret provider is enabled; it is meant for local development only"
            );
            resolver = resolver.with_provider(ExecProvider::new(exec.allow.clone()));
        }
        Self {
            resolver: Arc::new(resolver),
        }
    }

    /// Names of `secrets.NAME` references in `text`.
    pub fn references(text: &str) -> BTreeSet<String> {
        match SECRET_REF.as_ref() {
            Some(re) => re
                .captures_iter(text)
                .filter_map(|c| c.get(1).map(|m| m.as_str().to_owned()))
                .collect(),
            None => BTreeSet::new(),
        }
    }

    /// The declared secrets the host's selected modules and matching vars
    /// overlays reference. A module whose `when` itself mentions a secret is
    /// included regardless of its selector, since the selector cannot be
    /// evaluated without the value.
    pub fn scoped_names(revision: &Revision, ctx: &HostContext) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for module in &revision.modules {
            let cfg = &module.config;
            let selected = match &cfg.when {
                None => true,
                Some(when) => {
                    when.raw().contains("secrets.")
                        || ctx.in_module(&cfg.name, &cfg.tags).matches(when)
                }
            };
            if !selected {
                continue;
            }
            if let Ok(yaml) = serde_yaml::to_string(cfg) {
                names.extend(Self::references(&yaml));
            }
            for (_, template) in &module.templates {
                names.extend(Self::references(template));
            }
        }
        for overlay in &revision.fleet.vars {
            if overlay.when.as_ref().is_some_and(|w| !ctx.matches(w)) {
                continue;
            }
            if let Ok(yaml) = serde_yaml::to_string(&overlay.values) {
                names.extend(Self::references(&yaml));
            }
            if let Some(file) = &overlay.file
                && let Some(value) = revision.var_files.get(file)
                && let Ok(yaml) = serde_yaml::to_string(value)
            {
                names.extend(Self::references(&yaml));
            }
        }
        names.retain(|n| revision.fleet.secrets.contains_key(n));
        names
    }

    /// Resolves `names` (which must be declared by the revision) into a bundle.
    /// Secrets that fail to resolve are left out and logged by name.
    pub async fn bundle(&self, revision: &Revision, names: &BTreeSet<String>) -> SecretBundle {
        let mut store = SecretStore::new();
        for name in names {
            let Some(reference) = revision.fleet.secrets.get(name) else {
                continue;
            };
            match self.resolver.resolve_one(name, reference).await {
                Ok(secret) => store.insert(secret),
                Err(e) => {
                    tracing::warn!(secret = %name, error = %e, "a secret could not be resolved")
                }
            }
        }
        SecretBundle {
            secrets: store
                .export(&TransportSink)
                .into_iter()
                .map(|(name, fingerprint, value)| SecretValue {
                    name,
                    value,
                    fingerprint: fingerprint.as_hex(),
                })
                .collect(),
        }
    }

    /// The bundle a host's Welcome/Assign carries for `revision`.
    pub async fn for_host(&self, revision: &Revision, ctx: &HostContext) -> SecretBundle {
        let names = Self::scoped_names(revision, ctx);
        self.bundle(revision, &names).await
    }

    /// A FetchSecrets answer: only names the revision declares and the
    /// host's selected modules reference.
    pub async fn fetch(
        &self,
        revision: &Revision,
        ctx: &HostContext,
        requested: &[String],
    ) -> SecretBundle {
        let allowed = Self::scoped_names(revision, ctx);
        let names: BTreeSet<String> = requested
            .iter()
            .filter(|n| allowed.contains(*n))
            .cloned()
            .collect();
        self.bundle(revision, &names).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EmptyConfig, FileProviderConfig, ProvidersConfig};
    use buckle_core::plan::RevisionId;
    use std::collections::BTreeMap;

    fn write(root: &std::path::Path, rel: &str, content: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[tokio::test]
    async fn hosts_get_only_the_secrets_their_modules_reference() {
        let repo = tempfile::tempdir().unwrap();
        let secrets = tempfile::tempdir().unwrap();
        std::fs::write(secrets.path().join("nomad_token"), "nomad-value\n").unwrap();
        std::fs::write(secrets.path().join("backup_key"), "backup-value").unwrap();
        write(
            repo.path(),
            "buckle.yaml",
            &format!(
                "version: 2\nfleet:\n  groups:\n    nomad: 'host.labels.role == \"worker\"'\nsecrets:\n  nomad_token: {{ file: {0}/nomad_token }}\n  backup_key: {{ file: {0}/backup_key }}\n  unused: {{ env: NOPE }}\n",
                secrets.path().display()
            ),
        );
        write(
            repo.path(),
            "modules/nomad/module.yaml",
            "name: nomad\nwhen: 'host.groups contains \"nomad\"'\ntasks:\n  - file.template: { src: nomad.hcl.j2, dest: /etc/nomad.hcl }\n",
        );
        write(
            repo.path(),
            "modules/nomad/files/nomad.hcl.j2",
            "token = \"{{ secrets.nomad_token }}\"\n",
        );
        write(
            repo.path(),
            "modules/backup/module.yaml",
            "name: backup\nwhen: 'host.labels.backup == true'\ntasks:\n  - command.run: { run: [\"rustic\"], env: { KEY: \"{{ secrets.backup_key }}\" } }\n",
        );
        let rev = buckle_core::engine::loader::load_dir(repo.path(), RevisionId::new("r")).unwrap();
        let mut labels = BTreeMap::new();
        labels.insert("role".to_string(), "worker".to_string());
        let ctx = crate::fleet::host_context("pi-1", &labels, &serde_json::Value::Null, &rev.fleet);
        let names = ServerSecrets::scoped_names(&rev, &ctx);
        assert_eq!(names.into_iter().collect::<Vec<_>>(), vec!["nomad_token"]);

        let server = ServerSecrets::new(
            &SecretsConfig {
                providers: ProvidersConfig {
                    file: Some(FileProviderConfig { root: None }),
                    env: Some(EmptyConfig {}),
                    exec: None,
                },
            },
            InstallKey::from_bytes([3; 32]),
        );
        let bundle = server.for_host(&rev, &ctx).await;
        assert_eq!(bundle.secrets.len(), 1);
        assert_eq!(bundle.secrets[0].name, "nomad_token");
        assert_eq!(bundle.secrets[0].value, b"nomad-value");
        assert_eq!(bundle.secrets[0].fingerprint.len(), 8);

        let fetched = server
            .fetch(
                &rev,
                &ctx,
                &["backup_key".into(), "nomad_token".into(), "unused".into()],
            )
            .await;
        let names: Vec<_> = fetched.secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["nomad_token"],
            "only referenced, declared names are answered"
        );
    }

    #[test]
    fn references_are_found_textually() {
        let refs = ServerSecrets::references("a {{ secrets.one }} b secrets.two.present c");
        assert_eq!(refs.into_iter().collect::<Vec<_>>(), vec!["one", "two"]);
    }
}
