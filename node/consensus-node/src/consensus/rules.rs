//! Per-phase fire-once rules for Basic HotStuff (paper Algorithm 2).
//!
//! Each rule is guarded so it fires at most once per (view, role) and only
//! when its preconditions hold. They're called in sequence by `try_advance`
//! after every event; the top-level orchestration is in `mod.rs`.
//!
//! Rule naming maps directly to paper line ranges:
//!   - `leader_prepare`     — lines 2-6   (broadcast PROPOSAL)
//!   - `replica_prepare`    — lines 8-10  (vote PREPARE)
//!   - `leader_pre_commit`  — lines 12-14 (broadcast prepareQC)
//!   - `replica_pre_commit` — lines 16-18 (set prepareQC, vote PRE-COMMIT)
//!   - `leader_commit`      — lines 20-22 (broadcast precommitQC)
//!   - `replica_commit`     — lines 24-26 (set lockedQC, vote COMMIT)
//!   - `leader_decide`      — lines 28-30 (broadcast commitQC)
//!   - `replica_decide`     — lines 32-34 (execute, advance view)

use super::execute::{maybe_kick_validation, try_execute_chain};
use super::handlers::enter_new_view;
use super::qc::form_qc;
use super::Wiring;
use crate::logging::log_event;
use crate::messages::{node_hash, sign_vote, MsgType, Node, Wire, GENESIS_HASH};
use crate::network::{broadcast, send_to};
use crate::state::{safe_node, ReplicaState};
use ledger_core::State;

/// Alg 2 lines 2-6: leader collects (n-f) NEW-VIEW(view=curView-1), picks the
/// highest prepareQC as highQC, builds and broadcasts PREPARE proposal.
pub(super) fn leader_prepare(rs: &mut ReplicaState, w: &Wiring<'_>, live_state: &State) {
    let v = rs.view;
    if rs.leader_for(v) != w.my_id {
        return;
    }
    if rs.vd_mut(v).broadcast_proposal {
        return;
    }
    let prev_view = v.saturating_sub(1);
    let count = rs.vd(prev_view).map_or(0, |d| d.new_view_msgs.len());
    if count < rs.quorum as usize {
        return;
    }

    // highQC = argmax{m.justify.viewNumber}.justify, may be ⊥ if all are ⊥.
    let high_qc = rs
        .vd(prev_view)
        .unwrap()
        .new_view_msgs
        .values()
        .filter_map(|j| j.clone())
        .max_by_key(|q| q.view);

    let parent_hash = high_qc.as_ref().map(|q| q.node_hash).unwrap_or(GENESIS_HASH);
    let new_height = match &high_qc {
        Some(q) => match rs.tree.get(&q.node_hash) {
            Some(parent_node) => parent_node.height + 1,
            None => {
                eprintln!(
                    "[node {}] leader_prepare: highQC.node not in tree, skipping",
                    w.my_id
                );
                return;
            }
        },
        None => 0,
    };

    if new_height >= w.workload.manifest.num_blocks as u64 {
        return; // workload exhausted
    }

    let block = w.workload.block(new_height);
    let leaf = Node {
        parent: parent_hash,
        cmd: block.cmd,
        height: new_height,
    };
    let leaf_hash = node_hash(&leaf);
    rs.tree.insert(leaf_hash, leaf.clone());

    let msg = Wire::Proposal {
        view: v,
        node: leaf.clone(),
        justify: high_qc.clone(),
        sender: w.my_id,
    };
    log_event(
        w.my_id,
        v,
        "propose_send",
        serde_json::json!({
            "leaf_hash": hex::encode(leaf_hash),
            "leaf_height": new_height,
            "high_qc_view": high_qc.as_ref().map(|q| q.view).unwrap_or(0),
        }),
    );
    broadcast(msg, w.peers);

    let vd = rs.vd_mut(v);
    if vd.proposal.is_none() {
        vd.proposal = Some((leaf, high_qc));
    }
    vd.broadcast_proposal = true;

    // Leader is also a replica; kick off validation locally.
    maybe_kick_validation(rs, w, leaf_hash, live_state);
}

/// Alg 2 lines 8-10: replica receives PREPARE, checks that leaf extends from
/// highQC.node and SafeNode(leaf, highQC). Adds application-level valid()
/// gate per the experiment design. Sends PREPARE vote to leader.
pub(super) fn replica_prepare(rs: &mut ReplicaState, w: &Wiring<'_>) {
    let v = rs.view;
    let proposal = match rs.vd(v).and_then(|d| d.proposal.clone()) {
        Some(p) => p,
        None => return,
    };
    if rs.vd_mut(v).voted_prepare {
        return;
    }
    let (leaf, high_qc) = proposal;
    let leaf_hash = node_hash(&leaf);

    // Application validity gate.
    let is_valid = match rs.validation_result.get(&leaf_hash).copied() {
        Some(b) => b,
        None => return, // wait for validation
    };
    if !is_valid {
        rs.vd_mut(v).voted_prepare = true;
        log_event(
            w.my_id,
            v,
            "prepare_skip",
            serde_json::json!({ "reason": "valid_false" }),
        );
        return;
    }

    // Consensus safety gate.
    if !safe_node(&leaf, &high_qc, &rs.locked_qc, &rs.tree) {
        rs.vd_mut(v).voted_prepare = true;
        log_event(
            w.my_id,
            v,
            "prepare_skip",
            serde_json::json!({ "reason": "safe_node_false" }),
        );
        return;
    }

    let sig = sign_vote(w.sk, MsgType::Prepare, v, &leaf_hash);
    let vote = Wire::Vote {
        msg_type: MsgType::Prepare,
        view: v,
        node_hash: leaf_hash,
        voter: w.my_id,
        partial_sig: sig.clone(),
    };
    let leader = rs.leader_for(v);
    if leader == w.my_id {
        rs.vd_mut(v)
            .prepare_votes
            .insert(w.my_id, (leaf_hash, sig));
    } else {
        send_to(leader, vote, w.peers);
    }
    rs.vd_mut(v).voted_prepare = true;
    log_event(
        w.my_id,
        v,
        "prepare_vote_send",
        serde_json::json!({ "leaf_hash": hex::encode(leaf_hash) }),
    );
}

/// Alg 2 lines 12-14: leader collects (n-f) PREPARE votes, forms prepareQC,
/// broadcasts PRE-COMMIT.
pub(super) fn leader_pre_commit(rs: &mut ReplicaState, w: &Wiring<'_>) {
    let v = rs.view;
    if rs.leader_for(v) != w.my_id {
        return;
    }
    if rs.vd_mut(v).broadcast_pre_commit {
        return;
    }
    let qc = match form_qc(
        rs.vd(v).map_or(&Default::default(), |d| &d.prepare_votes),
        MsgType::Prepare,
        v,
        rs.quorum,
    ) {
        Some(q) => q,
        None => return,
    };
    let msg = Wire::PhaseQc {
        msg_type: MsgType::PreCommit,
        view: v,
        justify: qc.clone(),
        sender: w.my_id,
    };
    log_event(
        w.my_id,
        v,
        "phase_qc_send",
        serde_json::json!({ "phase": "pre_commit", "node_hash": hex::encode(qc.node_hash) }),
    );
    broadcast(msg, w.peers);

    let vd = rs.vd_mut(v);
    vd.broadcast_pre_commit = true;
    if !vd.seen_pre_commit_qc {
        vd.seen_pre_commit_qc = true;
        rs.staged_pre_commit_qc.insert(v, qc);
    }
}

/// Alg 2 lines 16-18: replica receives PRE-COMMIT, sets prepareQC ←
/// m.justify, votes PRE-COMMIT.
pub(super) fn replica_pre_commit(rs: &mut ReplicaState, w: &Wiring<'_>) {
    let v = rs.view;
    let vd = rs.vd_mut(v);
    if !vd.seen_pre_commit_qc || vd.voted_precommit {
        return;
    }
    let qc = match rs.staged_pre_commit_qc.get(&v).cloned() {
        Some(q) => q,
        None => return,
    };
    rs.prepare_qc = Some(qc.clone());
    let nh = qc.node_hash;

    let sig = sign_vote(w.sk, MsgType::PreCommit, v, &nh);
    let vote = Wire::Vote {
        msg_type: MsgType::PreCommit,
        view: v,
        node_hash: nh,
        voter: w.my_id,
        partial_sig: sig.clone(),
    };
    let leader = rs.leader_for(v);
    if leader == w.my_id {
        rs.vd_mut(v).precommit_votes.insert(w.my_id, (nh, sig));
    } else {
        send_to(leader, vote, w.peers);
    }
    rs.vd_mut(v).voted_precommit = true;
    log_event(
        w.my_id,
        v,
        "pre_commit_vote_send",
        serde_json::json!({ "node_hash": hex::encode(nh) }),
    );
}

/// Alg 2 lines 20-22: leader collects (n-f) PRE-COMMIT votes, forms
/// precommitQC, broadcasts COMMIT.
pub(super) fn leader_commit(rs: &mut ReplicaState, w: &Wiring<'_>) {
    let v = rs.view;
    if rs.leader_for(v) != w.my_id {
        return;
    }
    if rs.vd_mut(v).broadcast_commit {
        return;
    }
    let qc = match form_qc(
        rs.vd(v).map_or(&Default::default(), |d| &d.precommit_votes),
        MsgType::PreCommit,
        v,
        rs.quorum,
    ) {
        Some(q) => q,
        None => return,
    };
    let msg = Wire::PhaseQc {
        msg_type: MsgType::Commit,
        view: v,
        justify: qc.clone(),
        sender: w.my_id,
    };
    log_event(
        w.my_id,
        v,
        "phase_qc_send",
        serde_json::json!({ "phase": "commit", "node_hash": hex::encode(qc.node_hash) }),
    );
    broadcast(msg, w.peers);

    let vd = rs.vd_mut(v);
    vd.broadcast_commit = true;
    if !vd.seen_commit_qc {
        vd.seen_commit_qc = true;
        rs.staged_commit_qc.insert(v, qc);
    }
}

/// Alg 2 lines 24-26: replica receives COMMIT, sets lockedQC ← m.justify,
/// votes COMMIT.
pub(super) fn replica_commit(rs: &mut ReplicaState, w: &Wiring<'_>) {
    let v = rs.view;
    let vd = rs.vd_mut(v);
    if !vd.seen_commit_qc || vd.voted_commit {
        return;
    }
    let qc = match rs.staged_commit_qc.get(&v).cloned() {
        Some(q) => q,
        None => return,
    };
    rs.locked_qc = Some(qc.clone());
    let nh = qc.node_hash;

    let sig = sign_vote(w.sk, MsgType::Commit, v, &nh);
    let vote = Wire::Vote {
        msg_type: MsgType::Commit,
        view: v,
        node_hash: nh,
        voter: w.my_id,
        partial_sig: sig.clone(),
    };
    let leader = rs.leader_for(v);
    if leader == w.my_id {
        rs.vd_mut(v).commit_votes.insert(w.my_id, (nh, sig));
    } else {
        send_to(leader, vote, w.peers);
    }
    rs.vd_mut(v).voted_commit = true;
    log_event(
        w.my_id,
        v,
        "commit_vote_send",
        serde_json::json!({ "node_hash": hex::encode(nh) }),
    );
}

/// Alg 2 lines 28-30: leader collects (n-f) COMMIT votes, forms commitQC,
/// broadcasts DECIDE.
pub(super) fn leader_decide(rs: &mut ReplicaState, w: &Wiring<'_>) {
    let v = rs.view;
    if rs.leader_for(v) != w.my_id {
        return;
    }
    if rs.vd_mut(v).broadcast_decide {
        return;
    }
    let qc = match form_qc(
        rs.vd(v).map_or(&Default::default(), |d| &d.commit_votes),
        MsgType::Commit,
        v,
        rs.quorum,
    ) {
        Some(q) => q,
        None => return,
    };
    let msg = Wire::PhaseQc {
        msg_type: MsgType::Decide,
        view: v,
        justify: qc.clone(),
        sender: w.my_id,
    };
    log_event(
        w.my_id,
        v,
        "phase_qc_send",
        serde_json::json!({ "phase": "decide", "node_hash": hex::encode(qc.node_hash) }),
    );
    broadcast(msg, w.peers);

    let vd = rs.vd_mut(v);
    vd.broadcast_decide = true;
    if !vd.seen_decide_qc {
        vd.seen_decide_qc = true;
        rs.staged_decide_qc.insert(v, qc);
    }
}

/// Alg 2 lines 32-34: replica receives DECIDE, executes commands through
/// commitQC.node (walking back to last_executed). Resets timeout, advances
/// to view+1.
pub(super) fn replica_decide(
    rs: &mut ReplicaState,
    w: &Wiring<'_>,
    live_state: &mut State,
    chained_root: &mut [u8; 32],
) {
    let v = rs.view;
    if !rs.vd_mut(v).seen_decide_qc {
        return;
    }
    let qc = match rs.staged_decide_qc.get(&v).cloned() {
        Some(q) => q,
        None => return,
    };
    rs.decided_nodes.insert(qc.node_hash);

    try_execute_chain(rs, w, live_state, chained_root, qc.node_hash);

    log_event(
        w.my_id,
        v,
        "view_decide",
        serde_json::json!({ "node_hash": hex::encode(qc.node_hash) }),
    );
    // Reset Pacemaker timeout on successful decision (paper Section 6).
    rs.timeout_interval_ms = w.timeout_base_ms;
    enter_new_view(rs, v + 1, w);
}
