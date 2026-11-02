//! The gRPC protocol between Buckle agents and the Buckle server, generated
//! from `proto/buckle/v1/fleet.proto` with tonic.
//!
//! Conventions:
//! * Revision and digest fields are hex strings.
//! * Structured documents the UI also needs (plans, outcomes, facts) travel as
//!   JSON bytes of the `buckle-core` types, so the schema lives in one place.
//! * Trace context is carried in gRPC metadata (`traceparent`), not in messages.

pub mod v1 {
    tonic::include_proto!("buckle.v1");

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
