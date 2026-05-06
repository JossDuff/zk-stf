//! Per-replica consensus state for Basic HotStuff.
//!
//! Variables follow paper Algorithm 1 / 2 naming:
//!   - `view`         (curView) — monotonically increasing.
//!   - `locked_qc`    (lockedQC) — set when we voted COMMIT.
//!   - `prepare_qc`   (prepareQC) — set when we voted PRE-COMMIT.
//!
//! Per-view bookkeeping (`ViewData`) tracks both replica-side flags
//! ("did I vote PREPARE for this view yet?") and leader-side vote collections
//! ("which voters' partial signatures have I gathered?"). The leader-only
//! fields are dormant in views where this replica isn't leader.

use crate::messages::{node_hash, Node, GENESIS_HASH, QC};
use ledger_core::State;
use std::collections::HashMap;

/// Per-view fired-once event. Stale ones (view != current) are dropped
/// in `consensus::handle_timer`.
#[derive(Clone, Debug)]
pub struct TimerEvent {
    pub view: u64,
}

#[derive(Default)]
pub struct ViewData {
    // ── as a replica ──
    /// `(leaf node, justify highQC)` from the leader's PREPARE broadcast.
    pub proposal: Option<(Node, Option<QC>)>,
    pub voted_prepare: bool,
    pub voted_precommit: bool,
    pub voted_commit: bool,
    pub seen_pre_commit_qc: bool,
    pub seen_commit_qc: bool,
    pub seen_decide_qc: bool,
    pub timer_armed: bool,

    // ── as a leader ──
    /// NEW-VIEW(view = this_view) messages from replicas; collected by the
    /// NEXT view's leader at view start. Stored under view = (curView - 1).
    pub new_view_msgs: HashMap<u32, Option<QC>>,
    pub prepare_votes: HashMap<u32, ([u8; 32], Vec<u8>)>,
    pub precommit_votes: HashMap<u32, ([u8; 32], Vec<u8>)>,
    pub commit_votes: HashMap<u32, ([u8; 32], Vec<u8>)>,
    pub broadcast_proposal: bool,
    pub broadcast_pre_commit: bool,
    pub broadcast_commit: bool,
    pub broadcast_decide: bool,
}

pub struct ReplicaState {
    pub n: u32,
    #[allow(dead_code)]
    pub f: u32,
    pub quorum: u32, // n - f

    pub view: u64,
    pub locked_qc: Option<QC>,
    pub prepare_qc: Option<QC>,

    pub last_executed_height: i64, // -1 if no commits yet

    pub view_data: HashMap<u64, ViewData>,
    pub tree: HashMap<[u8; 32], Node>,

    /// `valid(v)` cache, keyed by node_hash. Populated by the validator
    /// worker; consulted by the propose-step rule when deciding whether to
    /// vote PREPARE for a node.
    pub validation_result: HashMap<[u8; 32], bool>,
    /// Single in-flight validation hash (worker is serial).
    pub pending_validation: Option<[u8; 32]>,
    /// reexecute mode: post-state to swap into live state at execute time.
    pub post_state_by_node: HashMap<[u8; 32], State>,
    /// verify mode: post-state root, becomes the next height's chain head.
    pub post_root_by_node: HashMap<[u8; 32], [u8; 32]>,
    /// verify mode: pre-state root, checked against the chain at execute time.
    pub pre_root_by_node: HashMap<[u8; 32], [u8; 32]>,

    /// Nodes for which we've seen a valid commitQC (DECIDE phase fired). The
    /// node may or may not have been executed locally yet (depends on whether
    /// validation has completed).
    pub decided_nodes: HashMap<[u8; 32], QC>,

    /// height -> node_hash that committed it. Built up at execute time.
    pub committed_at_height: HashMap<u64, [u8; 32]>,

    /// QCs delivered via PhaseQc messages, indexed by view. Replica-side
    /// rules consume these in the corresponding phase.
    pub staged_pre_commit_qc: HashMap<u64, QC>,
    pub staged_commit_qc: HashMap<u64, QC>,
    pub staged_decide_qc: HashMap<u64, QC>,

    /// Pacemaker (paper Section 6): replica's current per-view timeout. Reset
    /// to base on a successful decision; doubled on every NEXTVIEW.
    pub timeout_interval_ms: u64,
}

impl ReplicaState {
    pub fn new(n: u32, timeout_base_ms: u64) -> Self {
        let f = (n - 1) / 3;
        let quorum = n - f;
        Self {
            n,
            f,
            quorum,
            view: 0, // becomes 1 when we enter the first view
            locked_qc: None,
            prepare_qc: None,
            last_executed_height: -1,
            view_data: HashMap::new(),
            tree: HashMap::new(),
            validation_result: HashMap::new(),
            pending_validation: None,
            post_state_by_node: HashMap::new(),
            post_root_by_node: HashMap::new(),
            pre_root_by_node: HashMap::new(),
            decided_nodes: HashMap::new(),
            committed_at_height: HashMap::new(),
            staged_pre_commit_qc: HashMap::new(),
            staged_commit_qc: HashMap::new(),
            staged_decide_qc: HashMap::new(),
            timeout_interval_ms: timeout_base_ms,
        }
    }

    /// Round-robin: view `v` is led by node `(v - 1) mod n`. View 1 → node 0.
    /// Paper Section 6: "a rotating leader scheme in which all correct
    /// replicas keep a predefined leader schedule."
    pub fn leader_for(&self, view: u64) -> u32 {
        if view == 0 {
            return 0;
        }
        ((view - 1) % self.n as u64) as u32
    }

    pub fn vd_mut(&mut self, view: u64) -> &mut ViewData {
        self.view_data.entry(view).or_default()
    }
    pub fn vd(&self, view: u64) -> Option<&ViewData> {
        self.view_data.get(&view)
    }
}

/// SafeNode predicate (paper Algorithm 1, lines 25-27):
///
/// ```text
/// safeNode(node, qc) :=
///   (node extends from lockedQC.node)        // safety rule
///   OR (qc.viewNumber > lockedQC.viewNumber) // liveness rule
/// ```
///
/// The two-rule disjunction lets a replica unlock from a stale `lockedQC`
/// when a higher-view QC arrives — the three-phase paradigm guarantees
/// safety even so.
pub fn safe_node(
    node: &Node,
    qc: &Option<QC>,
    locked_qc: &Option<QC>,
    tree: &HashMap<[u8; 32], Node>,
) -> bool {
    let locked = match locked_qc {
        Some(l) => l,
        None => return true, // no lock yet; trivially safe
    };
    if extends_from(node, &locked.node_hash, tree) {
        return true;
    }
    match qc {
        Some(q) => q.view > locked.view,
        None => false,
    }
}

/// True if `target_hash` is `node` itself or one of its ancestors via
/// parent pointers in `tree`.
pub fn extends_from(
    node: &Node,
    target_hash: &[u8; 32],
    tree: &HashMap<[u8; 32], Node>,
) -> bool {
    if &node_hash(node) == target_hash {
        return true;
    }
    let mut cur = node.parent;
    let mut steps = 0u64;
    loop {
        if &cur == target_hash {
            return true;
        }
        if cur == GENESIS_HASH {
            return false;
        }
        match tree.get(&cur) {
            Some(parent_node) => cur = parent_node.parent,
            None => return false, // unknown ancestor; can't verify
        }
        steps += 1;
        if steps > 100_000 {
            return false; // defensive, shouldn't happen on a sane workload
        }
    }
}
