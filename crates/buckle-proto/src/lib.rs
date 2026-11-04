//! The gRPC protocol between Buckle agents and the Buckle server, generated
//! from `proto/buckle/v1/fleet.proto` with tonic.
//!
//! Conventions:
//! * Revision and digest fields are hex strings.
//! * Structured documents the UI also needs (plans, outcomes, facts) travel as
//!   JSON bytes of the `buckle-core` types, so the schema lives in one place.
//! * Trace context: the session call carries `traceparent` in gRPC metadata;
//!   because one session outlives many operations, the server-driven
//!   operations (Apply, Rollback, RunHealth) also carry their own
//!   `traceparent` field, which the agent uses as the parent of its span.
//! * Messages carrying secret values or tokens (SecretValue, SecretBundle,
//!   Welcome, Assign, Hello, HostUpdate) have Debug impls that show names and
//!   fingerprints only, never values or tokens.

pub mod v1 {
    tonic::include_proto!("buckle.v1");

    use std::fmt;

    fn redacted(token: &str) -> &'static str {
        if token.is_empty() { "" } else { "<redacted>" }
    }

    impl fmt::Debug for SecretValue {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("SecretValue")
                .field("name", &self.name)
                .field("fingerprint", &self.fingerprint)
                .finish_non_exhaustive()
        }
    }

    impl fmt::Debug for SecretBundle {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("SecretBundle")
                .field("secrets", &self.secrets)
                .finish()
        }
    }

    impl fmt::Debug for Welcome {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Welcome")
                .field("server_version", &self.server_version)
                .field("session_id", &self.session_id)
                .field("stable_revision", &self.stable_revision)
                .field("issued_token", &redacted(&self.issued_token))
                .field("labels", &self.labels)
                .field("status", &self.status)
                .field("secrets", &self.secrets)
                .field("stable_tree_digest", &self.stable_tree_digest)
                .finish()
        }
    }

    impl fmt::Debug for Assign {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Assign")
                .field("revision", &self.revision)
                .field("stable", &self.stable)
                .field("secrets", &self.secrets)
                .field("tree_digest", &self.tree_digest)
                .field("target", &self.target)
                .field("cluster", &self.cluster)
                .finish()
        }
    }

    impl fmt::Debug for ClusterTarget {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("ClusterTarget")
                .field("kind", &self.kind)
                .field("api", &self.api)
                .field("identity", &self.identity)
                .field("labels", &self.labels)
                .field(
                    "fingerprint_key",
                    &if self.fingerprint_key.is_empty() {
                        ""
                    } else {
                        "<redacted>"
                    },
                )
                .finish()
        }
    }

    impl fmt::Debug for Hello {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Hello")
                .field("host_id", &self.host_id)
                .field("agent_version", &self.agent_version)
                .field("labels", &self.labels)
                .field("applied_revision", &self.applied_revision)
                .field("last_plan_digest", &self.last_plan_digest)
                .field("token", &redacted(&self.token))
                .field("workload_token", &redacted(&self.workload_token))
                .field("platform", &self.platform)
                .field("arch", &self.arch)
                .field("bootstrap", &self.bootstrap)
                .field(
                    "unreported_apply_revisions",
                    &self.unreported_apply_revisions,
                )
                .finish()
        }
    }

    impl fmt::Debug for HostUpdate {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("HostUpdate")
                .field("status", &self.status)
                .field("labels", &self.labels)
                .field("stable_revision", &self.stable_revision)
                .field("secrets", &self.secrets)
                .field("stable_tree_digest", &self.stable_tree_digest)
                .finish()
        }
    }

    impl From<buckle_health::HealthScope> for HealthScope {
        fn from(scope: buckle_health::HealthScope) -> Self {
            match scope {
                buckle_health::HealthScope::Baseline => HealthScope::Baseline,
                buckle_health::HealthScope::Bake => HealthScope::Bake,
                buckle_health::HealthScope::Rollout => HealthScope::Rollout,
                buckle_health::HealthScope::Drift => HealthScope::Drift,
                buckle_health::HealthScope::Manual => HealthScope::Manual,
            }
        }
    }

    impl HealthScope {
        /// The core scope, or `None` for an unspecified value.
        pub fn to_core(self) -> Option<buckle_health::HealthScope> {
            match self {
                HealthScope::Unspecified => None,
                HealthScope::Baseline => Some(buckle_health::HealthScope::Baseline),
                HealthScope::Bake => Some(buckle_health::HealthScope::Bake),
                HealthScope::Rollout => Some(buckle_health::HealthScope::Rollout),
                HealthScope::Drift => Some(buckle_health::HealthScope::Drift),
                HealthScope::Manual => Some(buckle_health::HealthScope::Manual),
            }
        }
    }

    /// Mirror of `buckle_core::health::HealthScope` so this crate does not
    /// depend on core (the server and agent convert at the edge).
    pub mod buckle_health {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum HealthScope {
            Baseline,
            Bake,
            Rollout,
            Drift,
            Manual,
        }
    }
}

/// The gRPC metadata key carrying the per-host or bootstrap token.
pub const TOKEN_METADATA_KEY: &str = "x-buckle-token";

/// The gRPC metadata key carrying W3C trace context.
pub const TRACEPARENT_METADATA_KEY: &str = "traceparent";

#[cfg(test)]
mod tests {
    use super::v1::*;
    use prost::Message;

    #[test]
    fn messages_round_trip_through_prost() {
        let hello = AgentMessage {
            msg: Some(agent_message::Msg::Hello(Hello {
                host_id: "pi-1".into(),
                agent_version: "2.0.0".into(),
                labels: [("role".to_string(), "worker".to_string())]
                    .into_iter()
                    .collect(),
                ..Default::default()
            })),
        };
        let bytes = hello.encode_to_vec();
        let decoded = AgentMessage::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded, hello);
    }

    #[test]
    fn debug_never_shows_secret_values_or_tokens() {
        let bundle = || {
            Some(SecretBundle {
                secrets: vec![SecretValue {
                    name: "db".into(),
                    value: b"hunter2".to_vec(),
                    fingerprint: "0a1b2c3d".into(),
                }],
            })
        };
        let shown = [
            format!("{:?}", bundle()),
            format!(
                "{:?}",
                ServerMessage {
                    msg: Some(server_message::Msg::Welcome(Welcome {
                        issued_token: "tok-en-value".into(),
                        secrets: bundle(),
                        ..Default::default()
                    })),
                }
            ),
            format!(
                "{:?}",
                Assign {
                    secrets: bundle(),
                    cluster: Some(ClusterTarget {
                        fingerprint_key: b"hunter2".to_vec(),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            ),
            format!(
                "{:?}",
                AgentMessage {
                    msg: Some(agent_message::Msg::Hello(Hello {
                        token: "tok-en-value".into(),
                        workload_token: "tok-en-value".into(),
                        ..Default::default()
                    })),
                }
            ),
            format!(
                "{:?}",
                HostUpdate {
                    secrets: bundle(),
                    ..Default::default()
                }
            ),
        ];
        for text in shown {
            assert!(!text.contains("hunter2"), "{text}");
            assert!(
                !text.contains("104, 117, 110"),
                "bytes of the value: {text}"
            );
            assert!(!text.contains("tok-en-value"), "{text}");
        }
        assert!(format!("{:?}", bundle()).contains("0a1b2c3d"));
    }

    #[test]
    fn health_scope_converts_both_ways() {
        for scope in [
            buckle_health::HealthScope::Baseline,
            buckle_health::HealthScope::Bake,
            buckle_health::HealthScope::Rollout,
            buckle_health::HealthScope::Drift,
            buckle_health::HealthScope::Manual,
        ] {
            let proto: HealthScope = scope.into();
            assert_eq!(proto.to_core(), Some(scope));
        }
        assert_eq!(HealthScope::Unspecified.to_core(), None);
    }
}
