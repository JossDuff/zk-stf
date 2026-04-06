#![no_main]
sp1_zkvm::entrypoint!(main);

use ledger_lib::{apply_block, hash_transactions, BlockCommit, State, Tx};

pub fn main() {
    let state: State = sp1_zkvm::io::read();
    let txs: Vec<Tx> = sp1_zkvm::io::read();

    let pre_state_hash = state.hash();
    let tx_hash = hash_transactions(&txs);

    let mut new_state = state;
    apply_block(&mut new_state, &txs);

    let post_state_hash = new_state.hash();

    let commit = BlockCommit {
        pre_state_hash,
        post_state_hash,
        tx_hash,
    };
    sp1_zkvm::io::commit(&commit);
}
