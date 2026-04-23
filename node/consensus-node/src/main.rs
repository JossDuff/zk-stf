//! consensus-node: simplified HotStuff-inspired BFT consensus for the
//! zk-stf throughput experiment.
//!
//! n = 4, f = 1, quorum = 2f+1 = 3 (generalized: quorum = 2*((n-1)/3)+1).
//! Round-robin leader (leader = round % n). No view change, no signatures,
//! no mempool. Blocks are pre-materialized on every node under
//! <workloads_dir>/<workload>/block_NNNN/.
//!
//! Per-round:
//!   1. leader broadcasts Propose { round, block_num, block_hash }
//!   2. every node validates locally (dispatch on (speed, mode))
//!   3. every node broadcasts Vote { round, block_num, block_hash, voter_id }
//!   4. commit once a quorum of matching votes is seen
//!
//! Validation dispatch:
//!   fast + *         → apply_block
//!   slow + reexecute → apply_block + sleep(slow_delay_ms)
//!   slow + verify    → client.verify(proof); chain via BlockCommit

use clap::{Parser, ValueEnum};
use ledger_core::{apply_block, BlockCommit, State, Tx};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sp1_sdk::{
    blocking::{Prover, ProverClient},
    Elf, ProvingKey, SP1ProofWithPublicValues,
};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;

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

    /// Comma-separated peer hostnames (the OTHER n-1 nodes).
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
    #[allow(dead_code)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Msg {
    Hello {
        node_id: u32,
    },
    Propose {
        round: u64,
        block_num: u64,
        block_hash: [u8; 32],
    },
    Vote {
        round: u64,
        block_num: u64,
        block_hash: [u8; 32],
        voter_id: u32,
    },
}

type MsgTx = mpsc::UnboundedSender<Msg>;
type PeerWriters = Arc<Mutex<HashMap<u32, MsgTx>>>;
type InboxTx = mpsc::UnboundedSender<Msg>;

// ─── framing ────────────────────────────────────────────────────────────────

async fn write_frame<W: AsyncWriteExt + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    let len = bytes.len() as u32;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(bytes).await?;
    Ok(())
}

async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn send_msg<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &Msg) -> std::io::Result<()> {
    let bytes = bincode::serialize(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_frame(w, &bytes).await
}

async fn recv_msg<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Msg> {
    let bytes = read_frame(r).await?;
    bincode::deserialize(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// ─── connection lifecycle ───────────────────────────────────────────────────

async fn handshake(stream: &mut TcpStream, my_id: u32) -> std::io::Result<u32> {
    send_msg(stream, &Msg::Hello { node_id: my_id }).await?;
    match recv_msg(stream).await? {
        Msg::Hello { node_id } => Ok(node_id),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected Hello, got {other:?}"),
        )),
    }
}

/// Completes the handshake, then atomically registers the peer's writer. If
/// another connection to the same peer already exists, drops this one.
async fn install_connection(
    mut stream: TcpStream,
    my_id: u32,
    peer_writers: PeerWriters,
    inbox: InboxTx,
    tag: String,
) {
    let peer_id = match handshake(&mut stream, my_id).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("{tag} handshake failed: {e}");
            return;
        }
    };

    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Msg>();
    {
        let mut guard = peer_writers.lock().await;
        if guard.contains_key(&peer_id) {
            return; // dup; drop this conn
        }
        guard.insert(peer_id, outbound_tx);
    }

    eprintln!("{tag} peer {peer_id} connected");

    let (mut r, mut w) = stream.into_split();

    let inbox_r = inbox.clone();
    let peers_r = peer_writers.clone();
    let tag_r = tag.clone();
    tokio::spawn(async move {
        loop {
            match recv_msg(&mut r).await {
                Ok(msg) => {
                    if inbox_r.send(msg).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("{tag_r} peer {peer_id} read closed: {e}");
                    break;
                }
            }
        }
        peers_r.lock().await.remove(&peer_id);
    });

    tokio::spawn(async move {
        while let Some(msg) = outbound_rx.recv().await {
            if let Err(e) = send_msg(&mut w, &msg).await {
                eprintln!("{tag} peer {peer_id} write error: {e}");
                break;
            }
        }
    });
}

async fn listen_task(port: u16, my_id: u32, peer_writers: PeerWriters, inbox: InboxTx) {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("failed to bind 0.0.0.0:{port}: {e}"));
    eprintln!("[node {my_id}] listening on 0.0.0.0:{port}");
    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                tokio::spawn(install_connection(
                    stream,
                    my_id,
                    peer_writers.clone(),
                    inbox.clone(),
                    format!("[accept {addr}]"),
                ));
            }
            Err(e) => {
                eprintln!("accept error: {e}");
                sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn dial_task(host: String, port: u16, my_id: u32, peer_writers: PeerWriters, inbox: InboxTx) {
    let mut backoff_ms: u64 = 200;
    loop {
        match TcpStream::connect((host.as_str(), port)).await {
            Ok(stream) => {
                install_connection(
                    stream,
                    my_id,
                    peer_writers,
                    inbox,
                    format!("[dial {host}]"),
                )
                .await;
                return;
            }
            Err(_) => {
                sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(2000);
            }
        }
    }
}

// ─── mailbox ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Mailbox {
    propose: HashMap<u64, (u64, [u8; 32])>,
    votes: HashMap<(u64, u64, [u8; 32]), HashSet<u32>>,
}

fn file_msg(mb: &mut Mailbox, msg: Msg) {
    match msg {
        Msg::Hello { .. } => {}
        Msg::Propose {
            round,
            block_num,
            block_hash,
        } => {
            mb.propose.insert(round, (block_num, block_hash));
        }
        Msg::Vote {
            round,
            block_num,
            block_hash,
            voter_id,
        } => {
            mb.votes
                .entry((round, block_num, block_hash))
                .or_default()
                .insert(voter_id);
        }
    }
}

async fn drain_until<F>(
    inbox_rx: &mut mpsc::UnboundedReceiver<Msg>,
    mailbox: &mut Mailbox,
    mut cond: F,
) where
    F: FnMut(&Mailbox) -> bool,
{
    while !cond(mailbox) {
        let msg = inbox_rx.recv().await.expect("inbox channel closed");
        file_msg(mailbox, msg);
    }
}

async fn broadcast_and_self(peer_writers: &PeerWriters, mailbox: &mut Mailbox, msg: Msg) {
    {
        let guard = peer_writers.lock().await;
        for tx in guard.values() {
            let _ = tx.send(msg.clone());
        }
    }
    file_msg(mailbox, msg);
}

// ─── logging ────────────────────────────────────────────────────────────────

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock went backwards")
        .as_nanos() as u64
}

fn log_event(node_id: u32, round: u64, phase: &str, extras: serde_json::Value) {
    let mut extra_str = String::new();
    if let serde_json::Value::Object(map) = extras {
        for (k, v) in map {
            extra_str.push(' ');
            extra_str.push_str(&k);
            extra_str.push('=');
            match v {
                serde_json::Value::String(s) => extra_str.push_str(&s),
                other => extra_str.push_str(&other.to_string()),
            }
        }
    }
    println!(
        "[node {node_id}] round={round} {phase} ts_ns={}{extra_str}",
        now_ns()
    );
}

// ─── main ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    sp1_sdk::utils::setup_logger();
    let args = Args::parse();

    let workload_dir = args.workloads_dir.join(&args.workload);
    let elf_path = workload_dir.join("ledger-program.elf");
    let tag = format!("[node {}]", args.node_id);

    let peer_hosts: Vec<String> = args
        .peers
        .split(',')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let n: u32 = peer_hosts.len() as u32 + 1;
    let f: u32 = (n - 1) / 3;
    let quorum: u32 = 2 * f + 1;

    eprintln!(
        "{tag} speed={:?} mode={:?} workload={} port={} n={} f={} quorum={}",
        args.speed, args.mode, args.workload, args.port, n, f, quorum
    );
    eprintln!("{tag} workload_dir={:?}", workload_dir);
    eprintln!("{tag} peers={:?}", peer_hosts);

    let manifest: Manifest = serde_json::from_str(
        &fs::read_to_string(workload_dir.join("manifest.json"))
            .expect("failed to read manifest.json"),
    )
    .expect("failed to parse manifest.json");

    eprintln!(
        "{tag} workload: {} blocks × {} txs, {} accounts, initial_balance={}",
        manifest.num_blocks,
        manifest.num_txs_per_block,
        manifest.num_accounts,
        manifest.initial_balance,
    );

    let slow_verify = matches!((args.speed, args.mode), (Speed::Slow, Mode::Verify));
    let slow_reexec = matches!((args.speed, args.mode), (Speed::Slow, Mode::Reexecute));

    // Slow+verify: set up prover/verifying key once (in blocking thread to
    // avoid nested-runtime panic). Everyone else: init full State from manifest.
    let verifier = if slow_verify {
        eprintln!("{tag} loading elf + prover setup...");
        let ep = elf_path.clone();
        let v = tokio::task::spawn_blocking(move || {
            let elf_bytes = fs::read(&ep)
                .unwrap_or_else(|e| panic!("failed to read elf at {ep:?}: {e}"));
            let elf: Elf = elf_bytes.into();
            let client = ProverClient::from_env();
            let pk = client.setup(elf).expect("failed to setup elf");
            (client, pk)
        })
        .await
        .expect("prover setup panicked");
        eprintln!("{tag} prover setup done");
        Some(Arc::new(v))
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
    let mut chained_root: [u8; 32] = [0u8; 32];

    // ─── networking bootstrap ───────────────────────────────────────────────
    let peer_writers: PeerWriters = Arc::new(Mutex::new(HashMap::new()));
    let (inbox_tx, mut inbox_rx) = mpsc::unbounded_channel::<Msg>();

    tokio::spawn(listen_task(
        args.port,
        args.node_id,
        peer_writers.clone(),
        inbox_tx.clone(),
    ));

    for host in peer_hosts.iter().cloned() {
        tokio::spawn(dial_task(
            host,
            args.port,
            args.node_id,
            peer_writers.clone(),
            inbox_tx.clone(),
        ));
    }

    eprintln!("{tag} waiting for {} peers...", n - 1);
    loop {
        let count = peer_writers.lock().await.len() as u32;
        if count >= n - 1 {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    eprintln!("{tag} all peers connected");

    // ─── consensus loop ─────────────────────────────────────────────────────
    let mut mailbox = Mailbox::default();
    let run_start = Instant::now();
    let mut total_txs: u64 = 0;
    let mut total_validate = Duration::ZERO;

    for round in 0..manifest.num_blocks as u64 {
        let leader_id = round % n as u64;

        let block_dir = workload_dir.join(format!("block_{:04}", round));
        let meta: BlockMeta = serde_json::from_str(
            &fs::read_to_string(block_dir.join("commit.json"))
                .expect("failed to read commit.json"),
        )
        .expect("failed to parse commit.json");

        let txs_file = block_dir.join("transactions.bin");
        let proof_file = block_dir.join("proof.bin");

        // Every node hashes its local block file; leader's Propose should match.
        let local_hash: [u8; 32] = {
            let bytes = fs::read(&txs_file).expect("failed to read transactions.bin");
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            hasher.finalize().into()
        };

        log_event(
            args.node_id,
            round,
            "round_start",
            serde_json::json!({
                "leader_id": leader_id,
                "local_hash": hex::encode(local_hash),
            }),
        );

        // Propose
        if args.node_id as u64 == leader_id {
            log_event(args.node_id, round, "propose_send", serde_json::json!({}));
            broadcast_and_self(
                &peer_writers,
                &mut mailbox,
                Msg::Propose {
                    round,
                    block_num: round,
                    block_hash: local_hash,
                },
            )
            .await;
        }

        // Await propose
        drain_until(&mut inbox_rx, &mut mailbox, |mb| mb.propose.contains_key(&round)).await;
        let (propose_block_num, propose_block_hash) = mailbox.propose[&round];
        log_event(
            args.node_id,
            round,
            "propose_recv",
            serde_json::json!({
                "block_num": propose_block_num,
                "block_hash": hex::encode(propose_block_hash),
            }),
        );

        if propose_block_num != round {
            panic!(
                "round {round}: propose.block_num={} mismatch (expected {round})",
                propose_block_num
            );
        }
        if propose_block_hash != local_hash {
            panic!(
                "round {round}: propose.block_hash {} does not match local {}",
                hex::encode(propose_block_hash),
                hex::encode(local_hash),
            );
        }

        // Validate
        let validate_kind = if slow_verify { "verify" } else { "reexec" };
        log_event(
            args.node_id,
            round,
            "validate_start",
            serde_json::json!({ "kind": validate_kind }),
        );
        let validate_start = Instant::now();

        match (args.speed, args.mode) {
            (Speed::Slow, Mode::Verify) => {
                let proof_bytes = fs::read(&proof_file).expect("failed to read proof.bin");
                let v = verifier.as_ref().unwrap().clone();
                let commit: BlockCommit = tokio::task::spawn_blocking(move || {
                    let proof: SP1ProofWithPublicValues = bincode::deserialize(&proof_bytes)
                        .expect("failed to deserialize proof");
                    v.0.verify(&proof, v.1.verifying_key(), None)
                        .expect("VERIFY FAILED");
                    proof.public_values.clone().read()
                })
                .await
                .expect("verify task panicked");

                if round > 0 && commit.pre_state_root != chained_root {
                    panic!(
                        "round {round}: proof pre_state_root {} does not chain from prev post {}",
                        hex::encode(commit.pre_state_root),
                        hex::encode(chained_root),
                    );
                }
                chained_root = commit.post_state_root;
            }
            _ => {
                let txs_bytes = fs::read(&txs_file).expect("failed to read transactions.bin");
                let txs: Vec<Tx> = bincode::deserialize(&txs_bytes)
                    .expect("failed to deserialize transactions");

                let s = state.as_mut().expect("state must be init for non-slow-verify");
                tokio::task::block_in_place(|| {
                    apply_block(s, &txs);
                });

                if slow_reexec && args.slow_delay_ms > 0 {
                    sleep(Duration::from_millis(args.slow_delay_ms)).await;
                }
            }
        }

        let validate_elapsed = validate_start.elapsed();
        total_validate += validate_elapsed;
        log_event(
            args.node_id,
            round,
            "validate_end",
            serde_json::json!({
                "kind": validate_kind,
                "elapsed_ns": validate_elapsed.as_nanos() as u64,
            }),
        );

        // Vote
        let vote = Msg::Vote {
            round,
            block_num: round,
            block_hash: local_hash,
            voter_id: args.node_id,
        };
        log_event(args.node_id, round, "vote_sent", serde_json::json!({}));
        broadcast_and_self(&peer_writers, &mut mailbox, vote).await;

        // Await quorum
        let vote_key = (round, round, local_hash);
        let needed = quorum as usize;
        drain_until(&mut inbox_rx, &mut mailbox, |mb| {
            mb.votes.get(&vote_key).map_or(0, |s| s.len()) >= needed
        })
        .await;
        log_event(
            args.node_id,
            round,
            "quorum_reached",
            serde_json::json!({
                "votes": mailbox.votes.get(&vote_key).map_or(0, |s| s.len()),
            }),
        );

        total_txs += meta.txs_total as u64;
        log_event(
            args.node_id,
            round,
            "committed",
            serde_json::json!({
                "tx_count": meta.txs_total,
            }),
        );

        // Purge stale mailbox entries for committed rounds.
        mailbox.propose.retain(|r, _| *r > round);
        mailbox.votes.retain(|(r, _, _), _| *r > round);
    }

    let wall = run_start.elapsed();
    let throughput = total_txs as f64 / wall.as_secs_f64();
    eprintln!(
        "{tag} ===== summary: blocks={} txs={} validate_total={:?} wall={:?} throughput={:.2} tx/s",
        manifest.num_blocks, total_txs, total_validate, wall, throughput
    );

    // Grace period: keep serving so late peers can reach quorum.
    eprintln!("{tag} grace 2s then exit");
    sleep(Duration::from_secs(2)).await;
}
