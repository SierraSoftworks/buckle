//! Local mode: the agent watches a configuration directory instead of talking
//! to a server (design §08, last paragraph). Every settled change (2 s
//! debounce) becomes an Assign of a synthetic revision whose id is the digest
//! of the tree's contents, followed by RequestPlan and an automatic Apply.
//! Reports are logged.
//!
//! With no server to deliver them, the agent resolves the fleet's `secrets:`
//! itself (design §06, "In local mode the agent resolves with its own
//! providers"): the `env` and `file` providers, plus `exec` for the programs
//! allowed on the command line.

use super::{Hello, Instruction, Report, RevisionSource, Session, Transport, Welcome, WireSecret};
use crate::bundle::tree_digest;
use async_trait::async_trait;
use buckle_core::plan::RevisionId;
use buckle_core::secrets::providers::{EnvProvider, ExecProvider, FileProvider};
use buckle_core::secrets::sinks::TransportSink;
use buckle_core::secrets::{InstallKey, Resolver, SecretStore};
use notify::Watcher;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

/// The default quiet period before a change is applied.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(2);

/// Watches a directory and applies it on change.
pub struct LocalTransport {
    source: PathBuf,
    debounce: Duration,
    secrets: Option<std::sync::Arc<Resolver>>,
}

impl LocalTransport {
    /// Watches `source` with the default debounce.
    pub fn new(source: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            debounce: DEFAULT_DEBOUNCE,
            secrets: None,
        }
    }

    /// Resolves the fleet's secrets with the `env` and `file` providers
    /// (fingerprinted under `key`), and `exec` for the programs in
    /// `exec_allow` when it is not empty.
    pub fn with_secrets(mut self, key: InstallKey, exec_allow: Vec<String>) -> Self {
        let mut resolver = Resolver::new(key)
            .with_provider(EnvProvider)
            .with_provider(FileProvider::default());
        if !exec_allow.is_empty() {
            resolver = resolver.with_provider(ExecProvider::new(exec_allow));
        }
        self.secrets = Some(std::sync::Arc::new(resolver));
        self
    }

    /// Uses another debounce period.
    pub fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }
}

/// Logs a report the way local mode shows it.
pub fn log_report(report: &Report) {
    match report {
        Report::For { target, report } => {
            let _span = tracing::info_span!("target", %target).entered();
            log_report(report)
        }
        Report::Facts(_) => tracing::debug!("facts collected"),
        Report::Heartbeat { busy, .. } => tracing::debug!(busy, "heartbeat"),
        Report::Plan(plan) => tracing::info!(
            revision = %plan.revision.short(),
            digest = %plan.digest.short(),
            changes = plan.changes(),
            removals = plan.removals.len(),
            "planned"
        ),
        Report::PlanFailed {
            revision, error, ..
        } => {
            tracing::error!(revision = %revision.short(), %error, "planning failed")
        }
        Report::StalePlan { plan, .. } => {
            tracing::warn!(revision = %plan.revision.short(), "the plan changed before it could be applied")
        }
        Report::ApplyEvent { event, .. } => tracing::info!(event = ?event, "apply"),
        Report::ApplyResult {
            revision, outcome, ..
        } => {
            if outcome.succeeded {
                tracing::info!(
                    revision = %revision.short(),
                    changes = outcome.changes,
                    removals = outcome.removals,
                    duration_ms = outcome.duration_ms,
                    "applied"
                )
            } else {
                tracing::error!(
                    revision = %revision.short(),
                    failed_module = ?outcome.failed_module,
                    rolled_back = ?outcome.rolled_back_modules,
                    error = ?outcome.error,
                    "apply failed"
                )
            }
        }
        Report::Health { report, .. } => {
            let failures: Vec<String> = report
                .failures(&[])
                .map(|r| format!("{}/{}", r.module, r.check))
                .collect();
            tracing::info!(
                scope = ?report.scope,
                checks = report.results.len(),
                failing = ?failures,
                "health checked"
            )
        }
        Report::Drift {
            revision,
            plan,
            corrected,
        } => match plan {
            Some(plan) => tracing::warn!(
                revision = %revision.short(),
                changes = plan.changes(),
                corrected,
                "drift detected"
            ),
            None => tracing::info!(revision = %revision.short(), "no drift"),
        },
        Report::Upgrade { version, outcome } => {
            tracing::info!(%version, outcome = ?outcome, "upgrade")
        }
        Report::RebootFailed { error, .. } => {
            tracing::error!(%error, "the reboot could not be started")
        }
        Report::LocalRollback(record) => tracing::warn!(
            from = %record.from.short(),
            to = %record.to.short(),
            reason = %record.reason,
            "rolled back locally"
        ),
    }
}

/// Resolves `names` (every declared secret when `None`) from the
/// `secrets:` of the buckle.yaml in `source`. A secret that cannot be
/// resolved is logged and left out, so the modules that use it fail to plan
/// with a message naming it.
async fn resolve_secrets(
    resolver: &Resolver,
    source: &std::path::Path,
    names: Option<&[String]>,
) -> buckle_core::Result<Vec<WireSecret>> {
    use buckle_core::error::HumanizableResult;
    let text = std::fs::read_to_string(source.join("buckle.yaml")).to_human_error()?;
    let fleet = buckle_core::model::FleetConfig::parse(&text)?;
    let mut store = SecretStore::new();
    for (name, reference) in &fleet.secrets {
        if names.is_some_and(|n| !n.contains(name)) {
            continue;
        }
        match resolver.resolve_one(name, reference).await {
            Ok(secret) => store.insert(secret),
            Err(err) => {
                tracing::warn!(secret = %name, error = %err.description(), "a secret could not be resolved")
            }
        }
    }
    Ok(store
        .export(&TransportSink)
        .into_iter()
        .map(|(name, fingerprint, value)| WireSecret {
            name,
            value,
            fingerprint: fingerprint.as_hex(),
        })
        .collect())
}

fn assignment(revision: RevisionId, secrets: Vec<WireSecret>) -> [Instruction; 3] {
    [
        Instruction::Assign {
            revision: revision.clone(),
            stable: true,
            secrets,
            tree_digest: None,
            target: None,
            cluster: None,
        },
        Instruction::RequestPlan {
            revision: revision.clone(),
            select: None,
            target: None,
        },
        Instruction::Apply {
            revision,
            plan_digest: None,
            approval_digest: None,
            waived_checks: Vec::new(),
            force: false,
            traceparent: None,
            select: None,
            rollout_id: None,
            phase: None,
            target: None,
        },
    ]
}

#[async_trait]
impl Transport for LocalTransport {
    async fn connect(&self, _hello: Hello) -> buckle_core::Result<Session> {
        let source = self.source.clone();
        if !source.join("buckle.yaml").is_file() {
            return Err(buckle_core::error::user(
                format!("{} does not contain a buckle.yaml.", source.display()),
                &["Point `source` in agent.yaml at a Buckle configuration directory."],
            ));
        }
        let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<()>();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            match event {
                Ok(event) if !event.kind.is_access() => {
                    let _ = fs_tx.send(());
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(error = %err, "the configuration watcher reported an error"),
            }
        })
        .map_err(|e| {
            buckle_core::error::wrap_system(
                e,
                "The agent could not start watching its configuration directory.",
                &["Check that the platform supports file watching (inotify, FSEvents or ReadDirectoryChangesW)."],
            )
        })?;
        watcher
            .watch(&source, notify::RecursiveMode::Recursive)
            .map_err(|e| {
                buckle_core::error::wrap_user(
                    e,
                    format!("The agent could not watch {}.", source.display()),
                    &["Check that the directory exists and that the agent may read it."],
                )
            })?;

        let (instruction_tx, instruction_rx) = mpsc::channel::<Instruction>(64);
        let (report_tx, mut report_rx) = mpsc::channel::<Report>(256);
        let acks = instruction_tx.clone();
        tokio::spawn(async move {
            while let Some(report) = report_rx.recv().await {
                log_report(&report);
                // Logging is all local mode does with a report, so durable
                // ones are acknowledged as soon as they are logged.
                if let Some(id) = report.report_id() {
                    let _ = acks
                        .send(Instruction::Ack {
                            report_ids: vec![id],
                        })
                        .await;
                }
            }
        });

        let debounce = self.debounce;
        let resolver = self.secrets.clone();
        tokio::spawn(async move {
            let _watcher = watcher;
            let mut last: Option<String> = None;
            let welcome = Instruction::Welcome(Welcome {
                server_version: "local".into(),
                session_id: "local".into(),
                status: "active".into(),
                ..Default::default()
            });
            if instruction_tx.send(welcome).await.is_err() {
                return;
            }
            loop {
                match tree_digest(&source) {
                    Ok(digest) if last.as_deref() != Some(digest.as_str()) => {
                        tracing::info!(revision = %&digest[..8], "configuration changed");
                        last = Some(digest.clone());
                        let secrets = match &resolver {
                            Some(resolver) => resolve_secrets(resolver, &source, None)
                                .await
                                .unwrap_or_else(|err| {
                                    tracing::warn!(error = %err.description(), "could not resolve the configuration's secrets");
                                    Vec::new()
                                }),
                            None => Vec::new(),
                        };
                        for instruction in assignment(RevisionId(digest), secrets) {
                            if instruction_tx.send(instruction).await.is_err() {
                                return;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::warn!(error = %err, "could not read the configuration directory")
                    }
                }
                // Wait for a change, then for the tree to settle.
                tokio::select! {
                    event = fs_rx.recv() => if event.is_none() { return },
                    _ = instruction_tx.closed() => return,
                }
                loop {
                    match tokio::time::timeout(debounce, fs_rx.recv()).await {
                        Ok(Some(())) => continue,
                        Ok(None) => return,
                        Err(_) => break,
                    }
                }
            }
        });

        Ok(Session {
            instructions: instruction_rx,
            reports: report_tx,
        })
    }

    async fn fetch_revision(
        &self,
        _revision: &RevisionId,
        _tree_digest: Option<&str>,
    ) -> buckle_core::Result<RevisionSource> {
        Ok(RevisionSource::Directory(self.source.clone()))
    }

    async fn fetch_secrets(
        &self,
        _revision: &RevisionId,
        names: &[String],
    ) -> buckle_core::Result<Vec<WireSecret>> {
        match &self.secrets {
            Some(resolver) => resolve_secrets(resolver, &self.source, Some(names)).await,
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_mode_resolves_declared_secrets_with_the_file_provider() {
        let source = tempfile::tempdir().expect("tempdir");
        let vault = tempfile::tempdir().expect("tempdir");
        let path = vault.path().join("db");
        std::fs::write(&path, "hunter2\n").expect("write");
        std::fs::write(
            source.path().join("buckle.yaml"),
            format!(
                "version: 2\nsecrets:\n  db: {{ file: {} }}\n  gone: {{ file: {} }}\n",
                path.display(),
                vault.path().join("missing").display()
            ),
        )
        .expect("write");
        let transport = LocalTransport::new(source.path())
            .with_secrets(InstallKey::from_bytes([3; 32]), Vec::new());
        let got = transport
            .fetch_secrets(
                &RevisionId::new("r"),
                &["db".to_string(), "gone".to_string()],
            )
            .await
            .expect("resolved");
        assert_eq!(got.len(), 1, "a missing secret is left out");
        assert_eq!(got[0].name, "db");
        assert_eq!(got[0].value, b"hunter2", "one trailing newline trimmed");
        assert_eq!(got[0].fingerprint.len(), 8);
        // Without a resolver (the default), nothing is resolved.
        assert!(
            LocalTransport::new(source.path())
                .fetch_secrets(&RevisionId::new("r"), &["db".to_string()])
                .await
                .expect("ok")
                .is_empty()
        );
    }
}
