#![no_main]
sp1_zkvm::entrypoint!(main);

use ledger_core::{apply_block, compute_state_root, hash_transactions, BlockCommit, State, Tx};

pub fn main() {
    let state: State = sp1_zkvm::io::read();
    let txs: Vec<Tx> = sp1_zkvm::io::read();

    let pre_state_root = compute_state_root(&state);
    let tx_hash = hash_transactions(&txs);

    let mut new_state = state;
    apply_block(&mut new_state, &txs);

    let post_state_root = compute_state_root(&new_state);

    let commit = BlockCommit {
        pre_state_root,
        post_state_root,
        tx_hash,
    };
    sp1_zkvm::io::commit(&commit);
}
