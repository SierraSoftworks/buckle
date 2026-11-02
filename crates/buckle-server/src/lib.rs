//! The Buckle server: watches a Git repository, turns commits into revisions,
//! orchestrates phased rollouts across the fleet, speaks gRPC to agents and
//! serves the HTTP API and the embedded UI to operators.
//!
//! The pieces, each usable on its own (and tested that way):
//!
//! * [`store`] — SQLite persistence behind one connection.
//! * [`source`] — the Git / directory watchers that build revisions.
//! * [`rollout`] — the persisted rollout controller, driving hosts through a
//!   [`rollout::HostLink`] with time from a [`clock::Clock`].
//! * [`grpc`] — the agent-facing `Fleet` service (token and Tailscale auth).
//! * [`web`] — the operator API, SSE, OIDC sign-in and the embedded UI.

pub mod clock;
pub mod config;
pub mod events;
pub mod fleet;
pub mod grpc;
pub mod rollout;
pub mod secrets;
pub mod source;
pub mod store;
pub mod views;
pub mod web;

pub use config::ServerConfig;

use buckle_core::error::HumanizableResult;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The server's version, as agents and the UI see it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Loads (creating on first start) the 32-byte install key at
/// `data_dir/install.key`, used for secret fingerprints and approval tokens.
pub fn install_key(data_dir: &std::path::Path) -> buckle_core::Result<[u8; 32]> {
    let path = data_dir.join("install.key");
    buckle_core::secrets::InstallKey::load_or_create(&path)?;
    let bytes = std::fs::read(&path).to_human_error()?;
    bytes.try_into().map_err(|_| {
        buckle_core::error::system(
            format!("The install key at {} is corrupt.", path.display()),
            &["Delete the file to generate a new key; secret fingerprints will change."],
        )
    })
}

/// The SHA-256 fingerprint of the gRPC certificate (generating it in `auto`
/// mode), or `None` when TLS is off. For `buckle server --print-fingerprint`.
pub fn fingerprint(config: &ServerConfig) -> buckle_core::Result<Option<String>> {
    Ok(grpc::tls::load(&config.listen.tls, &config.data_dir)?.map(|i| i.fingerprint))
}

/// Runs the server until shutdown is requested. Returns the process exit code.
pub async fn run(config: ServerConfig, shutdown: CancellationToken) -> buckle_core::Result<i32> {
    // Several TLS stacks are linked (tonic, reqwest); pin the process-wide
    // rustls provider so neither has to guess. Already installed is fine.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    std::fs::create_dir_all(&config.data_dir).map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            format!(
                "Could not create the data directory {}.",
                config.data_dir.display()
            ),
            &["Check `data_dir` in server.yaml and the server's permissions."],
        )
    })?;
    let store = store::Store::open(&config.data_dir.join("buckle.db")).await?;
    store.call(|db| db.hosts().mark_all_offline()).await?;
    let key = install_key(&config.data_dir)?;
    let secrets = secrets::ServerSecrets::new(
        &config.secrets,
        buckle_core::secrets::InstallKey::from_bytes(key),
    );
    let revisions = Arc::new(source::cache::RevisionCache::new(&config.data_dir));
    let events = events::EventBus::default();
    let sessions = Arc::new(rollout::Sessions::new());
    let clock: Arc<dyn clock::Clock> = Arc::new(clock::TokioClock::new());
    let cancel = shutdown.child_token();

    let (controller, controller_task) = rollout::spawn(
        rollout::ControllerDeps {
            store: store.clone(),
            link: sessions.clone(),
            clock: clock.clone(),
            events: events.clone(),
            revisions: revisions.clone(),
            secrets: secrets.clone(),
            approval_key: key,
            options: rollout::ControllerOptions {
                disconnect_timeout: config.agents.disconnect_timeout,
            },
        },
        cancel.clone(),
    )
    .await?;

    let notify = controller.clone();
    let builder = source::RevisionBuilder::new(store.clone(), revisions.clone(), events.clone())
        .on_added(Arc::new(move |rec| notify.revision_added(&rec.id)));
    let mut tasks = Vec::new();
    let mut trigger = None;
    let mut webhook_secret = None;
    if let Some(git) = &config.source.git {
        let watcher = source::git::GitWatcher::new(
            git.clone(),
            &config.data_dir,
            builder.clone(),
            store.clone(),
        );
        trigger = Some(watcher.trigger());
        webhook_secret = git.webhook_secret.clone();
        tracing::info!(url = %config::redact_url(&git.url), branch = %git.branch, "watching the Git source");
        tasks.push(tokio::spawn(watcher.run(cancel.clone())));
    } else if let Some(path) = &config.source.path {
        tracing::info!(path = %path.display(), "watching the source directory");
        let watcher = source::path::PathWatcher::new(path.clone(), builder.clone());
        tasks.push(tokio::spawn(watcher.run(cancel.clone())));
    } else {
        return Err(buckle_core::error::user(
            "server.yaml configures no source.",
            &["Set source.git (a repository and branch) or source.path (a local directory)."],
        ));
    }

    let identity = grpc::tls::load(&config.listen.tls, &config.data_dir)?;
    match &identity {
        Some(id) => {
            tracing::info!(fingerprint = %format_args!("sha256:{}", id.fingerprint), "gRPC TLS certificate fingerprint (agents pin this with --ca-fingerprint)")
        }
        None => tracing::warn!("the gRPC listener is plaintext (listen.tls.mode: none)"),
    }
    let whois: Option<Arc<dyn grpc::Whois>> = config
        .agents
        .auth
        .contains(&config::AgentAuth::Tailscale)
        .then(|| {
            Arc::new(grpc::LocalApiWhois::new(
                config.agents.tailscale_socket.clone(),
            )) as Arc<dyn grpc::Whois>
        });
    let fleet = grpc::FleetService {
        store: store.clone(),
        sessions: sessions.clone(),
        controller: controller.clone(),
        revisions: revisions.clone(),
        secrets,
        agents: config.agents.clone(),
        whois,
        events: events.clone(),
        clock,
        server_version: VERSION.to_owned(),
    };
    let grpc_listener = tokio::net::TcpListener::bind(&config.listen.grpc)
        .await
        .map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!("Could not listen for agents on {}.", config.listen.grpc),
                &["Check listen.grpc in server.yaml; another process may hold the port."],
            )
        })?;
    tracing::info!(address = %config.listen.grpc, "listening for agents");
    let grpc_task = tokio::spawn(grpc::serve(fleet, grpc_listener, identity, cancel.clone()));

    let http_listener = std::net::TcpListener::bind(&config.listen.http).map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            format!("Could not listen for HTTP on {}.", config.listen.http),
            &["Check listen.http in server.yaml; another process may hold the port."],
        )
    })?;
    tracing::info!(address = %config.listen.http, "serving the API and UI");
    let state = web::AppState::new(
        store,
        revisions,
        controller,
        events,
        config.web.clone(),
        webhook_secret,
        trigger,
        VERSION.to_owned(),
    );
    let http_task = tokio::spawn(web::serve(state, http_listener, cancel.clone()));

    let mut failed = false;
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => tracing::info!("shutting down"),
        r = grpc_task => {
            failed = !shutdown.is_cancelled();
            tracing::error!(result = ?r.map(|r| r.map_err(|e| e.to_string())), "the gRPC server stopped");
        }
        r = http_task => {
            failed = !shutdown.is_cancelled();
            tracing::error!(result = ?r.map(|r| r.map_err(|e| e.to_string())), "the HTTP server stopped");
        }
    }
    cancel.cancel();
    for t in tasks {
        let _ = t.await;
    }
    let _ = controller_task.await;
    Ok(if failed { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_server_starts_serves_and_shuts_down() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("cfg");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("buckle.yaml"), "version: 2\n").unwrap();
        let grpc = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let http = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let yaml = format!(
            "listen: {{ grpc: \"{grpc}\", http: \"{http}\", tls: {{ mode: auto }} }}\ndata_dir: {}\nsource: {{ path: {} }}\nagents: {{ auth: [token], bootstrap_token: boot }}\n",
            dir.path().join("data").display(),
            source.display()
        );
        let config = ServerConfig::parse(&yaml).unwrap();
        let fp = fingerprint(&config)
            .unwrap()
            .expect("auto TLS has a fingerprint");
        let shutdown = CancellationToken::new();
        let server = tokio::spawn(run(config.clone(), shutdown.clone()));
        let url = format!("http://{http}/robots.txt");
        let mut ok = false;
        for _ in 0..100 {
            if let Ok(r) = reqwest::get(&url).await
                && r.status().is_success()
            {
                ok = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(ok, "the HTTP listener answers");
        // The directory source produced a valid revision, which promoted with no hosts.
        let mut promoted = false;
        for _ in 0..100 {
            let r = reqwest::get(format!("http://{http}/api/v1/fleet"))
                .await
                .unwrap();
            let fleet: buckle_api::Fleet = r.json().await.unwrap();
            if fleet.stable_revision.is_some() {
                promoted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(promoted, "the first revision rolled out and became stable");
        assert_eq!(
            fingerprint(&config).unwrap().unwrap(),
            fp,
            "the certificate is reused"
        );
        shutdown.cancel();
        let code = tokio::time::timeout(std::time::Duration::from_secs(20), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(code, 0);
    }
}
