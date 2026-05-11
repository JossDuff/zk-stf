//! TCP transport: framing, handshake, dial/listen, broadcast, send_to,
//! timer scheduler.
//!
//! Each consensus node maintains a writer task per peer (fed via mpsc) and a
//! reader task per peer (forwarding into a shared inbox). Pubkeys are
//! exchanged in the `Hello` handshake and stashed in `PeerKeys` for later
//! verification of vote signatures.

use crate::messages::Wire;
use crate::state::TimerEvent;
use ed25519_dalek::VerifyingKey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::time::sleep;

pub type MsgTx = mpsc::UnboundedSender<Wire>;
pub type PeerWriters = Arc<Mutex<HashMap<u32, MsgTx>>>;
pub type PeerKeys = Arc<Mutex<HashMap<u32, VerifyingKey>>>;
pub type InboxTx = mpsc::UnboundedSender<Wire>;

// ─── framing ─────────────────────────────────────────────────────────────────

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

async fn send_msg<W: AsyncWriteExt + Unpin>(w: &mut W, msg: &Wire) -> std::io::Result<()> {
    let bytes = bincode::serialize(msg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_frame(w, &bytes).await
}

async fn recv_msg<R: AsyncReadExt + Unpin>(r: &mut R) -> std::io::Result<Wire> {
    let bytes = read_frame(r).await?;
    bincode::deserialize(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// ─── connection lifecycle ───────────────────────────────────────────────────

async fn handshake(
    stream: &mut TcpStream,
    my_id: u32,
    my_pk: [u8; 32],
) -> std::io::Result<(u32, [u8; 32])> {
    send_msg(
        stream,
        &Wire::Hello {
            node_id: my_id,
            pubkey: my_pk,
        },
    )
    .await?;
    match recv_msg(stream).await? {
        Wire::Hello { node_id, pubkey } => Ok((node_id, pubkey)),
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected Hello, got {other:?}"),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
async fn install_connection(
    mut stream: TcpStream,
    my_id: u32,
    my_pk: [u8; 32],
    peer_writers: PeerWriters,
    peer_keys: PeerKeys,
    inbox: InboxTx,
    tag: String,
) {
    let (peer_id, peer_pk_bytes) = match handshake(&mut stream, my_id, my_pk).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{tag} handshake failed: {e}");
            return;
        }
    };

    let peer_vk = match VerifyingKey::from_bytes(&peer_pk_bytes) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{tag} peer {peer_id} sent invalid pubkey: {e}");
            return;
        }
    };

    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<Wire>();
    // Insert pubkey FIRST. `wait_for_peers` polls `peer_writers.len()`, so if
    // the writer is published before the key, the consensus loop can start
    // and reject this peer's first message during QC verification.
    peer_keys.lock().await.insert(peer_id, peer_vk);
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

pub async fn listen_task(
    port: u16,
    my_id: u32,
    my_pk: [u8; 32],
    peer_writers: PeerWriters,
    peer_keys: PeerKeys,
    inbox: InboxTx,
) {
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
                    my_pk,
                    peer_writers.clone(),
                    peer_keys.clone(),
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

pub async fn dial_task(
    host: String,
    port: u16,
    my_id: u32,
    my_pk: [u8; 32],
    peer_writers: PeerWriters,
    peer_keys: PeerKeys,
    inbox: InboxTx,
) {
    let mut backoff_ms: u64 = 200;
    loop {
        match TcpStream::connect((host.as_str(), port)).await {
            Ok(stream) => {
                install_connection(
                    stream,
                    my_id,
                    my_pk,
                    peer_writers,
                    peer_keys,
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

// ─── broadcast / send_to / timer ────────────────────────────────────────────

/// Send `msg` to every connected peer. Does NOT self-deliver — callers update
/// their own state synchronously.
pub fn broadcast(msg: Wire, peers: &PeerWriters) {
    let peers = peers.clone();
    tokio::spawn(async move {
        let guard = peers.lock().await;
        for tx in guard.values() {
            let _ = tx.send(msg.clone());
        }
    });
}

/// Send `msg` to a single peer (used for NEW-VIEW to the next leader).
/// Silently no-op if the peer isn't connected.
pub fn send_to(peer_id: u32, msg: Wire, peers: &PeerWriters) {
    let peers = peers.clone();
    tokio::spawn(async move {
        let guard = peers.lock().await;
        if let Some(tx) = guard.get(&peer_id) {
            let _ = tx.send(msg);
        }
    });
}

/// Schedule `evt` to fire after `ms` milliseconds. Stale events (different
/// view than current) are filtered when consumed.
pub fn arm_timer(evt: TimerEvent, ms: u64, sender: mpsc::UnboundedSender<TimerEvent>) {
    tokio::spawn(async move {
        sleep(Duration::from_millis(ms)).await;
        let _ = sender.send(evt);
    });
}
