//! Block validation: re-execute or verify SP1 proof.
//!
//! `Validator` encapsulates per-mode resources (slow_delay for reexecute;
//! prover client + verifying key for verify). `run_validation` is the unit of
//! work the worker task runs on the blocking pool: read from disk,
//! apply or verify, return the post-state or post-root.
//!
//! Validation is the only place STF cost is paid in the experiment, so this
//! is also where the slow-node delay is injected (slow + reexecute only).

use ledger_core::{apply_block, BlockCommit, State, Tx};
use sp1_sdk::{
    blocking::{EnvProver, Prover},
    SP1ProofWithPublicValues, SP1VerifyingKey,
};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub enum Validator {
    Reexecute {
        /// Per-tx sleep added during validation; total = `this * txs_total`.
        /// Set to 0 for fast nodes.
        slow_delay_per_tx_ns: u64,
    },
    Verify {
        client: Arc<EnvProver>,
        vkey: Arc<SP1VerifyingKey>,
    },
}

impl Validator {
    pub fn kind(&self) -> &'static str {
        match self {
            Validator::Reexecute { .. } => "reexec",
            Validator::Verify { .. } => "verify",
        }
    }
}

pub struct ValidateRequest {
    /// HotStuff node hash this block belongs to (used as cache key).
    pub node_hash: [u8; 32],
    /// Blockchain height (matches `node.height` and `commit.json` ordering).
    pub height: u64,
    pub txs_path: PathBuf,
    pub proof_path: PathBuf,
    pub txs_total: u32,
}

#[derive(Clone)]
pub struct ValidateOutcome {
    pub node_hash: [u8; 32],
    pub height: u64,
    pub valid: bool,
    /// reexecute mode: post-state to swap into live state at commit.
    pub post_state: Option<State>,
    /// verify mode: post-state root from the proof's public values.
    pub post_root: Option<[u8; 32]>,
    /// verify mode: pre-state root, used to chain at commit time.
    pub pre_root: Option<[u8; 32]>,
    pub elapsed: Duration,
}

/// Runs synchronously on a blocking-pool thread. The state snapshot is moved
/// in (not borrowed), so `apply_block` mutates it directly with no extra clone.
pub fn run_validation(
    req: &ValidateRequest,
    state_snapshot: State,
    v: &Validator,
) -> ValidateOutcome {
    let start = Instant::now();
    let invalid = |elapsed: Duration| ValidateOutcome {
        node_hash: req.node_hash,
        height: req.height,
        valid: false,
        post_state: None,
        post_root: None,
        pre_root: None,
        elapsed,
    };

    match v {
        Validator::Reexecute {
            slow_delay_per_tx_ns,
        } => {
            let txs_bytes = match fs::read(&req.txs_path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("validate: read txs failed: {e}");
                    return invalid(start.elapsed());
                }
            };
            let txs: Vec<Tx> = match bincode::deserialize(&txs_bytes) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("validate: deserialize txs failed: {e}");
                    return invalid(start.elapsed());
                }
            };
            let mut s = state_snapshot;
            apply_block(&mut s, &txs);
            if *slow_delay_per_tx_ns > 0 {
                let delay_ns = slow_delay_per_tx_ns * req.txs_total as u64;
                std::thread::sleep(Duration::from_nanos(delay_ns));
            }
            ValidateOutcome {
                node_hash: req.node_hash,
                height: req.height,
                valid: true,
                post_state: Some(s),
                post_root: None,
                pre_root: None,
                elapsed: start.elapsed(),
            }
        }
        Validator::Verify { client, vkey } => {
            let proof_bytes = match fs::read(&req.proof_path) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("validate: read proof failed: {e}");
                    return invalid(start.elapsed());
                }
            };
            let proof: SP1ProofWithPublicValues = match bincode::deserialize(&proof_bytes) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("validate: deserialize proof failed: {e}");
                    return invalid(start.elapsed());
                }
            };
            if let Err(e) = client.verify(&proof, vkey.as_ref(), None) {
                eprintln!("validate: verify failed: {e}");
                return invalid(start.elapsed());
            }
            let commit: BlockCommit = proof.public_values.clone().read();
            ValidateOutcome {
                node_hash: req.node_hash,
                height: req.height,
                valid: true,
                post_state: None,
                post_root: Some(commit.post_state_root),
                pre_root: Some(commit.pre_state_root),
                elapsed: start.elapsed(),
            }
        }
    }
}
