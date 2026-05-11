//! Wire format + ed25519 vote signatures for HotStuff.
//!
//! Per Yin et al. (PODC '19), Algorithm 2: messages are typed
//! NEW-VIEW / PREPARE / PRE-COMMIT / COMMIT / DECIDE. We split them into four
//! Wire variants that map to the paper's roles:
//!
//!   - `Proposal` — leader's PREPARE broadcast: a new leaf node + highQC.
//!   - `PhaseQc` — leader's PRE-COMMIT / COMMIT / DECIDE broadcast, carrying
//!     the previous phase's QC as `justify`.
//!   - `Vote` — replica's PREPARE / PRE-COMMIT / COMMIT vote, with a partial
//!     signature over (msg_type, view, node_hash).
//!   - `NewView` — replica's NEW-VIEW to the next view's leader, carrying
//!     the replica's current `prepareQC`.
//!
//! The paper uses (k, n)-threshold signatures so a QC is one O(1) authenticator.
//! We use plain aggregated ed25519: a QC is a `Vec<(voter_id, signature)>` of
//! at least `n - f` distinct voters. The authors' own reference implementation
//! (arXiv §8.1) likewise uses per-signer secp256k1, not threshold sigs;
//! threshold sigs are footnoted as an orthogonal optimization (Table 1).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum MsgType {
    NewView,
    Prepare,
    PreCommit,
    Commit,
    Decide,
}

impl MsgType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MsgType::NewView => "new_view",
            MsgType::Prepare => "prepare",
            MsgType::PreCommit => "pre_commit",
            MsgType::Commit => "commit",
            MsgType::Decide => "decide",
        }
    }
}

/// A tree node: parent link + the command (block tx_hash) it carries.
/// `height` mirrors the per-node height field that the practical
/// implementation in arXiv §6 / Algorithm 4 also tracks; it lets us cheaply
/// identify which block a node represents and walk ancestry chains.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Node {
    pub parent: [u8; 32],
    pub cmd: [u8; 32],
    pub height: u64,
}

/// Sentinel parent for the first real node. Genesis itself is virtual.
pub const GENESIS_HASH: [u8; 32] = [0u8; 32];

pub fn node_hash(n: &Node) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(n.parent);
    h.update(n.cmd);
    h.update(n.height.to_le_bytes());
    h.finalize().into()
}

/// Quorum certificate over (msg_type, view, node_hash). Aggregated ed25519
/// signatures from at least `n - f` distinct voters serve as the authenticator.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QC {
    pub msg_type: MsgType,
    pub view: u64,
    pub node_hash: [u8; 32],
    pub sigs: Vec<(u32, Vec<u8>)>,
}

/// Bytes signed by each voter to produce their partial signature in a vote.
pub fn vote_digest(msg_type: MsgType, view: u64, node_hash: &[u8; 32]) -> Vec<u8> {
    bincode::serialize(&(msg_type, view, *node_hash)).expect("vote digest")
}

pub fn sign_vote(
    sk: &SigningKey,
    msg_type: MsgType,
    view: u64,
    node_hash: &[u8; 32],
) -> Vec<u8> {
    let bytes = vote_digest(msg_type, view, node_hash);
    sk.sign(&bytes).to_bytes().to_vec()
}

pub fn verify_vote_sig(
    pk: &VerifyingKey,
    msg_type: MsgType,
    view: u64,
    node_hash: &[u8; 32],
    sig: &[u8],
) -> bool {
    let bytes = vote_digest(msg_type, view, node_hash);
    let sig = match Signature::from_slice(sig) {
        Ok(s) => s,
        Err(_) => return false,
    };
    pk.verify(&bytes, &sig).is_ok()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Wire {
    Hello {
        node_id: u32,
        pubkey: [u8; 32],
    },
    /// PREPARE phase from the leader: a freshly created leaf + the highQC
    /// that justifies it (`None` only for the very first view).
    Proposal {
        view: u64,
        node: Node,
        justify: Option<QC>,
        sender: u32,
    },
    /// PRE-COMMIT / COMMIT / DECIDE broadcast from the leader, carrying the
    /// previous phase's QC.
    PhaseQc {
        msg_type: MsgType,
        view: u64,
        justify: QC,
        sender: u32,
    },
    /// PREPARE / PRE-COMMIT / COMMIT vote from a replica.
    Vote {
        msg_type: MsgType,
        view: u64,
        node_hash: [u8; 32],
        voter: u32,
        partial_sig: Vec<u8>,
    },
    /// NEW-VIEW(view=v) is sent by a replica to the leader of view v+1 as it
    /// transitions out of view v. Carries the sender's current `prepareQC`.
    NewView {
        view: u64,
        justify: Option<QC>,
        sender: u32,
    },
}
