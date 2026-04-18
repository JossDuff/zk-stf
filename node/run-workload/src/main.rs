//! Replay a saved workload: re-execute each block and verify each proof.
//!
//! Compares locally computed state roots and tx hashes against the saved commit,
//! then verifies the SP1 proof. Stops on the first mismatch or verification failure.
//!
//! Usage:
//!   RUST_LOG=info cargo run --release -- \
//!     --workload-dir ../workloads/one_million \
//!     --elf-path ledger-program.elf

use clap::Parser;
use ledger_core::{apply_block, compute_state_root, hash_transactions, BlockCommit, State, Tx};
use serde::Deserialize;
use sp1_sdk::{
    blocking::{Prover, ProverClient},
    Elf, ProvingKey, SP1ProofWithPublicValues,
};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to the workload directory.
    #[arg(long)]
    workload_dir: PathBuf,

    /// Path to the prebuilt SP1 program ELF.
    #[arg(long)]
    elf_path: PathBuf,
}

#[derive(Deserialize)]
struct Manifest {
    num_accounts: u32,
    initial_balance: u64,
    num_txs_per_block: u32,
    num_blocks: u32,
}

#[derive(Deserialize)]
struct BlockMeta {
    block_number: u32,
    pre_state_root: String,
    post_state_root: String,
    tx_hash: String,
    txs_applied: u32,
    txs_total: u32,
}

fn main() {
    sp1_sdk::utils::setup_logger();

    let args = Args::parse();

    let manifest_path = args.workload_dir.join("manifest.json");
    let manifest: Manifest = serde_json::from_str(
        &fs::read_to_string(&manifest_path).expect("failed to read manifest.json"),
    )
    .expect("failed to parse manifest.json");

    println!(
        "Workload: {} blocks x {} txs, {} accounts, initial balance {}",
        manifest.num_blocks, manifest.num_txs_per_block, manifest.num_accounts, manifest.initial_balance
    );

    let elf_bytes = fs::read(&args.elf_path)
        .unwrap_or_else(|e| panic!("failed to read elf at {:?}: {}", args.elf_path, e));
    let elf: Elf = elf_bytes.into();

    let client = ProverClient::from_env();
    let pk = client.setup(elf).expect("failed to setup elf");

    let mut state = State::new();
    for i in 0..manifest.num_accounts {
        state.set_balance(i, manifest.initial_balance);
    }

    let mut exec_times: Vec<Duration> = Vec::with_capacity(manifest.num_blocks as usize);
    let mut verify_times: Vec<Duration> = Vec::with_capacity(manifest.num_blocks as usize);

    for block_num in 0..manifest.num_blocks {
        println!(
            "\n========== Block {}/{} ==========",
            block_num,
            manifest.num_blocks - 1
        );

        let block_dir = args.workload_dir.join(format!("block_{:04}", block_num));

        let meta: BlockMeta = serde_json::from_str(
            &fs::read_to_string(block_dir.join("commit.json"))
                .expect("failed to read commit.json"),
        )
        .expect("failed to parse commit.json");

        let txs_bytes =
            fs::read(block_dir.join("transactions.bin")).expect("failed to read transactions.bin");
        let txs: Vec<Tx> =
            bincode::deserialize(&txs_bytes).expect("failed to deserialize transactions");

        let exec_start = Instant::now();
        let pre_root = compute_state_root(&state);
        let mut new_state = state.clone();
        let applied = apply_block(&mut new_state, &txs);
        let post_root = compute_state_root(&new_state);
        let tx_hash = hash_transactions(&txs);
        let exec_elapsed = exec_start.elapsed();
        exec_times.push(exec_elapsed);

        println!("Execution: {:?} ({}/{} txs applied)", exec_elapsed, applied, meta.txs_total);

        let local_pre = hex::encode(pre_root);
        let local_post = hex::encode(post_root);
        let local_tx_hash = hex::encode(tx_hash);

        if local_pre != meta.pre_state_root {
            eprintln!(
                "MISMATCH block {}: pre_state_root\n  local:  {}\n  saved:  {}",
                block_num, local_pre, meta.pre_state_root
            );
            std::process::exit(1);
        }
        if local_post != meta.post_state_root {
            eprintln!(
                "MISMATCH block {}: post_state_root\n  local:  {}\n  saved:  {}",
                block_num, local_post, meta.post_state_root
            );
            std::process::exit(1);
        }
        if local_tx_hash != meta.tx_hash {
            eprintln!(
                "MISMATCH block {}: tx_hash\n  local:  {}\n  saved:  {}",
                block_num, local_tx_hash, meta.tx_hash
            );
            std::process::exit(1);
        }
        println!("State roots and tx hash match saved commit.");

        let proof_bytes =
            fs::read(block_dir.join("proof.bin")).expect("failed to read proof.bin");
        let proof: SP1ProofWithPublicValues =
            bincode::deserialize(&proof_bytes).expect("failed to deserialize proof");

        let commit: BlockCommit = proof.public_values.clone().read();
        if commit.pre_state_root != pre_root {
            eprintln!(
                "MISMATCH block {}: proof pre_state_root doesn't match local computation",
                block_num
            );
            std::process::exit(1);
        }
        if commit.post_state_root != post_root {
            eprintln!(
                "MISMATCH block {}: proof post_state_root doesn't match local computation",
                block_num
            );
            std::process::exit(1);
        }
        if commit.tx_hash != tx_hash {
            eprintln!(
                "MISMATCH block {}: proof tx_hash doesn't match local computation",
                block_num
            );
            std::process::exit(1);
        }

        let verify_start = Instant::now();
        client
            .verify(&proof, pk.verifying_key(), None)
            .unwrap_or_else(|e| {
                eprintln!("VERIFICATION FAILED block {}: {}", block_num, e);
                std::process::exit(1);
            });
        let verify_elapsed = verify_start.elapsed();
        verify_times.push(verify_elapsed);

        println!("Proof verified: {:?}", verify_elapsed);

        state = new_state;
    }

    let total_exec: Duration = exec_times.iter().sum();
    let total_verify: Duration = verify_times.iter().sum();
    let num_blocks = manifest.num_blocks as f64;

    let avg_exec = total_exec.as_secs_f64() / num_blocks;
    let avg_verify = total_verify.as_secs_f64() / num_blocks;

    println!("\n========== Summary ==========");
    println!("Blocks processed: {}", manifest.num_blocks);
    println!("Total execution time:      {:?}", total_exec);
    println!("Total verification time:   {:?}", total_verify);
    println!("Avg execution per block:   {:.3}s", avg_exec);
    println!("Avg verification per block: {:.3}s", avg_verify);
    println!("Avg exec/verify ratio:     {:.1}x", avg_exec / avg_verify);
    println!(
        "All {} blocks: state roots match, tx hashes match, proofs verified.",
        manifest.num_blocks
    );
}
