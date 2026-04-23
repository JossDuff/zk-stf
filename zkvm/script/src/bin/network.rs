//! Generate a multi-block workload and prove each block on the Succinct Prover Network.
//!
//! Saves transactions, commits, and proofs to a workload directory for later benchmarking.
//!
//! Requires NETWORK_PRIVATE_KEY env var (or in .env file).
//!
//! Usage:
//!   RUST_LOG=info cargo run --release --bin network -- \
//!     --num-blocks 100 --num-txs 1000000 --num-accounts 10000 --workload-dir workloads/run1

use clap::Parser;
use ledger_core::{apply_block, compute_state_root, hash_transactions, BlockCommit, State, Tx};
use serde::{Deserialize, Serialize};
use sp1_sdk::{
    include_elf, network::NetworkMode, Elf, ProveRequest, Prover, ProverClient, ProvingKey,
    SP1Stdin,
};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

const LEDGER_ELF: Elf = include_elf!("ledger-program");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Number of blocks to generate and prove.
    #[arg(long, default_value = "1")]
    num_blocks: u32,

    /// Number of transactions per block.
    #[arg(long, default_value = "100")]
    num_txs: u32,

    /// Number of accounts in the ledger.
    #[arg(long, default_value = "10000")]
    num_accounts: u32,

    /// Starting balance for each account.
    #[arg(long, default_value = "10000")]
    initial_balance: u64,

    /// Directory to save the workload.
    #[arg(long, default_value = "workloads/default")]
    workload_dir: PathBuf,

    /// Block number to resume from (skips already-generated blocks).
    #[arg(long, default_value = "0")]
    resume_from: u32,
}

/// Saved alongside the workload so the benchmark can reconstruct genesis state.
#[derive(Serialize, Deserialize)]
struct Manifest {
    num_accounts: u32,
    initial_balance: u64,
    num_txs_per_block: u32,
    num_blocks: u32,
}

/// Per-block metadata saved as JSON.
#[derive(Serialize, Deserialize)]
struct BlockMeta {
    block_number: u32,
    pre_state_root: String,
    post_state_root: String,
    tx_hash: String,
    txs_applied: u32,
    txs_total: u32,
    native_execution_us: u64,
    network_prove_us: u64,
    verify_us: u64,
}

/// Deterministic pseudo-random transaction generation.
/// Takes a mutable seed so it advances across blocks.
fn generate_txs(num_txs: u32, num_accounts: u32, seed: &mut u64) -> Vec<Tx> {
    let mut txs = Vec::with_capacity(num_txs as usize);
    for _ in 0..num_txs {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let from = (*seed % num_accounts as u64) as u32;
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mut to = (*seed % num_accounts as u64) as u32;
        if to == from {
            to = (to + 1) % num_accounts;
        }
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let amount = (*seed % 10) + 1;
        txs.push(Tx { from, to, amount });
    }
    txs
}

fn generate_initial_state(num_accounts: u32, initial_balance: u64) -> State {
    let mut state = State::new();
    for i in 0..num_accounts {
        state.set_balance(i, initial_balance);
    }
    state
}

#[tokio::main]
async fn main() {
    sp1_sdk::utils::setup_logger();
    dotenv::dotenv().ok();

    let args = Args::parse();

    // Create workload directory structure.
    fs::create_dir_all(&args.workload_dir).expect("failed to create workload directory");

    // Write the ELF alongside the blocks so verifying nodes load exactly the
    // binary that was used to derive the vk baked into these proofs. Skipping
    // this (or copying a freshly-rebuilt ELF over it later) yields
    // `invalid public values: sp1 vk hash mismatch` at verify time.
    let elf_path = args.workload_dir.join("ledger-program.elf");
    fs::write(&elf_path, &*LEDGER_ELF).expect("failed to write ELF");
    println!(
        "Wrote ELF: {} ({} bytes)",
        elf_path.display(),
        LEDGER_ELF.len()
    );

    // Write manifest.
    let manifest = Manifest {
        num_accounts: args.num_accounts,
        initial_balance: args.initial_balance,
        num_txs_per_block: args.num_txs,
        num_blocks: args.num_blocks,
    };
    let manifest_path = args.workload_dir.join("manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .expect("failed to write manifest");

    println!(
        "Workload: {} blocks x {} txs, {} accounts",
        args.num_blocks, args.num_txs, args.num_accounts
    );
    println!("Saving to: {}", args.workload_dir.display());

    // Connect to the Succinct Prover Network.
    let client = ProverClient::builder()
        .network_for(NetworkMode::Mainnet)
        .build()
        .await;

    let pk = client.setup(LEDGER_ELF).await.unwrap();

    // Initialize state and seed.
    let mut state = generate_initial_state(args.num_accounts, args.initial_balance);
    let mut seed: u64 = 12345;

    // If resuming, replay state forward through skipped blocks.
    if args.resume_from > 0 {
        println!(
            "Replaying blocks 0..{} to reconstruct state...",
            args.resume_from
        );
        for _ in 0..args.resume_from {
            let txs = generate_txs(args.num_txs, args.num_accounts, &mut seed);
            apply_block(&mut state, &txs);
        }
        println!(
            "State reconstructed, resuming from block {}",
            args.resume_from
        );
    }

    for block_num in args.resume_from..args.num_blocks {
        println!(
            "\n========== Block {}/{} ==========",
            block_num, args.num_blocks - 1
        );

        let block_dir = args.workload_dir.join(format!("block_{:04}", block_num));
        fs::create_dir_all(&block_dir).expect("failed to create block directory");

        // Generate transactions for this block.
        let txs = generate_txs(args.num_txs, args.num_accounts, &mut seed);

        // Native execution.
        let native_start = Instant::now();
        let pre_root = compute_state_root(&state);
        let mut new_state = state.clone();
        let applied = apply_block(&mut new_state, &txs);
        let post_root = compute_state_root(&new_state);
        let tx_hash = hash_transactions(&txs);
        let native_elapsed = native_start.elapsed();

        println!(
            "Native execution: {:?} ({} txs applied)",
            native_elapsed, applied
        );

        // Save transactions (bincode).
        let txs_path = block_dir.join("transactions.bin");
        let txs_bytes = bincode::serialize(&txs).expect("failed to serialize transactions");
        fs::write(&txs_path, &txs_bytes).expect("failed to write transactions");
        println!(
            "Saved transactions: {} ({:.1} MB)",
            txs_path.display(),
            txs_bytes.len() as f64 / 1_000_000.0
        );

        // Prove on network.
        let mut stdin = SP1Stdin::new();
        stdin.write(&state);
        stdin.write(&txs);

        println!("Submitting to Succinct Prover Network...");
        let prove_start = Instant::now();
        let proof = client
            .prove(&pk, stdin)
            .compressed()
            .await
            .expect("failed to generate proof on network");
        let prove_elapsed = prove_start.elapsed();
        println!("Network prove time: {:?}", prove_elapsed);

        // Verify public values match.
        let commit: BlockCommit = proof.public_values.clone().read();
        assert_eq!(commit.pre_state_root, pre_root);
        assert_eq!(commit.post_state_root, post_root);
        assert_eq!(commit.tx_hash, tx_hash);

        // Verify proof.
        let verify_start = Instant::now();
        client
            .verify(&proof, pk.verifying_key(), None)
            .expect("failed to verify proof");
        let verify_elapsed = verify_start.elapsed();
        println!("Verify time: {:?}", verify_elapsed);

        // Save proof (bincode).
        let proof_path = block_dir.join("proof.bin");
        let proof_bytes = bincode::serialize(&proof).expect("failed to serialize proof");
        fs::write(&proof_path, &proof_bytes).expect("failed to write proof");
        println!(
            "Saved proof: {} ({:.1} MB)",
            proof_path.display(),
            proof_bytes.len() as f64 / 1_000_000.0
        );

        // Save block metadata (JSON).
        let meta = BlockMeta {
            block_number: block_num,
            pre_state_root: hex::encode(pre_root),
            post_state_root: hex::encode(post_root),
            tx_hash: hex::encode(tx_hash),
            txs_applied: applied,
            txs_total: args.num_txs,
            native_execution_us: native_elapsed.as_micros() as u64,
            network_prove_us: prove_elapsed.as_micros() as u64,
            verify_us: verify_elapsed.as_micros() as u64,
        };
        let meta_path = block_dir.join("commit.json");
        fs::write(&meta_path, serde_json::to_string_pretty(&meta).unwrap())
            .expect("failed to write block metadata");

        println!(
            "Block {} complete: native={:?}, prove={:?}, verify={:?}",
            block_num, native_elapsed, prove_elapsed, verify_elapsed
        );

        // Advance state for the next block.
        state = new_state;
    }

    // Update manifest with final block count.
    let manifest = Manifest {
        num_accounts: args.num_accounts,
        initial_balance: args.initial_balance,
        num_txs_per_block: args.num_txs,
        num_blocks: args.num_blocks,
    };
    fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .expect("failed to update manifest");

    println!("\n========== Done ==========");
    println!("Workload saved to: {}", args.workload_dir.display());
}
