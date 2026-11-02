//! Server-side secret resolution, scoped per host (design §06).
//!
//! The server resolves `fleet.secrets` with its own providers and sends each
//! host only the secrets referenced by the modules (and vars overlays) its
//! labels, facts and groups select. A `when` that tests a secret is
//! evaluated here, with the value resolved on the server, so a module's
//! secrets go only to the hosts the module actually selects.

use crate::config::{EnvProviderConfig, SecretsConfig};
use buckle_core::engine::Revision;
use buckle_core::secrets::providers::{ExecProvider, FileProvider, ScopedEnvProvider};
use buckle_core::secrets::sinks::TransportSink;
use buckle_core::secrets::{InstallKey, Resolver, SecretStore};
use buckle_core::selectors::HostContext;
use buckle_proto::v1::{SecretBundle, SecretValue};
use std::collections::BTreeSet;
use std::sync::Arc;

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
            if file.root.is_none() {
                tracing::warn!(
                    "secrets.providers.file has no root: fleet secrets may read any file the server can"
                );
            }
            resolver = resolver.with_provider(FileProvider {
                root: file.root.clone(),
            });
        }
        if let Some(env) = &config.providers.env {
            let mut deny: Vec<String> = EnvProviderConfig::ALWAYS_DENIED
                .iter()
                .map(|s| (*s).to_owned())
                .collect();
            deny.extend(config.reserved_env.iter().cloned());
            deny.extend(env.deny.iter().cloned());
            resolver = resolver.with_provider(ScopedEnvProvider {
                allow: env.allow.clone(),
                deny,
            });
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

    /// Names of secret references in `text` (`secrets.NAME`,
    /// `secrets["NAME"]`), as the revision validator finds them.
    pub fn references(text: &str) -> BTreeSet<String> {
        buckle_core::engine::validate::secret_references(text)
    }

    /// The declared secrets that some module's or vars overlay's `when`
    /// tests.
    fn condition_names(revision: &Revision) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for module in &revision.modules {
            if let Some(when) = &module.config.when {
                names.extend(Self::references(when.raw()));
            }
        }
        for overlay in &revision.fleet.vars {
            if let Some(when) = &overlay.when {
                names.extend(Self::references(when.raw()));
            }
        }
        names.retain(|n| revision.fleet.secrets.contains_key(n));
        names
    }

    /// The declared secrets the host's selected modules and matching vars
    /// overlays reference, with every `when` evaluated on the server: the
    /// secrets a condition tests are resolved here first (one that cannot
    /// be resolved is absent, as it would be on the agent).
    pub async fn scoped_names(&self, revision: &Revision, ctx: &HostContext) -> BTreeSet<String> {
        let conditions = Self::condition_names(revision);
        if conditions.is_empty() {
            return Self::scoped_names_with(revision, ctx);
        }
        let store = self.resolve(revision, &conditions).await;
        let ctx = ctx.clone().with_secrets(Arc::new(store));
        Self::scoped_names_with(revision, &ctx)
    }

    /// [`Self::scoped_names`] against a context that already carries the
    /// secrets the conditions test (or deliberately none). A module whose
    /// `when` reads facts the server has not received from the host yet is
    /// counted as selected: the agent decides with the real facts, and
    /// withholding the secret would fail its first plan
    /// ([`buckle_core::engine::secret_names_for_host`]).
    pub fn scoped_names_with(revision: &Revision, ctx: &HostContext) -> BTreeSet<String> {
        buckle_core::engine::secret_names_for_host(revision, ctx)
    }

    /// Resolves `names` (which must be declared by the revision). Secrets
    /// that fail to resolve are left out and logged by name.
    async fn resolve(&self, revision: &Revision, names: &BTreeSet<String>) -> SecretStore {
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
        store
    }

    /// Resolves `names` (which must be declared by the revision) into a bundle.
    /// Secrets that fail to resolve are left out and logged by name.
    pub async fn bundle(&self, revision: &Revision, names: &BTreeSet<String>) -> SecretBundle {
        let store = self.resolve(revision, names).await;
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
        let names = self.scoped_names(revision, ctx).await;
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
        let allowed = self.scoped_names(revision, ctx).await;
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
    use crate::config::{EnvProviderConfig, FileProviderConfig, ProvidersConfig};
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
        let names = ServerSecrets::scoped_names_with(&rev, &ctx);
        assert_eq!(names.into_iter().collect::<Vec<_>>(), vec!["nomad_token"]);

        let server = ServerSecrets::new(
            &SecretsConfig {
                providers: ProvidersConfig {
                    file: Some(FileProviderConfig { root: None }),
                    env: Some(EnvProviderConfig::default()),
                    exec: None,
                },
                ..Default::default()
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
    fn references_are_found_by_word_and_subscript() {
        let refs = ServerSecrets::references(
            "a {{ secrets.one }} b secrets.two.present c {{ secrets[\"three\"] }} mysecrets.four",
        );
        assert_eq!(
            refs.into_iter().collect::<Vec<_>>(),
            vec!["one", "three", "two"]
        );
    }

    fn file_server(root: &std::path::Path) -> ServerSecrets {
        ServerSecrets::new(
            &SecretsConfig {
                providers: ProvidersConfig {
                    file: Some(FileProviderConfig {
                        root: Some(root.to_path_buf()),
                    }),
                    env: Some(EnvProviderConfig::default()),
                    exec: None,
                },
                reserved_env: ["SERVER_ONLY_TEST_VAR".to_string()].into(),
            },
            InstallKey::from_bytes([3; 32]),
        )
    }

    #[tokio::test]
    async fn a_condition_on_a_secret_is_evaluated_on_the_server() {
        let repo = tempfile::tempdir().unwrap();
        let secrets = tempfile::tempdir().unwrap();
        std::fs::write(secrets.path().join("license"), "pro").unwrap();
        std::fs::write(secrets.path().join("pro_key"), "k").unwrap();
        std::fs::write(secrets.path().join("ent_key"), "e").unwrap();
        write(
            repo.path(),
            "buckle.yaml",
            "version: 2\nsecrets:\n  license: { file: license }\n  pro_key: { file: pro_key }\n  ent_key: { file: ent_key }\n  missing: { file: nope }\n",
        );
        write(
            repo.path(),
            "modules/pro/module.yaml",
            "name: pro\nwhen: 'secrets.license == \"pro\"'\ntasks:\n  - command.run: { run: [\"x\"], env: { K: \"{{ secrets.pro_key }}\" } }\n",
        );
        write(
            repo.path(),
            "modules/ent/module.yaml",
            "name: ent\nwhen: 'secrets.license == \"enterprise\"'\ntasks:\n  - command.run: { run: [\"x\"], env: { K: \"{{ secrets.ent_key }}\" } }\n",
        );
        write(
            repo.path(),
            "modules/opt/module.yaml",
            "name: opt\nwhen: 'secrets.missing.present'\ntasks:\n  - command.run: { run: [\"x\"], env: { K: \"{{ secrets.missing }}\" } }\n",
        );
        let rev = buckle_core::engine::loader::load_dir(repo.path(), RevisionId::new("r")).unwrap();
        let ctx = crate::fleet::host_context(
            "pi-1",
            &BTreeMap::new(),
            &serde_json::Value::Null,
            &rev.fleet,
        );
        let server = file_server(secrets.path());
        let names = server.scoped_names(&rev, &ctx).await;
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            vec!["license", "pro_key"],
            "only the module whose condition holds ships its secrets"
        );
    }

    #[tokio::test]
    async fn the_servers_own_environment_is_not_a_secret_source() {
        // SAFETY: the variables are unique to this test.
        unsafe {
            std::env::set_var("SERVER_ONLY_TEST_VAR", "server-credential");
            std::env::set_var("BUCKLE_SECRETS_TEST_VAR", "server-credential");
            std::env::set_var("FLEET_SECRETS_TEST_VAR", "fleet-value");
        }
        let repo = tempfile::tempdir().unwrap();
        write(
            repo.path(),
            "buckle.yaml",
            "version: 2\nsecrets:\n  own: { env: SERVER_ONLY_TEST_VAR }\n  prefixed: { env: BUCKLE_SECRETS_TEST_VAR }\n  fleet: { env: FLEET_SECRETS_TEST_VAR }\n",
        );
        let rev = buckle_core::engine::loader::load_dir(repo.path(), RevisionId::new("r")).unwrap();
        let server = file_server(repo.path());
        let names: BTreeSet<String> = ["own", "prefixed", "fleet"]
            .into_iter()
            .map(String::from)
            .collect();
        let bundle = server.bundle(&rev, &names).await;
        let got: Vec<_> = bundle.secrets.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(got, vec!["fleet"]);
    }
}
