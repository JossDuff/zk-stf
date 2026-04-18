use clap::Parser;
use ledger_core::{apply_block, compute_state_root, hash_transactions, BlockCommit, State, Tx};
use sp1_sdk::{
    blocking::{ProveRequest, Prover, ProverClient},
    include_elf, Elf, ProvingKey, SP1Stdin,
};
use std::time::Instant;

const LEDGER_ELF: Elf = include_elf!("ledger-program");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Run native execution only (simulates re-execution mode).
    #[arg(long)]
    execute: bool,

    /// Generate a ZK proof and verify it (simulates leader prove + validator verify).
    #[arg(long)]
    prove: bool,

    /// Number of transactions per block.
    #[arg(long, default_value = "100")]
    num_txs: u32,

    /// Number of accounts in the ledger.
    #[arg(long, default_value = "100")]
    num_accounts: u32,

    /// Starting balance for each account.
    #[arg(long, default_value = "1000")]
    initial_balance: u64,
}

/// Deterministic pseudo-random transaction generation (LCG).
fn generate_txs(num_txs: u32, num_accounts: u32) -> Vec<Tx> {
    let mut txs = Vec::with_capacity(num_txs as usize);
    let mut seed: u64 = 12345;
    for _ in 0..num_txs {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let from = (seed % num_accounts as u64) as u32;
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mut to = (seed % num_accounts as u64) as u32;
        if to == from {
            to = (to + 1) % num_accounts;
        }
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let amount = (seed % 10) + 1;
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

fn main() {
    sp1_sdk::utils::setup_logger();
    dotenv::dotenv().ok();

    let args = Args::parse();

    if args.execute == args.prove {
        eprintln!("Error: You must specify either --execute or --prove");
        std::process::exit(1);
    }

    let state = generate_initial_state(args.num_accounts, args.initial_balance);
    let txs = generate_txs(args.num_txs, args.num_accounts);

    println!("Accounts: {}", args.num_accounts);
    println!("Transactions: {}", args.num_txs);

    // Always run native execution for baseline timing.
    let native_start = Instant::now();
    let pre_root = compute_state_root(&state);
    let mut native_state = state.clone();
    let applied = apply_block(&mut native_state, &txs);
    let native_post_root = compute_state_root(&native_state);
    let native_tx_hash = hash_transactions(&txs);
    let native_elapsed = native_start.elapsed();

    println!("\n--- Native Execution (re-execution baseline) ---");
    println!("Txs applied: {}/{}", applied, args.num_txs);
    println!("Pre-state root:  {}", hex::encode(pre_root));
    println!("Post-state root: {}", hex::encode(native_post_root));
    println!("Execution time:  {:?}", native_elapsed);

    if args.execute {
        // Also run inside the SP1 VM (no proof) to get cycle count.
        let client = ProverClient::from_env();
        let mut stdin = SP1Stdin::new();
        stdin.write(&state);
        stdin.write(&txs);

        println!("\n--- SP1 Execute (cycle count) ---");
        let (mut output, report) = client.execute(LEDGER_ELF, stdin).run().unwrap();
        let commit: BlockCommit = output.read();

        assert_eq!(commit.pre_state_root, pre_root);
        assert_eq!(commit.post_state_root, native_post_root);
        assert_eq!(commit.tx_hash, native_tx_hash);
        println!("Public values match native execution.");
        println!("Cycles: {}", report.total_instruction_count());
    } else {
        // Prove + verify.
        let client = ProverClient::from_env();
        let pk = client.setup(LEDGER_ELF).expect("failed to setup elf");

        let mut stdin = SP1Stdin::new();
        stdin.write(&state);
        stdin.write(&txs);

        println!("\n--- SP1 Prove ---");
        let prove_start = Instant::now();
        let proof = client
            .prove(&pk, stdin)
            .run()
            .expect("failed to generate proof");
        let prove_elapsed = prove_start.elapsed();
        println!("Prove time: {:?}", prove_elapsed);

        // Check public values match native execution.
        let commit: BlockCommit = proof.public_values.clone().read();
        assert_eq!(commit.pre_state_root, pre_root);
        assert_eq!(commit.post_state_root, native_post_root);
        assert_eq!(commit.tx_hash, native_tx_hash);
        println!("Public values match native execution.");

        println!("\n--- SP1 Verify ---");
        let verify_start = Instant::now();
        client
            .verify(&proof, pk.verifying_key(), None)
            .expect("failed to verify proof");
        let verify_elapsed = verify_start.elapsed();
        println!("Verify time: {:?}", verify_elapsed);

        println!("\n--- Summary ---");
        println!("Native execution: {:?}", native_elapsed);
        println!("Prove time:       {:?}", prove_elapsed);
        println!("Verify time:      {:?}", verify_elapsed);
        println!(
            "Prove/Verify ratio: {:.1}x",
            prove_elapsed.as_secs_f64() / verify_elapsed.as_secs_f64()
        );
        println!(
            "Native/Verify ratio: {:.1}x",
            native_elapsed.as_secs_f64() / verify_elapsed.as_secs_f64()
        );
    }
}
