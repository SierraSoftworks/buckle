//! The gRPC transport: a tonic client for the `Fleet` service.
//!
//! * TLS follows `agent.yaml`'s `tls` section: a pinned `ca_fingerprint`
//!   (exactly that certificate, see [`super::tls`]), a `ca_file` bundle, or
//!   the system roots; `http://` URLs need `insecure_plaintext: true`.
//! * Every call carries `x-buckle-token` (the per-host token from the state
//!   directory, else the bootstrap token) and, when the current span is
//!   traced, `traceparent`.
//! * `Session` sends [`Hello`] first; server messages become
//!   [`Instruction`]s and [`Report`]s become `AgentMessage`s.
//! * `FetchRevision` chunks are assembled and verified against the digest the
//!   server sends on the last chunk (and the Assign's tree digest, if any).

use super::convert::{hello_to_proto, instruction_from_proto, report_to_proto};
use super::{Hello, Instruction, Report, RevisionSource, Session, Transport, WireSecret};
use crate::bundle::{MAX_BUNDLE_BYTES, digests_match, verify_digest};
use crate::config::AgentConfig;
use crate::store::Store;
use async_trait::async_trait;
use buckle_core::plan::RevisionId;
use buckle_proto::v1 as pb;
use buckle_proto::v1::fleet_client::FleetClient;
use futures::StreamExt;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};

/// The W3C `traceparent` of the current span, when it is part of a trace.
pub fn current_traceparent() -> Option<String> {
    use tracing_batteries::prelude::{
        OpenTelemetrySpanExt, TraceContextExt, get_text_map_propagator,
    };
    let context = tracing::Span::current().context();
    if !context.span().span_context().is_valid() {
        return None;
    }
    let mut carrier: HashMap<String, String> = HashMap::new();
    get_text_map_propagator(|propagator| propagator.inject_context(&context, &mut carrier));
    carrier.remove(buckle_proto::TRACEPARENT_METADATA_KEY)
}

/// Makes `traceparent` (when present and valid) the parent of `span`.
pub fn set_parent_from_traceparent(span: &tracing::Span, traceparent: Option<&str>) {
    use tracing_batteries::prelude::{
        OpenTelemetrySpanExt, TraceContextExt, get_text_map_propagator,
    };
    let Some(traceparent) = traceparent else {
        return;
    };
    let mut carrier: HashMap<String, String> = HashMap::new();
    carrier.insert(
        buckle_proto::TRACEPARENT_METADATA_KEY.to_owned(),
        traceparent.to_owned(),
    );
    let parent = get_text_map_propagator(|propagator| propagator.extract(&carrier));
    if parent.span().span_context().is_valid() {
        let _ = span.set_parent(parent);
    }
}

fn connection_error(
    err: impl std::error::Error + Send + Sync + 'static,
    what: &str,
) -> buckle_core::Error {
    buckle_core::error::wrap_user(
        err,
        format!("The agent could not {what}."),
        &[
            "Check that the server is running and reachable from this host, and that `server` in agent.yaml is correct.",
            "The agent keeps retrying with backoff.",
        ],
    )
}

fn status_error(status: tonic::Status, what: &str) -> buckle_core::Error {
    let advice: &[&str] = match status.code() {
        tonic::Code::Unauthenticated | tonic::Code::PermissionDenied => &[
            "The server did not accept this host's token. Re-enrol with `buckle up --token <bootstrap>` or ask an admin to approve the host.",
        ],
        tonic::Code::Unavailable => {
            &["The server is unreachable; the agent keeps retrying with backoff."]
        }
        _ => &["Check the server's logs for details."],
    };
    buckle_core::error::wrap_user(status, format!("The server refused to {what}."), advice)
}

/// Builds the endpoint for `config.server` with the configured TLS mode.
pub fn endpoint(config: &AgentConfig) -> buckle_core::Result<Endpoint> {
    let server = config.server.as_deref().ok_or_else(|| {
        buckle_core::error::user(
            "The agent configuration has no `server` to connect to.",
            &["Set `server` in agent.yaml, or `source` to watch a local directory."],
        )
    })?;
    let url = url::Url::parse(server).map_err(|e| {
        buckle_core::error::wrap_user(
            e,
            format!("The server URL '{server}' is not valid."),
            &["Use a URL like https://buckle.example.ts.net:7700."],
        )
    })?;
    let tls = &config.tls;
    let plaintext = match url.scheme() {
        "http" if tls.insecure_plaintext => true,
        "http" => {
            return Err(buckle_core::error::user(
                format!(
                    "The server URL '{server}' is plaintext but `tls.insecure_plaintext` is not set."
                ),
                &[
                    "Use an https:// URL, or set `tls: { insecure_plaintext: true }` when the connection is protected another way (e.g. a tailnet).",
                ],
            ));
        }
        "https" if tls.insecure_plaintext => {
            return Err(buckle_core::error::user(
                format!(
                    "The server URL '{server}' asks for TLS but `tls.insecure_plaintext` is set."
                ),
                &[
                    "Remove `insecure_plaintext` to use TLS, or use an http:// URL if the connection really is protected another way (e.g. a tailnet).",
                ],
            ));
        }
        "https" => false,
        other => {
            return Err(buckle_core::error::user(
                format!("The server URL scheme '{other}' is not supported."),
                &["Use an https:// (or, with insecure_plaintext, http://) URL."],
            ));
        }
    };
    let endpoint = Endpoint::from_shared(url.to_string())
        .map_err(|e| connection_error(e, "use the configured server URL"))?
        .connect_timeout(std::time::Duration::from_secs(15))
        .tcp_keepalive(Some(std::time::Duration::from_secs(30)))
        .http2_keep_alive_interval(std::time::Duration::from_secs(30))
        .keep_alive_timeout(std::time::Duration::from_secs(20))
        .keep_alive_while_idle(true);
    if plaintext {
        return Ok(endpoint);
    }

    let base = ClientTlsConfig::new().assume_http2(true);
    if let Some(fingerprint) = &tls.ca_fingerprint {
        let verifier = super::tls::PinnedCertVerifier::new(fingerprint)?;
        return endpoint
            .tls_config_with_verifier(base, verifier.into_arc())
            .map_err(|e| connection_error(e, "configure certificate pinning"));
    }
    if let Some(ca_file) = &tls.ca_file {
        let pem = std::fs::read(ca_file).map_err(|e| {
            buckle_core::error::wrap_user(
                e,
                format!("The CA bundle {} could not be read.", ca_file.display()),
                &["Check `tls.ca_file` in agent.yaml."],
            )
        })?;
        return endpoint
            .tls_config(base.ca_certificate(Certificate::from_pem(pem)))
            .map_err(|e| connection_error(e, "load the configured CA bundle"));
    }
    endpoint
        .tls_config(base.with_native_roots())
        .map_err(|e| connection_error(e, "load the system's trusted certificates"))
}

/// The gRPC transport.
#[derive(Clone)]
pub struct GrpcTransport {
    channel: Channel,
    store: Store,
    bootstrap_token: Option<String>,
}

impl GrpcTransport {
    /// A transport for `config.server` (connects lazily).
    pub fn new(config: &AgentConfig) -> buckle_core::Result<Self> {
        let channel = endpoint(config)?.connect_lazy();
        Ok(Self::from_channel(
            channel,
            Store::new(&config.state_dir),
            config.bootstrap_token.clone(),
        ))
    }

    /// A transport over an existing channel (tests use in-memory channels).
    pub fn from_channel(channel: Channel, store: Store, bootstrap_token: Option<String>) -> Self {
        Self {
            channel,
            store,
            bootstrap_token,
        }
    }

    fn token(&self) -> Option<String> {
        self.store
            .read_token()
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "could not read the per-host token");
                None
            })
            .or_else(|| self.bootstrap_token.clone())
    }

    fn request<T>(&self, message: T, token: Option<&str>) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        let metadata = request.metadata_mut();
        if let Some(token) = token
            && let Ok(value) = MetadataValue::try_from(token)
        {
            metadata.insert(buckle_proto::TOKEN_METADATA_KEY, value);
        }
        if let Some(traceparent) = current_traceparent()
            && let Ok(value) = MetadataValue::try_from(traceparent.as_str())
        {
            metadata.insert(buckle_proto::TRACEPARENT_METADATA_KEY, value);
        }
        request
    }

    fn client(&self) -> FleetClient<Channel> {
        FleetClient::new(self.channel.clone()).max_decoding_message_size(64 * 1024 * 1024)
    }
}

/// Assembles a `FetchRevision` stream and verifies it.
pub async fn collect_chunks<S>(
    mut chunks: S,
    revision: &RevisionId,
    tree_digest: Option<&str>,
) -> buckle_core::Result<Vec<u8>>
where
    S: futures::Stream<Item = Result<pb::Chunk, tonic::Status>> + Unpin,
{
    let mut data = Vec::new();
    let mut total: Option<u64> = None;
    let mut digest: Option<String> = None;
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(|s| status_error(s, "send the revision"))?;
        if total.is_none() && chunk.total > 0 {
            total = Some(chunk.total);
            if chunk.total > MAX_BUNDLE_BYTES {
                return Err(buckle_core::error::user(
                    format!(
                        "Revision {} is {} bytes, more than the agent accepts.",
                        revision.short(),
                        chunk.total
                    ),
                    &[
                        "Keep large files out of the configuration repository, or fetch them as blobs.",
                    ],
                ));
            }
        }
        if data.len() as u64 + chunk.data.len() as u64 > MAX_BUNDLE_BYTES {
            return Err(buckle_core::error::user(
                format!(
                    "Revision {} is larger than the agent accepts.",
                    revision.short()
                ),
                &["Keep large files out of the configuration repository, or fetch them as blobs."],
            ));
        }
        data.extend_from_slice(&chunk.data);
        if !chunk.digest.is_empty() {
            digest = Some(chunk.digest);
        }
    }
    if let Some(total) = total
        && total != data.len() as u64
    {
        return Err(buckle_core::error::user(
            format!(
                "The download of revision {} was cut short: {} of {total} bytes arrived.",
                revision.short(),
                data.len()
            ),
            &["The agent will fetch it again."],
        ));
    }
    let digest = digest.ok_or_else(|| {
        buckle_core::error::user(
            format!(
                "The server sent revision {} without a digest, so it cannot be verified.",
                revision.short()
            ),
            &["Upgrade the server; every FetchRevision stream ends with the payload digest."],
        )
    })?;
    verify_digest(&data, &digest)?;
    if let Some(expected) = tree_digest
        && !digests_match(expected, &digest)
    {
        return Err(buckle_core::error::user(
            format!(
                "Revision {} arrived with digest {digest}, but it was assigned with {expected}.",
                revision.short()
            ),
            &["The server's revision store is inconsistent; check its logs."],
        ));
    }
    Ok(data)
}

#[async_trait]
impl Transport for GrpcTransport {
    async fn connect(&self, hello: Hello) -> buckle_core::Result<Session> {
        let (report_tx, report_rx) = mpsc::channel::<Report>(256);
        let first = pb::AgentMessage {
            msg: Some(pb::agent_message::Msg::Hello(hello_to_proto(&hello))),
        };
        let outbound = futures::stream::once(async move { first })
            .chain(ReceiverStream::new(report_rx).map(|report| report_to_proto(&report)));
        let request = self.request(outbound, hello.token.as_deref());
        let response = self
            .client()
            .session(request)
            .await
            .map_err(|s| status_error(s, "open a session"))?;
        let mut inbound = response.into_inner();
        let (instruction_tx, instruction_rx) = mpsc::channel::<Instruction>(64);
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(message)) => {
                        if let Some(instruction) = instruction_from_proto(message)
                            && instruction_tx.send(instruction).await.is_err()
                        {
                            break;
                        }
                    }
                    Ok(None) => {
                        tracing::info!("the server closed the session");
                        break;
                    }
                    Err(status) => {
                        tracing::warn!(code = ?status.code(), message = %status.message(), "the session stream failed");
                        break;
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
        revision: &RevisionId,
        tree_digest: Option<&str>,
    ) -> buckle_core::Result<RevisionSource> {
        let token = self.token();
        let request = self.request(
            pb::FetchRevisionRequest {
                revision: revision.0.clone(),
            },
            token.as_deref(),
        );
        let stream = self
            .client()
            .fetch_revision(request)
            .await
            .map_err(|s| status_error(s, "send the revision"))?
            .into_inner();
        let data = collect_chunks(stream, revision, tree_digest).await?;
        Ok(RevisionSource::Tarball(data))
    }

    async fn fetch_secrets(
        &self,
        revision: &RevisionId,
        names: &[String],
    ) -> buckle_core::Result<Vec<WireSecret>> {
        let token = self.token();
        let request = self.request(
            pb::FetchSecretsRequest {
                revision: revision.0.clone(),
                names: names.to_vec(),
            },
            token.as_deref(),
        );
        let bundle = self
            .client()
            .fetch_secrets(request)
            .await
            .map_err(|s| status_error(s, "send secrets"))?
            .into_inner();
        Ok(super::convert::secrets_from_proto(Some(bundle)))
    }

    async fn issue_identity(
        &self,
        request: &crate::identity::IdentityRequest,
    ) -> buckle_core::Result<crate::identity::IssuedIdentity> {
        use crate::identity::Purpose;
        let token = self.token();
        let message = pb::IssueIdentityRequest {
            target: request.target.clone(),
            purpose: match request.purpose {
                Purpose::Read => pb::IdentityPurpose::Read,
                Purpose::Deploy => pb::IdentityPurpose::Deploy,
                Purpose::Drain => pb::IdentityPurpose::Drain,
                Purpose::Acl => pb::IdentityPurpose::Acl,
            } as i32,
            revision: request
                .revision
                .as_ref()
                .map(|r| r.0.clone())
                .unwrap_or_default(),
            rollout_id: request.rollout.clone().unwrap_or_default(),
        };
        let issued = self
            .client()
            .issue_identity(self.request(message, token.as_deref()))
            .await
            .map_err(|status| {
                let advice: &[&str] = match status.code() {
                    tonic::Code::PermissionDenied | tonic::Code::NotFound => &[
                        "Check the cluster's `nodes` and `executors` selectors and its `identity` in buckle.yaml.",
                    ],
                    tonic::Code::Unimplemented => {
                        &["Upgrade the Buckle server; it predates cluster identities."]
                    }
                    _ => &["Check the server's logs for details."],
                };
                buckle_core::error::wrap_user(
                    status,
                    format!("The server did not issue an identity for {}.", request.target),
                    advice,
                )
            })?
            .into_inner();
        let expires_at = issued
            .expires_at
            .and_then(|t| chrono::DateTime::from_timestamp(t.seconds, t.nanos.max(0) as u32))
            .ok_or_else(|| {
                buckle_core::error::system(
                    "The server issued an identity without an expiry.",
                    &["Report this issue."],
                )
            })?;
        Ok(crate::identity::IssuedIdentity {
            token: issued.token,
            expires_at,
            audience: issued.audience,
            id: issued.id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn config(server: &str, plaintext: bool) -> AgentConfig {
        let mut config: AgentConfig =
            serde_yaml::from_str(&format!("server: {server}\n")).expect("config");
        config.tls.insecure_plaintext = plaintext;
        config
    }

    #[rstest]
    #[case("http://127.0.0.1:7700", true, true)]
    #[case("http://127.0.0.1:7700", false, false)]
    #[case::no_silent_downgrade("https://127.0.0.1:7700", true, false)]
    #[case("ftp://127.0.0.1:7700", true, false)]
    #[tokio::test]
    async fn plaintext_must_be_explicit(
        #[case] server: &str,
        #[case] plaintext: bool,
        #[case] ok: bool,
    ) {
        let built = endpoint(&config(server, plaintext));
        assert_eq!(built.is_ok(), ok);
        if let Ok(endpoint) = built {
            assert_eq!(endpoint.uri().scheme_str(), Some("http"));
        }
    }

    #[tokio::test]
    async fn https_endpoints_accept_a_pinned_fingerprint() {
        let mut cfg = config("https://buckle.example:7700", false);
        cfg.tls.ca_fingerprint = Some(super::super::tls::fingerprint_of(b"cert"));
        endpoint(&cfg).expect("pinned endpoint");
        cfg.tls.ca_fingerprint = Some("sha256:nothex".into());
        assert!(endpoint(&cfg).is_err());
    }

    fn chunks(
        parts: Vec<pb::Chunk>,
    ) -> impl futures::Stream<Item = Result<pb::Chunk, tonic::Status>> + Unpin {
        futures::stream::iter(parts.into_iter().map(Ok))
    }

    fn chunked(data: &[u8], digest: &str) -> Vec<pb::Chunk> {
        let pieces: Vec<&[u8]> = data.chunks(3).collect();
        let n = pieces.len();
        pieces
            .into_iter()
            .enumerate()
            .map(|(i, piece)| pb::Chunk {
                data: piece.to_vec(),
                total: if i == 0 { data.len() as u64 } else { 0 },
                digest: if i == n - 1 {
                    digest.to_owned()
                } else {
                    String::new()
                },
            })
            .collect()
    }

    #[tokio::test]
    async fn chunk_streams_are_verified() {
        let data = b"a revision tarball";
        let digest = format!("sha256:{}", crate::bundle::sha256_hex(data));
        let rev = RevisionId::new("r");
        let got = collect_chunks(chunks(chunked(data, &digest)), &rev, Some(&digest))
            .await
            .expect("valid");
        assert_eq!(got, data);

        let mut corrupted = chunked(data, &digest);
        corrupted[1].data[0] ^= 0xff;
        assert!(collect_chunks(chunks(corrupted), &rev, None).await.is_err());

        let mut short = chunked(data, &digest);
        short.remove(2);
        assert!(collect_chunks(chunks(short), &rev, None).await.is_err());

        let other = format!("sha256:{}", crate::bundle::sha256_hex(b"other"));
        assert!(
            collect_chunks(chunks(chunked(data, &digest)), &rev, Some(&other))
                .await
                .is_err()
        );

        assert!(
            collect_chunks(chunks(chunked(data, "")), &rev, None)
                .await
                .is_err()
        );
    }
}
