//! Basic HotStuff consensus state machine.
//!
//! Implements Algorithm 2 from Yin/Malkhi/Reiter/Golan-Gueta/Abraham,
//! "HotStuff: BFT Consensus with Linearity and Responsiveness" (PODC '19).
//!
//! Each view has four phases (PREPARE → PRE-COMMIT → COMMIT → DECIDE) preceded
//! by a NEW-VIEW collection step at the leader. We track the protocol with a
//! set of fire-once rules that are re-evaluated after every event:
//!
//!   - `rules::leader_prepare`     — Alg 2 lines 2-6   (leader broadcasts proposal)
//!   - `rules::replica_prepare`    — Alg 2 lines 8-10  (replica votes PREPARE)
//!   - `rules::leader_pre_commit`  — Alg 2 lines 12-14 (leader broadcasts prepareQC)
//!   - `rules::replica_pre_commit` — Alg 2 lines 16-18 (replica sets prepareQC, votes)
//!   - `rules::leader_commit`      — Alg 2 lines 20-22 (leader broadcasts precommitQC)
//!   - `rules::replica_commit`     — Alg 2 lines 24-26 (replica sets lockedQC, votes)
//!   - `rules::leader_decide`      — Alg 2 lines 28-30 (leader broadcasts commitQC)
//!   - `rules::replica_decide`     — Alg 2 lines 32-34 (replica executes)
//!
//! Module layout:
//!   - `handlers` — incoming wire messages, timer interrupts, validation
//!     outcomes, and the `enter_new_view` transition.
//!   - `rules` — the eight per-phase fire-once rules above.
//!   - `execute` — `try_execute_chain` (apply committed ancestors) and
//!     `maybe_kick_validation` (submit work to the validator).
//!   - `qc` — `form_qc` and `verify_qc` helpers.
//!
//! Validation gating: per the experiment design, a replica's PREPARE vote
//! requires application-level `valid(node)` to have completed. Subsequent
//! votes (PRE-COMMIT, COMMIT) act on QC arrival without an additional
//! validation gate. State advance at execute time is gated on validation
//! completion for every ancestor being applied.

mod execute;
mod handlers;
mod qc;
mod rules;

use crate::network::PeerWriters;
use crate::state::{ReplicaState, TimerEvent};
use crate::validator::ValidateRequest;
use crate::workload::Workload;
use ed25519_dalek::SigningKey;
use ledger_core::State;
use tokio::sync::mpsc;

pub use handlers::{handle_msg, handle_timer, ingest_validation};

/// Wiring read by the consensus module: handles for sending, scheduling, and
/// validating. `live_state` is passed separately (and mutably to
/// `try_advance`) to avoid aliasing borrows.
pub struct Wiring<'a> {
    pub my_id: u32,
    pub sk: &'a SigningKey,
    pub peers: &'a PeerWriters,
    pub timer_tx: &'a mpsc::UnboundedSender<TimerEvent>,
    pub val_req_tx: &'a mpsc::UnboundedSender<(ValidateRequest, State)>,
    pub workload: &'a Workload,
    pub timeout_base_ms: u64,
}

/// Called once at startup. Sends the synthetic NEW-VIEW(view=0) to the
/// leader of view 1 and arms view 1's NEXTVIEW timer.
pub fn start_protocol(rs: &mut ReplicaState, w: &Wiring<'_>) {
    rs.view = 0;
    handlers::enter_new_view(rs, 1, w);
}

/// Re-evaluates every rule whose preconditions might have just become true.
/// Idempotent — guarded by per-view fire-once flags.
pub fn try_advance(
    rs: &mut ReplicaState,
    w: &Wiring<'_>,
    live_state: &mut State,
    chained_root: &mut [u8; 32],
) {
    rules::leader_prepare(rs, w, &*live_state);
    rules::replica_prepare(rs, w);
    rules::leader_pre_commit(rs, w);
    rules::replica_pre_commit(rs, w);
    rules::leader_commit(rs, w);
    rules::replica_commit(rs, w);
    rules::leader_decide(rs, w);
    rules::replica_decide(rs, w, live_state, chained_root);
    // Backfill: a validation outcome that landed after its DECIDE moved past
    // wouldn't otherwise trigger an apply pass for the older height.
    execute::retry_pending_execute(rs, w, live_state, chained_root);
    // Retry any validation whose kickoff was deferred because its parent
    // hadn't been validated yet (re-execute mode chaining).
    execute::try_kick_pending_validations(rs, w, &*live_state);
}
