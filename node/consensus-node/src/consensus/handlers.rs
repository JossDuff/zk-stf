//! Event handlers: incoming wire messages, NEXTVIEW timer, validation
//! outcomes, and the `enter_new_view` transition.
//!
//! Each handler ends in a state mutation; the rule engine in `rules.rs` is
//! re-entered after every handler call by the main event loop.

use super::execute::maybe_kick_validation;
use super::qc::verify_qc;
use super::Wiring;
use crate::logging::log_event;
use crate::messages::{node_hash, verify_vote_sig, MsgType, Wire, GENESIS_HASH};
use crate::network::{arm_timer, send_to, PeerKeys};
use crate::state::{ReplicaState, TimerEvent};
use crate::validator::ValidateOutcome;
use ledger_core::State;
use std::collections::hash_map::Entry;

/// Process an incoming wire message from a peer. Verifies authenticators
/// before mutating state.
pub async fn handle_msg(
    rs: &mut ReplicaState,
    msg: Wire,
    peer_keys: &PeerKeys,
    w: &Wiring<'_>,
    live_state: &State,
) {
    match msg {
        Wire::Hello { .. } => {}
        Wire::Proposal {
            view,
            node,
            justify,
            sender,
        } => {
            let expected = rs.leader_for(view);
            if sender != expected {
                eprintln!(
                    "[node {}] reject Proposal: sender={sender} expected leader={expected}",
                    w.my_id
                );
                return;
            }
            // If a highQC is attached, it must verify. A missing highQC is
            // legitimate at any view in which no prior view has reached
            // PRE-COMMIT — the leader's argmax over n-f NEW-VIEW messages is
            // just ⊥ when every replica's local prepareQC is still ⊥. The
            // actual safety check (extends from lockedQC.node, or qc.view >
            // lockedQC.view) is `safe_node`, which trivially returns true
            // when lockedQC is None, and would correctly reject a ⊥ justify
            // against a non-None lockedQC.
            if let Some(qc) = &justify {
                if !verify_qc(qc, rs.quorum, peer_keys).await {
                    eprintln!("[node {}] reject Proposal: bad highQC sigs", w.my_id);
                    return;
                }
            }
            // Tree consistency: leaf must extend from highQC.node (or from
            // genesis when justify is ⊥).
            let expected_parent = justify
                .as_ref()
                .map(|q| q.node_hash)
                .unwrap_or(GENESIS_HASH);
            if node.parent != expected_parent {
                eprintln!(
                    "[node {}] reject Proposal: leaf.parent != highQC.node",
                    w.my_id
                );
                return;
            }
            // Stash node + proposal.
            let leaf_hash = node_hash(&node);
            rs.tree.insert(leaf_hash, node.clone());
            let vd = rs.vd_mut(view);
            if vd.proposal.is_none() {
                vd.proposal = Some((node.clone(), justify.clone()));
                log_event(
                    w.my_id,
                    view,
                    "propose_recv",
                    serde_json::json!({
                        "leaf_hash": hex::encode(leaf_hash),
                        "leaf_height": node.height,
                        "high_qc_view": justify.as_ref().map(|q| q.view).unwrap_or(0),
                        "sender": sender,
                    }),
                );
            }
            maybe_kick_validation(rs, w, leaf_hash, live_state);
        }
        Wire::PhaseQc {
            msg_type,
            view,
            justify,
            sender,
        } => {
            let expected = rs.leader_for(view);
            if sender != expected {
                eprintln!(
                    "[node {}] reject PhaseQc: sender={sender} expected leader={expected}",
                    w.my_id
                );
                return;
            }
            let expected_qc_type = match msg_type {
                MsgType::PreCommit => MsgType::Prepare,
                MsgType::Commit => MsgType::PreCommit,
                MsgType::Decide => MsgType::Commit,
                _ => {
                    eprintln!(
                        "[node {}] reject PhaseQc: bad msg_type={:?}",
                        w.my_id, msg_type
                    );
                    return;
                }
            };
            if justify.msg_type != expected_qc_type || justify.view != view {
                eprintln!(
                    "[node {}] reject PhaseQc: justify type/view mismatch",
                    w.my_id
                );
                return;
            }
            if !verify_qc(&justify, rs.quorum, peer_keys).await {
                eprintln!("[node {}] reject PhaseQc: bad QC sigs", w.my_id);
                return;
            }
            let vd = rs.vd_mut(view);
            match msg_type {
                MsgType::PreCommit => {
                    if !vd.seen_pre_commit_qc {
                        vd.seen_pre_commit_qc = true;
                        log_event(
                            w.my_id,
                            view,
                            "phase_qc_recv",
                            serde_json::json!({ "phase": "pre_commit" }),
                        );
                        rs.staged_pre_commit_qc.insert(view, justify);
                    }
                }
                MsgType::Commit => {
                    if !vd.seen_commit_qc {
                        vd.seen_commit_qc = true;
                        log_event(
                            w.my_id,
                            view,
                            "phase_qc_recv",
                            serde_json::json!({ "phase": "commit" }),
                        );
                        rs.staged_commit_qc.insert(view, justify);
                    }
                }
                MsgType::Decide => {
                    if !vd.seen_decide_qc {
                        vd.seen_decide_qc = true;
                        log_event(
                            w.my_id,
                            view,
                            "phase_qc_recv",
                            serde_json::json!({ "phase": "decide" }),
                        );
                        // Record commit independently of the per-view rule
                        // firing, so a DECIDE that arrives after we've
                        // advanced past `view` (e.g. via NEXTVIEW timing
                        // out mid-COMMIT) still drives `retry_pending_execute`.
                        rs.decided_nodes.insert(justify.node_hash);
                        rs.staged_decide_qc.insert(view, justify);
                    }
                }
                _ => unreachable!(),
            }
        }
        Wire::Vote {
            msg_type,
            view,
            node_hash: nh,
            voter,
            partial_sig,
        } => {
            // Only the LEADER of `view` is meant to receive votes.
            if rs.leader_for(view) != w.my_id {
                return;
            }
            let pk_opt = peer_keys.lock().await.get(&voter).copied();
            if voter != w.my_id {
                let pk = match pk_opt {
                    Some(k) => k,
                    None => {
                        eprintln!(
                            "[node {}] reject Vote: unknown voter {voter}",
                            w.my_id
                        );
                        return;
                    }
                };
                if !verify_vote_sig(&pk, msg_type, view, &nh, &partial_sig) {
                    eprintln!("[node {}] reject Vote: bad sig from {voter}", w.my_id);
                    return;
                }
            }
            let vd = rs.vd_mut(view);
            let bucket = match msg_type {
                MsgType::Prepare => &mut vd.prepare_votes,
                MsgType::PreCommit => &mut vd.precommit_votes,
                MsgType::Commit => &mut vd.commit_votes,
                _ => return,
            };
            if let Entry::Vacant(e) = bucket.entry(voter) {
                e.insert((nh, partial_sig));
                log_event(
                    w.my_id,
                    view,
                    "vote_recv",
                    serde_json::json!({
                        "phase": msg_type.as_str(),
                        "voter": voter,
                    }),
                );
            }
        }
        Wire::NewView {
            view,
            justify,
            sender,
        } => {
            // We collect NEW-VIEW(view=v-1) when we're the leader of view v.
            if rs.leader_for(view + 1) != w.my_id {
                return;
            }
            if let Some(qc) = &justify {
                if !verify_qc(qc, rs.quorum, peer_keys).await {
                    eprintln!("[node {}] reject NewView: bad justify sigs", w.my_id);
                    return;
                }
            }
            let vd = rs.vd_mut(view);
            if let Entry::Vacant(e) = vd.new_view_msgs.entry(sender) {
                e.insert(justify);
                log_event(
                    w.my_id,
                    view + 1,
                    "new_view_recv",
                    serde_json::json!({ "from_view": view, "sender": sender }),
                );
            }
        }
    }
}

/// NEXTVIEW interrupt: paper Alg 2 line 35. Aborts any "wait for" in any
/// phase, sends NEW-VIEW(curView, prepareQC) to leader(curView+1), advances
/// to curView+1 with timeout doubled.
pub fn handle_timer(rs: &mut ReplicaState, evt: TimerEvent, w: &Wiring<'_>) {
    if evt.view != rs.view {
        return; // stale
    }
    if rs.vd(rs.view).is_some_and(|v| v.seen_decide_qc) {
        return; // already decided this view
    }
    log_event(
        w.my_id,
        rs.view,
        "next_view",
        serde_json::json!({ "reason": "timeout" }),
    );
    rs.timeout_interval_ms = rs.timeout_interval_ms.saturating_mul(2);
    enter_new_view(rs, rs.view + 1, w);
}

/// Folds a finished validation into per-replica state. The caller invokes
/// `try_advance` afterward to fire any newly-eligible rules.
pub fn ingest_validation(rs: &mut ReplicaState, out: ValidateOutcome) {
    rs.validation_result.insert(out.node_hash, out.valid);
    if let Some(ps) = out.post_state {
        rs.post_state_by_node.insert(out.node_hash, ps);
    }
    if let Some(pr) = out.post_root {
        rs.post_root_by_node.insert(out.node_hash, pr);
    }
    if let Some(pr) = out.pre_root {
        rs.pre_root_by_node.insert(out.node_hash, pr);
    }
    if rs.pending_validation == Some(out.node_hash) {
        rs.pending_validation = None;
    }
}

/// View transition: send NEW-VIEW(view=prev_view, prepareQC) to the new
/// view's leader, arm the NEXTVIEW timer, log. Used both at startup and at
/// every successful DECIDE / NEXTVIEW.
pub(super) fn enter_new_view(rs: &mut ReplicaState, new_view: u64, w: &Wiring<'_>) {
    let prev_view = rs.view;
    rs.view = new_view;

    log_event(
        w.my_id,
        new_view,
        "view_start",
        serde_json::json!({
            "leader": rs.leader_for(new_view),
            "timeout_ms": rs.timeout_interval_ms,
        }),
    );

    let next_leader = rs.leader_for(new_view);
    let my_prepare_qc = rs.prepare_qc.clone();
    let nv_msg = Wire::NewView {
        view: prev_view,
        justify: my_prepare_qc.clone(),
        sender: w.my_id,
    };
    if next_leader == w.my_id {
        rs.vd_mut(prev_view)
            .new_view_msgs
            .insert(w.my_id, my_prepare_qc);
    } else {
        send_to(next_leader, nv_msg, w.peers);
    }
    log_event(
        w.my_id,
        new_view,
        "new_view_send",
        serde_json::json!({ "to": next_leader, "from_view": prev_view }),
    );

    arm_timer(
        TimerEvent { view: new_view },
        rs.timeout_interval_ms,
        w.timer_tx.clone(),
    );
}
