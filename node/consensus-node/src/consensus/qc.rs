//! Quorum-certificate helpers: aggregate (n-f) partial signatures, verify them.
//!
//! The paper uses (k, n)-threshold signatures so a QC is one O(1) authenticator;
//! we use plain aggregated ed25519 (Vec<(voter_id, sig)>). To verify a QC we
//! walk its sigs, look up each voter's pubkey, and accept once we've counted
//! `quorum` distinct valid signers. See `messages.rs` for the per-vote digest.

use crate::messages::{verify_vote_sig, MsgType, QC};
use crate::network::PeerKeys;
use ed25519_dalek::VerifyingKey;
use std::collections::{HashMap, HashSet};

/// Form a QC from a vote bucket if some `node_hash` has ≥ `quorum` votes.
pub(super) fn form_qc(
    votes: &HashMap<u32, ([u8; 32], Vec<u8>)>,
    msg_type: MsgType,
    view: u64,
    quorum: u32,
) -> Option<QC> {
    let mut by_node: HashMap<[u8; 32], Vec<(u32, Vec<u8>)>> = HashMap::new();
    for (voter, (nh, sig)) in votes {
        by_node.entry(*nh).or_default().push((*voter, sig.clone()));
    }
    for (nh, sigs) in by_node {
        if sigs.len() >= quorum as usize {
            return Some(QC {
                msg_type,
                view,
                node_hash: nh,
                sigs,
            });
        }
    }
    None
}

/// Async wrapper that locks `peer_keys` once before delegating to the sync
/// verifier. Use this from message handlers where we already `await` the lock.
pub(super) async fn verify_qc(qc: &QC, quorum: u32, peer_keys: &PeerKeys) -> bool {
    let keys = peer_keys.lock().await;
    verify_qc_sync(qc, quorum, &keys)
}

fn verify_qc_sync(qc: &QC, quorum: u32, keys: &HashMap<u32, VerifyingKey>) -> bool {
    let mut seen: HashSet<u32> = HashSet::new();
    let mut count = 0u32;
    for (voter, sig) in &qc.sigs {
        if seen.contains(voter) {
            continue;
        }
        let pk = match keys.get(voter) {
            Some(k) => k,
            None => continue,
        };
        if verify_vote_sig(pk, qc.msg_type, qc.view, &qc.node_hash, sig) {
            seen.insert(*voter);
            count += 1;
        }
    }
    count >= quorum
}
