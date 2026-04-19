//! consensus-node: shell for the upcoming BFT consensus binary.
//!
//! For now: no networking. Each node independently replays the workload, using
//! the (speed, mode) dispatch that the real consensus will eventually use:
//!
//!   fast + reexecute → apply_block
//!   fast + verify    → apply_block (fast nodes always re-execute)
//!   slow + reexecute → apply_block + sleep(slow_delay_ms)
//!   slow + verify    → client.verify(proof); chain state_root from BlockCommit
//!
//! sun.sh invokes this with all the consensus flags already in place so the
//! deploy/build/launch plumbing gets exercised end-to-end before networking
//! lands.

use clap::{Parser, ValueEnum};
use ledger_core::{apply_block, BlockCommit, State, Tx};
use serde::Deserialize;
use sp1_sdk::{
    blocking::{Prover, ProverClient},
    Elf, ProvingKey, SP1ProofWithPublicValues,
};
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Speed {
    Fast,
    Slow,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Mode {
    Reexecute,
    Verify,
}

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    node_id: u32,

    #[arg(long, value_enum)]
    speed: Speed,

    /// Comma-separated peer hostnames. Parsed but unused until networking lands.
    #[arg(long)]
    peers: String,

    #[arg(long, default_value_t = 1895)]
    port: u16,

    #[arg(long, value_enum)]
    mode: Mode,

    #[arg(long)]
    workload: String,

    #[arg(long)]
    workloads_dir: PathBuf,

    /// Per-block sleep in ms for slow + reexecute mode.
    #[arg(long, default_value_t = 0)]
    slow_delay_ms: u64,
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
    #[allow(dead_code)]
    block_number: u32,
    #[allow(dead_code)]
    pre_state_root: String,
    #[allow(dead_code)]
    post_state_root: String,
    #[allow(dead_code)]
    tx_hash: String,
    #[allow(dead_code)]
    txs_applied: u32,
    txs_total: u32,
}

fn main() {
    sp1_sdk::utils::setup_logger();
    let args = Args::parse();

    let workload_dir = args.workloads_dir.join(&args.workload);
    let elf_path = workload_dir.join("ledger-program.elf");

    let tag = format!("[node {}]", args.node_id);

    println!(
        "{tag} speed={:?} mode={:?} workload={} peers={} port={}",
        args.speed, args.mode, args.workload, args.peers, args.port
    );
    println!("{tag} workload_dir={:?}", workload_dir);

    let manifest: Manifest = serde_json::from_str(
        &fs::read_to_string(workload_dir.join("manifest.json"))
            .expect("failed to read manifest.json"),
    )
    .expect("failed to parse manifest.json");

    println!(
        "{tag} workload: {} blocks × {} txs, {} accounts, initial_balance={}",
        manifest.num_blocks,
        manifest.num_txs_per_block,
        manifest.num_accounts,
        manifest.initial_balance,
    );

    let slow_verify = matches!((args.speed, args.mode), (Speed::Slow, Mode::Verify));
    let slow_reexec = matches!((args.speed, args.mode), (Speed::Slow, Mode::Reexecute));

    // Slow+verify: set up prover/verifying key once; no State carried.
    // Everyone else: init full State from manifest.
    let verifier = if slow_verify {
        println!("{tag} loading elf + prover setup...");
        let elf_bytes = fs::read(&elf_path)
            .unwrap_or_else(|e| panic!("failed to read elf at {elf_path:?}: {e}"));
        let elf: Elf = elf_bytes.into();
        let client = ProverClient::from_env();
        let pk = client.setup(elf).expect("failed to setup elf");
        println!("{tag} prover setup done");
        Some((client, pk))
    } else {
        None
    };

    let mut state: Option<State> = (!slow_verify).then(|| {
        let mut s = State::new();
        for i in 0..manifest.num_accounts {
            s.set_balance(i, manifest.initial_balance);
        }
        s
    });

    // For slow+verify only: chain pre → post through proof public values.
    let mut chained_root: Option<[u8; 32]> = None;

    let mut total_validate = Duration::ZERO;
    let mut total_txs: u64 = 0;
    let run_start = Instant::now();

    for block_num in 0..manifest.num_blocks {
        let block_dir = workload_dir.join(format!("block_{:04}", block_num));

        let meta: BlockMeta = serde_json::from_str(
            &fs::read_to_string(block_dir.join("commit.json"))
                .expect("failed to read commit.json"),
        )
        .expect("failed to parse commit.json");

        let validate_start = Instant::now();

        match (args.speed, args.mode) {
            (Speed::Slow, Mode::Verify) => {
                let proof_bytes = fs::read(block_dir.join("proof.bin"))
                    .expect("failed to read proof.bin");
                let proof: SP1ProofWithPublicValues = bincode::deserialize(&proof_bytes)
                    .expect("failed to deserialize proof");

                let (client, pk) = verifier.as_ref().unwrap();
                client
                    .verify(&proof, pk.verifying_key(), None)
                    .unwrap_or_else(|e| panic!("VERIFY FAILED block {block_num}: {e}"));

                let commit: BlockCommit = proof.public_values.clone().read();
                if let Some(prev) = chained_root {
                    if commit.pre_state_root != prev {
                        panic!(
                            "block {block_num}: proof pre_state_root {} does not chain from prev post {}",
                            hex::encode(commit.pre_state_root),
                            hex::encode(prev),
                        );
                    }
                }
                chained_root = Some(commit.post_state_root);
            }
            _ => {
                let txs_bytes = fs::read(block_dir.join("transactions.bin"))
                    .expect("failed to read transactions.bin");
                let txs: Vec<Tx> = bincode::deserialize(&txs_bytes)
                    .expect("failed to deserialize transactions");

                let s = state.as_mut().expect("state must be init for non-slow-verify");
                apply_block(s, &txs);

                if slow_reexec && args.slow_delay_ms > 0 {
                    thread::sleep(Duration::from_millis(args.slow_delay_ms));
                }
            }
        }

        let validate_elapsed = validate_start.elapsed();
        total_validate += validate_elapsed;
        total_txs += meta.txs_total as u64;

        println!(
            "{tag} block {}/{}: validate={:?} ({} txs)",
            block_num,
            manifest.num_blocks - 1,
            validate_elapsed,
            meta.txs_total,
        );
    }

    let wall = run_start.elapsed();
    let throughput = total_txs as f64 / wall.as_secs_f64();

    println!();
    println!("{tag} ========== Summary ==========");
    println!(
        "{tag} blocks={} txs={} validate_total={:?} wall={:?} throughput={:.2} tx/s",
        manifest.num_blocks, total_txs, total_validate, wall, throughput,
    );
}
