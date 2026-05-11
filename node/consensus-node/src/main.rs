//! consensus-node: canonical Basic HotStuff for the zk-stf experiment.
//!
//! Implements Algorithm 2 from Yin/Malkhi/Reiter/Golan-Gueta/Abraham,
//! "HotStuff: BFT Consensus with Linearity and Responsiveness," PODC 2019.
//!
//! Two block-validation modes via `--mode`:
//!   - `reexecute`: apply_block on a clone of state (slow + reexecute also
//!     sleeps `slow_delay_per_tx_ns * txs_total` to model weak hardware).
//!   - `verify`: SP1 proof verify using the workload's per-block proof.bin.
//!
//! Validation gates the PREPARE vote per the experiment design — a slow node
//! that can't validate before the per-view NEXTVIEW timer fires misses the
//! prepareQC and forces a view change. The Pacemaker uses paper-faithful
//! exponential timeout backoff (Section 6): timeout doubles on each NEXTVIEW,
//! resets to `--timeout-base-ms` on each successful decision.

mod consensus;
mod logging;
mod messages;
mod network;
mod state;
mod validator;
mod workload;

use clap::{Parser, ValueEnum};
use consensus::Wiring;
use ed25519_dalek::SigningKey;
use ledger_core::State;
use messages::Wire;
use network::{dial_task, listen_task, PeerKeys, PeerWriters};
use rand::rngs::OsRng;
use sp1_sdk::{
    blocking::{Prover, ProverClient},
    Elf, ProvingKey,
};
use state::{ReplicaState, TimerEvent};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;
use validator::{run_validation, ValidateOutcome, ValidateRequest, Validator};
use workload::Workload;

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
    /// This replica's id in [0, n). Determines round-robin leader rotation:
    /// view v is led by node (v - 1) mod n.
    #[arg(long)]
    node_id: u32,

    /// `fast` for full-speed validation; `slow` adds `slow_delay_per_tx_ns *
    /// txs_total` of sleep during re-execute validation to model weaker hardware.
    #[arg(long, value_enum)]
    speed: Speed,

    /// Comma-separated peer hostnames (the OTHER n-1 nodes).
    #[arg(long)]
    peers: String,

    #[arg(long, default_value_t = 1895)]
    port: u16,

    /// Block validation strategy: `reexecute` applies the txs locally,
    /// `verify` checks the SP1 proof in the block's proof.bin.
    #[arg(long, value_enum)]
    mode: Mode,

    /// Workload name; resolves to `<workloads_dir>/<workload>/`.
    #[arg(long)]
    workload: String,

    /// Directory containing one subdirectory per workload (each with
    /// `manifest.json`, `ledger-program.elf`, and `block_NNNN/...`).
    #[arg(long)]
    workloads_dir: PathBuf,

    /// Per-tx sleep in ns for slow + reexecute mode. Total per-block sleep =
    /// this * txs_total, modeling weaker hardware that scales with STF size.
    #[arg(long, default_value_t = 0)]
    slow_delay_per_tx_ns: u64,

    /// Initial NEXTVIEW timeout (ms). Per paper Section 6, the Pacemaker
    /// doubles this on each unsuccessful view; we reset to base on each
    /// successful decision.
    #[arg(long, default_value_t = 1000)]
    timeout_base_ms: u64,
}

#[tokio::main]
async fn main() {
    sp1_sdk::utils::setup_logger();
    let args = Args::parse();
    let tag = format!("[node {}]", args.node_id);

    let workload = Workload::load(args.workloads_dir.join(&args.workload));
    let elf_path = workload.elf_path();

    let peer_hosts: Vec<String> = args
        .peers
        .split(',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let n: u32 = peer_hosts.len() as u32 + 1;
    let f: u32 = (n - 1) / 3;
    let quorum: u32 = n - f;

    eprintln!(
        "{tag} speed={:?} mode={:?} workload={} port={} n={} f={} quorum={}",
        args.speed, args.mode, args.workload, args.port, n, f, quorum
    );
    eprintln!(
        "{tag} timeout_base_ms={} (paper-faithful exponential backoff)",
        args.timeout_base_ms
    );
    eprintln!(
        "{tag} workload: {} blocks, {} accounts, initial_balance={}",
        workload.manifest.num_blocks,
        workload.manifest.num_accounts,
        workload.manifest.initial_balance,
    );

    // Crypto setup.
    let signing_key = SigningKey::generate(&mut OsRng);
    let my_pk: [u8; 32] = signing_key.verifying_key().to_bytes();

    // Validator + initial state.
    let validator = build_validator(&args, &elf_path, &tag).await;
    let mut live_state = init_state(&args, &workload);
    let mut chained_root: [u8; 32] = [0u8; 32];

    // Networking bootstrap.
    let peer_writers: PeerWriters = Arc::new(Mutex::new(HashMap::new()));
    let peer_keys: PeerKeys = Arc::new(Mutex::new(HashMap::new()));
    // Insert our own pubkey so QC verification can check signatures we
    // contributed to (e.g. a prepareQC piggybacked on a peer's later NEW-VIEW
    // includes our partial sig from when we voted PREPARE).
    peer_keys
        .lock()
        .await
        .insert(args.node_id, signing_key.verifying_key());
    let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel::<Wire>();

    tokio::spawn(listen_task(
        args.port,
        args.node_id,
        my_pk,
        peer_writers.clone(),
        peer_keys.clone(),
        inbox_tx.clone(),
    ));
    for host in peer_hosts.iter().cloned() {
        tokio::spawn(dial_task(
            host,
            args.port,
            args.node_id,
            my_pk,
            peer_writers.clone(),
            peer_keys.clone(),
            inbox_tx.clone(),
        ));
    }
    wait_for_peers(&peer_writers, n, &tag).await;

    // Validator worker (serial — at most one validation in flight).
    let (val_req_tx, mut val_out_rx) = spawn_validator_worker(validator.clone());

    // Timer channel.
    let (timer_tx, mut timer_rx) = mpsc::unbounded_channel::<TimerEvent>();

    let mut rs = ReplicaState::new(n, args.timeout_base_ms);

    let run_start = Instant::now();
    let mut total_txs: u64 = 0;
    let mut last_counted_height: i64 = -1;

    // Kick off view 1.
    {
        let w = wiring(
            &args,
            &signing_key,
            &peer_writers,
            &timer_tx,
            &val_req_tx,
            &workload,
        );
        consensus::start_protocol(&mut rs, &w);
        consensus::try_advance(&mut rs, &w, &mut live_state, &mut chained_root);
    }

    // Main event loop.
    loop {
        if rs.last_executed_height + 1 >= workload.manifest.num_blocks as i64 {
            break;
        }

        tokio::select! {
            Some(msg) = inbox_rx.recv() => {
                let w = wiring(&args, &signing_key, &peer_writers, &timer_tx, &val_req_tx, &workload);
                consensus::handle_msg(&mut rs, msg, &peer_keys, &w, &live_state).await;
                consensus::try_advance(&mut rs, &w, &mut live_state, &mut chained_root);
            }
            Some(evt) = timer_rx.recv() => {
                let w = wiring(&args, &signing_key, &peer_writers, &timer_tx, &val_req_tx, &workload);
                consensus::handle_timer(&mut rs, evt, &w);
                consensus::try_advance(&mut rs, &w, &mut live_state, &mut chained_root);
            }
            Some(out) = val_out_rx.recv() => {
                logging::log_event(
                    args.node_id,
                    rs.view,
                    "validate_end",
                    serde_json::json!({
                        "kind": validator.kind(),
                        "valid": out.valid,
                        "height": out.height,
                        "elapsed_ns": out.elapsed.as_nanos() as u64,
                    }),
                );
                consensus::ingest_validation(&mut rs, out);
                let w = wiring(&args, &signing_key, &peer_writers, &timer_tx, &val_req_tx, &workload);
                consensus::try_advance(&mut rs, &w, &mut live_state, &mut chained_root);
            }
            else => break,
        }

        // Sum tx counts for any heights executed since the last tick.
        while last_counted_height < rs.last_executed_height {
            let h = (last_counted_height + 1) as u64;
            let block = workload.block(h);
            total_txs += block.meta.txs_total as u64;
            last_counted_height = h as i64;
        }
    }

    let wall = run_start.elapsed();
    let throughput = total_txs as f64 / wall.as_secs_f64();
    eprintln!(
        "{tag} ===== summary: blocks={} txs={} wall={:?} throughput={:.2} tx/s",
        workload.manifest.num_blocks, total_txs, wall, throughput
    );

    eprintln!("{tag} grace 2s then exit");
    sleep(Duration::from_secs(2)).await;
}

// ─── setup helpers ──────────────────────────────────────────────────────────

async fn build_validator(args: &Args, elf_path: &Path, tag: &str) -> Validator {
    match args.mode {
        Mode::Verify => {
            eprintln!("{tag} loading elf + prover setup...");
            let ep = elf_path.to_path_buf();
            let (client, vkey) = tokio::task::spawn_blocking(move || {
                let elf_bytes = fs::read(&ep)
                    .unwrap_or_else(|e| panic!("failed to read elf at {ep:?}: {e}"));
                let elf: Elf = elf_bytes.into();
                let client = ProverClient::from_env();
                let pk = client.setup(elf).expect("failed to setup elf");
                let vkey = pk.verifying_key().clone();
                (client, vkey)
            })
            .await
            .expect("prover setup panicked");
            eprintln!("{tag} prover setup done");
            Validator::Verify {
                client: Arc::new(client),
                vkey: Arc::new(vkey),
            }
        }
        Mode::Reexecute => {
            let slow_delay = if matches!(args.speed, Speed::Slow) {
                args.slow_delay_per_tx_ns
            } else {
                0
            };
            Validator::Reexecute {
                slow_delay_per_tx_ns: slow_delay,
            }
        }
    }
}

fn init_state(args: &Args, workload: &Workload) -> State {
    if matches!(args.mode, Mode::Reexecute) {
        let mut s = State::new();
        for i in 0..workload.manifest.num_accounts {
            s.set_balance(i, workload.manifest.initial_balance);
        }
        s
    } else {
        State::new()
    }
}

async fn wait_for_peers(peer_writers: &PeerWriters, n: u32, tag: &str) {
    eprintln!("{tag} waiting for {} peers...", n - 1);
    loop {
        let count = peer_writers.lock().await.len() as u32;
        if count >= n - 1 {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    eprintln!("{tag} all peers connected");
}

fn spawn_validator_worker(
    validator: Validator,
) -> (
    mpsc::UnboundedSender<(ValidateRequest, State)>,
    mpsc::UnboundedReceiver<ValidateOutcome>,
) {
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<(ValidateRequest, State)>();
    let (out_tx, out_rx) = mpsc::unbounded_channel::<ValidateOutcome>();
    tokio::spawn(async move {
        while let Some((req, snapshot)) = req_rx.recv().await {
            let v = validator.clone();
            let out_tx = out_tx.clone();
            let outcome = tokio::task::spawn_blocking(move || run_validation(&req, snapshot, &v))
                .await
                .expect("validation task panicked");
            let _ = out_tx.send(outcome);
        }
    });
    (req_tx, out_rx)
}

#[allow(clippy::too_many_arguments)]
fn wiring<'a>(
    args: &Args,
    sk: &'a SigningKey,
    peers: &'a PeerWriters,
    timer_tx: &'a mpsc::UnboundedSender<TimerEvent>,
    val_req_tx: &'a mpsc::UnboundedSender<(ValidateRequest, State)>,
    workload: &'a Workload,
) -> Wiring<'a> {
    Wiring {
        my_id: args.node_id,
        sk,
        peers,
        timer_tx,
        val_req_tx,
        workload,
        timeout_base_ms: args.timeout_base_ms,
    }
}
