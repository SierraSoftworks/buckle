//! Bake sampling the server has moved past.
//!
//! A bake (RunHealth with `until`) samples a target's checks until its window
//! ends, and runs in the target's queue: the host's, or a cluster's lane. A
//! later rollout instruction for the same target (the next phase's gate, the
//! next step's plan or apply, a rollback, a newer bake) means the server is
//! done with that bake (it was expedited, or ended early), so it stops
//! instead of holding that instruction until the old window ends.

use crate::transport::Instruction;
use buckle_core::health::HealthScope;
use std::collections::HashMap;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// A bake's target: `(cluster, workload)`, both `None` for the host itself.
type Key = (Option<String>, Option<String>);

/// The latest bake asked for, per target, and its stop token.
#[derive(Debug, Default)]
pub(super) struct Bakes(Mutex<HashMap<Key, (String, CancellationToken)>>);

impl Bakes {
    /// Notes an instruction as it arrives, before it is queued: a bake is
    /// registered (stopping the one before it), and any other rollout
    /// instruction for its target stops that target's bake.
    pub(super) fn observe(&self, instruction: &Instruction) {
        let key = || -> Key {
            (
                instruction.target().map(str::to_owned),
                instruction.workload().map(str::to_owned),
            )
        };
        let Ok(mut bakes) = self.0.lock() else {
            return;
        };
        let mut stop = |key: &Key| {
            if let Some((_, token)) = bakes.remove(key) {
                token.cancel();
            }
        };
        match instruction {
            Instruction::RunHealth {
                request_id,
                scope: HealthScope::Bake,
                until: Some(_),
                ..
            } => {
                let key = key();
                stop(&key);
                bakes.insert(key, (request_id.clone(), CancellationToken::new()));
            }
            Instruction::RunHealth {
                scope: HealthScope::Baseline | HealthScope::Bake | HealthScope::Rollout,
                ..
            }
            | Instruction::RequestPlan { .. }
            | Instruction::Apply { .. }
            | Instruction::Rollback { .. } => stop(&key()),
            _ => {}
        }
    }

    /// The stop token of the bake `request_id` for `target`/`workload`;
    /// already cancelled when a later instruction superseded it before it
    /// started (it then takes one sample).
    pub(super) fn token(
        &self,
        target: Option<&str>,
        workload: Option<&str>,
        request_id: &str,
    ) -> CancellationToken {
        let key = (target.map(str::to_owned), workload.map(str::to_owned));
        match self.0.lock().ok().and_then(|b| b.get(&key).cloned()) {
            Some((id, token)) if id == request_id => token,
            _ => {
                let token = CancellationToken::new();
                token.cancel();
                token
            }
        }
    }
}
