use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// A single balance transfer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tx {
    pub from: u32,
    pub to: u32,
    pub amount: u64,
}

/// Ledger state: account id -> balance.
/// Uses BTreeMap for deterministic iteration order when hashing.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct State {
    pub balances: BTreeMap<u32, u64>,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_balance(&self, account: u32) -> u64 {
        *self.balances.get(&account).unwrap_or(&0)
    }

    pub fn set_balance(&mut self, account: u32, amount: u64) {
        self.balances.insert(account, amount);
    }

    /// Deterministic SHA-256 hash of the full state.
    pub fn hash(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        for (account, balance) in &self.balances {
            hasher.update(account.to_le_bytes());
            hasher.update(balance.to_le_bytes());
        }
        hasher.finalize().into()
    }
}

/// Apply a block of transactions to the state.
/// Skips any tx where the sender has insufficient balance.
/// Returns the number of successfully applied transactions.
pub fn apply_block(state: &mut State, txs: &[Tx]) -> u32 {
    let mut applied = 0u32;
    for tx in txs {
        let from_bal = state.get_balance(tx.from);
        if from_bal >= tx.amount && tx.amount > 0 && tx.from != tx.to {
            state.set_balance(tx.from, from_bal - tx.amount);
            let to_bal = state.get_balance(tx.to);
            state.set_balance(tx.to, to_bal + tx.amount);
            applied += 1;
        }
    }
    applied
}

/// SHA-256 hash of a transaction list.
pub fn hash_transactions(txs: &[Tx]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for tx in txs {
        hasher.update(tx.from.to_le_bytes());
        hasher.update(tx.to.to_le_bytes());
        hasher.update(tx.amount.to_le_bytes());
    }
    hasher.finalize().into()
}

/// Public values committed by the ZK proof.
/// Validators check these against their expected pre-state hash
/// and the proposed transaction list hash.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockCommit {
    pub pre_state_hash: [u8; 32],
    pub post_state_hash: [u8; 32],
    pub tx_hash: [u8; 32],
}
