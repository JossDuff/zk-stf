//! Submit a ledger proof to the Succinct Prover Network.
//!
//! Requires NETWORK_PRIVATE_KEY env var (or in .env file) set to your
//! requester account's Secp256k1 private key.
//!
//! Usage:
//!   RUST_LOG=info cargo run --release --bin network -- --num-txs 500

use clap::Parser;
use ledger_lib::{apply_block, compute_state_root, hash_transactions, BlockCommit, State, Tx};
use sp1_sdk::{
    include_elf, network::NetworkMode, Elf, ProveRequest, Prover, ProverClient, ProvingKey,
    SP1Stdin,
};
use std::time::Instant;

const LEDGER_ELF: Elf = include_elf!("ledger-program");

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Number of transactions per block.
    #[arg(long, default_value = "100")]
    num_txs: u32,

    /// Number of accounts in the ledger.
    #[arg(long, default_value = "1000")]
    num_accounts: u32,

    /// Starting balance for each account.
    #[arg(long, default_value = "1000")]
    initial_balance: u64,
}

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

#[tokio::main]
async fn main() {
    sp1_sdk::utils::setup_logger();
    dotenv::dotenv().ok();

    let args = Args::parse();

    let state = generate_initial_state(args.num_accounts, args.initial_balance);
    let txs = generate_txs(args.num_txs, args.num_accounts);

    println!("Accounts: {}", args.num_accounts);
    println!("Transactions: {}", args.num_txs);

    // Native execution for baseline timing.
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

    // Build inputs for the prover.
    let mut stdin = SP1Stdin::new();
    stdin.write(&state);
    stdin.write(&txs);

    // Connect to the Succinct Prover Network.
    let client = ProverClient::builder()
        .network_for(NetworkMode::Mainnet)
        .build()
        .await;

    let pk = client.setup(LEDGER_ELF).await.unwrap();

    println!("\n--- Network Prove ---");
    println!("Submitting compressed proof to Succinct Prover Network...");
    let prove_start = Instant::now();
    let proof = client
        .prove(&pk, stdin)
        .compressed()
        .await
        .expect("failed to generate proof on network");
    let prove_elapsed = prove_start.elapsed();
    println!("Network prove time: {:?}", prove_elapsed);

    // Verify public values match native execution.
    let commit: BlockCommit = proof.public_values.clone().read();
    assert_eq!(commit.pre_state_root, pre_root);
    assert_eq!(commit.post_state_root, native_post_root);
    assert_eq!(commit.tx_hash, native_tx_hash);
    println!("Public values match native execution.");

    println!("\n--- Verify ---");
    let verify_start = Instant::now();
    client
        .verify(&proof, pk.verifying_key(), None)
        .expect("failed to verify proof");
    let verify_elapsed = verify_start.elapsed();
    println!("Verify time: {:?}", verify_elapsed);

    println!("\n--- Summary ---");
    println!("Native execution:   {:?}", native_elapsed);
    println!("Network prove time: {:?}", prove_elapsed);
    println!("Verify time:        {:?}", verify_elapsed);
    println!(
        "Native/Verify ratio: {:.1}x",
        native_elapsed.as_secs_f64() / verify_elapsed.as_secs_f64()
    );
}
