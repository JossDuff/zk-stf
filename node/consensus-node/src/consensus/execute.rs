//! Apply committed nodes to live state; submit work to the validator.
//!
//! A DECIDE arriving at view `v` commits node `b_v` *and* every uncommitted
//! ancestor on the path back to the last executed height (paper Alg 2 line 34:
//! "execute new commands through m.justify.node"). `try_execute_chain` walks
//! that path and applies in order, gating on validation completion for each
//! node.
//!
//! `maybe_kick_validation` is the single place we submit a block to the
//! validator worker. It dedupes against the in-flight pending hash and the
//! cached result map, so calling it from multiple callers (handlers, rules,
//! execute itself) is safe.

use super::Wiring;
use crate::logging::log_event;
use crate::messages::GENESIS_HASH;
use crate::state::ReplicaState;
use crate::validator::ValidateRequest;
use ledger_core::State;

/// Walk back from `decided_hash` through parent links, applying any ancestor
/// at height `last_executed_height + 1` whose validation has completed.
pub(super) fn try_execute_chain(
    rs: &mut ReplicaState,
    w: &Wiring<'_>,
    live_state: &mut State,
    chained_root: &mut [u8; 32],
    decided_hash: [u8; 32],
) {
    // Build the ancestor chain from decided down to (last_executed_height + 1).
    let mut chain: Vec<[u8; 32]> = Vec::new();
    let mut cur = decided_hash;
    while let Some(node) = rs.tree.get(&cur).cloned() {
        if (node.height as i64) <= rs.last_executed_height {
            break;
        }
        chain.push(cur);
        if node.parent == GENESIS_HASH {
            break;
        }
        cur = node.parent;
    }
    chain.reverse(); // ascending by height

    for nh in chain {
        let node = rs.tree.get(&nh).cloned().expect("chain node missing");
        let target_height = node.height;
        if (target_height as i64) != rs.last_executed_height + 1 {
            // Out of order; bail and try again next pass.
            return;
        }
        // Need validation done for this node.
        let valid = match rs.validation_result.get(&nh).copied() {
            Some(true) => true,
            Some(false) => {
                eprintln!(
                    "[node {}] execute: ancestor validation FAILED at h={target_height}",
                    w.my_id
                );
                return;
            }
            None => {
                // Kick off validation; will retry on completion.
                maybe_kick_validation(rs, w, nh, &*live_state);
                return;
            }
        };
        if !valid {
            return;
        }

        // Advance live_state / chained_root.
        if let Some(ps) = rs.post_state_by_node.remove(&nh) {
            *live_state = ps;
        }
        if rs.pre_root_by_node.contains_key(&nh) {
            // verify mode chain check
            if target_height > 0 {
                let pre = rs.pre_root_by_node.get(&nh).copied();
                if pre != Some(*chained_root) {
                    panic!(
                        "[node {}] chain break at h={target_height}: pre={} expected={}",
                        w.my_id,
                        pre.map(hex::encode).unwrap_or_else(|| "missing".into()),
                        hex::encode(*chained_root),
                    );
                }
            }
            if let Some(pr) = rs.post_root_by_node.remove(&nh) {
                *chained_root = pr;
            }
        }

        rs.last_executed_height = target_height as i64;
        rs.committed_at_height.insert(target_height, nh);
        log_event(
            w.my_id,
            rs.view,
            "execute",
            serde_json::json!({
                "height": target_height,
                "node_hash": hex::encode(nh),
            }),
        );
    }
}

/// Walk every node we've ever seen committed (via DECIDE) and try to execute
/// it. Called every `try_advance` pass to backfill state when a validation
/// outcome lands AFTER its DECIDE has already moved past — for instance,
/// when a slow node receives DECIDE for height H, fails to apply because
/// validation is still pending, and the validation completes only after the
/// view has advanced. `try_execute_chain` is idempotent (skips heights
/// already executed), so this is cheap to run on every event.
pub(super) fn retry_pending_execute(
    rs: &mut ReplicaState,
    w: &Wiring<'_>,
    live_state: &mut State,
    chained_root: &mut [u8; 32],
) {
    let hashes: Vec<[u8; 32]> = rs.decided_nodes.keys().copied().collect();
    for nh in hashes {
        try_execute_chain(rs, w, live_state, chained_root, nh);
    }
}

/// Submit a validation request for `leaf_hash`'s underlying block, if not
/// already in flight or cached.
///
/// Re-execute mode requires chaining: each validation must use the parent's
/// post-state as input, not whatever the current `live_state` happens to be.
/// We defer kickoff until the parent is either committed (so `live_state` is
/// its post-state) or independently validated (so we can pull its post-state
/// from `post_state_by_node`). `try_kick_pending_validations` retries
/// deferred kickoffs after each `ingest_validation`.
pub(super) fn maybe_kick_validation(
    rs: &mut ReplicaState,
    w: &Wiring<'_>,
    leaf_hash: [u8; 32],
    live_state: &State,
) {
    if rs.validation_result.contains_key(&leaf_hash) {
        return;
    }
    if rs.pending_validation == Some(leaf_hash) {
        return;
    }
    let node = match rs.tree.get(&leaf_hash) {
        Some(n) => n.clone(),
        None => return,
    };
    let block = w.workload.block(node.height);
    if block.cmd != node.cmd {
        eprintln!(
            "[node {}] validate skip: cmd mismatch at h={} (proposer cheating?)",
            w.my_id, node.height
        );
        return;
    }

    // Determine the input state that this validation should run against.
    //  - parent is genesis  -> initial state == live_state at last_executed=-1
    //  - parent is committed (height <= last_executed) -> live_state IS its post-state
    //  - parent is validated but not committed -> use cached post-state
    //  - parent isn't validated yet -> defer; we'll retry when its outcome lands
    let input_state = if node.parent == GENESIS_HASH {
        if rs.last_executed_height >= 0 {
            // Genesis is the parent but we've already advanced past height 0;
            // this only happens for retry views proposing the same first leaf
            // after we've already committed it via a different node. The
            // validation result is cached either way, so just bail.
            return;
        }
        live_state.clone()
    } else {
        match rs.tree.get(&node.parent) {
            Some(parent_node) => {
                let parent_h = parent_node.height as i64;
                if parent_h <= rs.last_executed_height {
                    live_state.clone()
                } else {
                    match rs.post_state_by_node.get(&node.parent) {
                        Some(ps) => ps.clone(),
                        None => return, // parent not yet validated; defer
                    }
                }
            }
            None => return, // parent unknown; defer until we receive it
        }
    };

    rs.pending_validation = Some(leaf_hash);
    let req = ValidateRequest {
        node_hash: leaf_hash,
        height: node.height,
        txs_path: block.txs_path,
        proof_path: block.proof_path,
        txs_total: block.meta.txs_total,
    };
    let _ = w.val_req_tx.send((req, input_state));
    log_event(
        w.my_id,
        rs.view,
        "validate_start",
        serde_json::json!({ "height": node.height }),
    );
}

/// Walk every known leaf and try to kick off its validation. A no-op for any
/// leaf that's already cached, in flight, or whose parent isn't ready yet.
/// Called every `try_advance` pass so a deferred validation gets retried as
/// soon as its parent's post-state lands.
pub(super) fn try_kick_pending_validations(
    rs: &mut ReplicaState,
    w: &Wiring<'_>,
    live_state: &State,
) {
    let leaves: Vec<[u8; 32]> = rs.tree.keys().copied().collect();
    for nh in leaves {
        if rs.pending_validation.is_some() {
            // Worker is serial; only useful to queue one at a time.
            return;
        }
        maybe_kick_validation(rs, w, nh, live_state);
    }
}
