//! Workload + per-block file layout.
//!
//! Every node has the same workload directory locally at experiment start:
//!
//! ```text
//! <workload>/
//!   manifest.json
//!   ledger-program.elf            (used only by --mode verify)
//!   block_0000/
//!     transactions.bin
//!     proof.bin
//!     commit.json                 (carries tx_hash, pre/post state roots)
//!   block_0001/...
//! ```
//!
//! The block identifier consensus uses (the `cmd` field in a HotStuff Node) is
//! the workload-generator's `tx_hash` from `commit.json`. We do NOT re-hash
//! `transactions.bin` at runtime — that file is only read inside the validator
//! worker when actually needed.

use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
pub struct Manifest {
    pub num_accounts: u32,
    pub initial_balance: u64,
    #[allow(dead_code)]
    pub num_txs_per_block: u32,
    pub num_blocks: u32,
}

#[derive(Deserialize)]
pub struct BlockMeta {
    #[allow(dead_code)]
    pub block_number: u32,
    #[allow(dead_code)]
    pub pre_state_root: String,
    #[allow(dead_code)]
    pub post_state_root: String,
    pub tx_hash: String,
    #[allow(dead_code)]
    pub txs_applied: u32,
    pub txs_total: u32,
}

pub struct BlockRef {
    /// The block's content identifier — used as the `cmd` of HotStuff nodes.
    pub cmd: [u8; 32],
    pub meta: BlockMeta,
    pub txs_path: PathBuf,
    pub proof_path: PathBuf,
}

pub struct Workload {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl Workload {
    pub fn load(dir: PathBuf) -> Self {
        let manifest: Manifest = serde_json::from_str(
            &fs::read_to_string(dir.join("manifest.json"))
                .expect("failed to read manifest.json"),
        )
        .expect("failed to parse manifest.json");
        Self { dir, manifest }
    }

    pub fn elf_path(&self) -> PathBuf {
        self.dir.join("ledger-program.elf")
    }

    pub fn block(&self, height: u64) -> BlockRef {
        block_at(&self.dir, height)
    }
}

fn block_at(workload_dir: &Path, height: u64) -> BlockRef {
    let block_dir = workload_dir.join(format!("block_{:04}", height));
    let meta: BlockMeta = serde_json::from_str(
        &fs::read_to_string(block_dir.join("commit.json"))
            .expect("failed to read commit.json"),
    )
    .expect("failed to parse commit.json");
    let mut cmd = [0u8; 32];
    hex::decode_to_slice(&meta.tx_hash, &mut cmd).unwrap_or_else(|e| {
        panic!(
            "invalid tx_hash hex {:?} in {:?}: {e}",
            meta.tx_hash, block_dir
        )
    });
    BlockRef {
        cmd,
        meta,
        txs_path: block_dir.join("transactions.bin"),
        proof_path: block_dir.join("proof.bin"),
    }
}
