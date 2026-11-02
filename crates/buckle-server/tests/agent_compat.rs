//! Wire compatibility with the real agent transport: the agent lane's
//! `GrpcTransport` enrols with the bootstrap token over TLS pinned to the
//! server's auto-generated certificate, receives Welcome and Assign, and
//! fetches and verifies the assigned revision.

use buckle_agent::config::AgentConfig;
use buckle_agent::transport::grpc::GrpcTransport;
use buckle_agent::transport::{Hello, Instruction, RevisionSource, Transport};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

async fn next(session: &mut buckle_agent::transport::Session) -> Instruction {
    tokio::time::timeout(Duration::from_secs(20), session.instructions.recv())
        .await
        .expect("an instruction arrives")
        .expect("the session is open")
}

#[tokio::test]
async fn the_agent_transport_enrols_is_assigned_and_fetches_a_verified_revision() {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("cfg");
    std::fs::create_dir_all(source.join("modules/motd/files")).unwrap();
    std::fs::write(
        source.join("buckle.yaml"),
        "version: 2\nrollout:\n  health: { bake: 1s }\n",
    )
    .unwrap();
    std::fs::write(source.join("modules/motd/module.yaml"), "name: motd\n").unwrap();
    std::fs::write(
        source.join("modules/motd/files/motd.j2"),
        "hello {{ host.id }}\n",
    )
    .unwrap();
    let grpc = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let http = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let yaml = format!(
        "listen: {{ grpc: \"{grpc}\", http: \"{http}\", tls: {{ mode: auto }} }}\ndata_dir: {}\nsource: {{ path: {} }}\nagents: {{ auth: [token], bootstrap_token: boot, minimum_agent_version: \"2.0.0-dev\" }}\n",
        dir.path().join("data").display(),
        source.display()
    );
    let config = buckle_server::ServerConfig::parse(&yaml).unwrap();
    let fingerprint = buckle_server::fingerprint(&config).unwrap().unwrap();
    let shutdown = CancellationToken::new();
    let server = tokio::spawn(buckle_server::run(config, shutdown.clone()));

    // Wait for the first revision to be promoted, so the host is a straggler.
    let mut stable = None;
    for _ in 0..200 {
        if let Ok(r) = reqwest::get(format!("http://{http}/api/v1/fleet")).await
            && let Ok(fleet) = r.json::<buckle_api::Fleet>().await
            && fleet.stable_revision.is_some()
        {
            stable = fleet.stable_revision;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stable = stable.expect("the first revision became stable");

    let state = dir.path().join("agent-state");
    let agent: AgentConfig = serde_yaml::from_str(&format!(
        "server: https://localhost:{}\nhost_id: pi-1\nstate_dir: {}\nbootstrap_token: boot\ntls: {{ ca_fingerprint: \"sha256:{fingerprint}\" }}\n",
        grpc.port(),
        state.display()
    ))
    .unwrap();
    let transport = GrpcTransport::new(&agent).unwrap();
    let mut session = transport
        .connect(Hello {
            host_id: "pi-1".into(),
            agent_version: "2.0.0".into(),
            labels: Default::default(),
            applied_revision: None,
            last_plan_digest: None,
            token: Some("boot".into()),
            platform: "linux".into(),
            arch: "aarch64".into(),
            bootstrap: true,
            unreported_apply_revisions: vec![],
        })
        .await
        .expect("the session opens over pinned TLS");

    let Instruction::Welcome(welcome) = next(&mut session).await else {
        panic!("Welcome comes first");
    };
    assert_eq!(welcome.status, "active");
    assert_eq!(
        welcome.stable_revision.as_ref().map(|r| r.0.as_str()),
        Some(stable.as_str())
    );
    assert_eq!(welcome.issued_token.as_deref().map(str::len), Some(64));
    // What the agent's session loop does with an issued token.
    buckle_agent::store::Store::new(&state)
        .write_token(welcome.issued_token.as_deref().unwrap())
        .unwrap();

    let (revision, tree_digest) = loop {
        match next(&mut session).await {
            Instruction::Assign {
                revision,
                tree_digest,
                ..
            } => break (revision, tree_digest),
            _ => continue,
        }
    };
    assert_eq!(revision.0, stable);
    let tree_digest = tree_digest.expect("Assign carries the tree digest");
    match transport
        .fetch_revision(&revision, Some(&tree_digest))
        .await
        .expect("the revision verifies")
    {
        RevisionSource::Tarball(bytes) => assert!(!bytes.is_empty()),
        RevisionSource::Directory(_) => panic!("a server revision is a tarball"),
    }

    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(20), server).await;
}
