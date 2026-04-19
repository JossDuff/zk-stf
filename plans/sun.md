# sun.sh v2 plan

Modifies the existing `sun.sh` (originally for a DHT assignment) to orchestrate
`consensus-node` across N sunlab machines. See `plans/consensus.md` for the
consensus binary itself.

## Keep unchanged
- `ALL_NODES`, color constants, `USERNAME`, `DOMAIN`, `PORT` (1895), `TARGET_DIR`
- `probe_node`, `get_sorted_nodes`, `select_nodes`, `cmd_list` — the node-discovery core is solid
- Parallel-SSH + per-node log file + Ctrl-C cleanup trap pattern

## Cut (DHT-specific)
- `cmd_exec` subcommand (unused here)
- `-k`, `-r`, `-R`, `-s`, `-d`, `--kill` flags and their plumbing
- Dual `dht` + `dht-client` per node (consensus is one process per node)
- `get_connections` (replaced by a `--peers host,host,host` builder)
- Local pre-build at `cmd_run` start — each node builds its own /scratch target

## Change
- `-v` flipped from "debug log level" to **proof-verification mode** (consensus-node `--mode verify`)
- `cleanup` kills `consensus-node` instead of `dht`/`dht-client`

## New flags on `run`
- `-n <num>` — total nodes (already present)
- `-ns <num>` — number of slow nodes (must satisfy `0 ≤ ns ≤ n`)
- `-v` — global proof-verify mode (default: re-execution)
- `-w <name>` / `--workload <name>` — workload dir under `workloads/` (e.g. `one_million`)
- `--slow-delay-ms <ms>` — extra sleep in slow nodes' re-execute branch (per-block; default 500)

## New `run` flow
1. **Validate args** — n, ns ≤ n, local `workloads/<name>/` exists and contains `ledger-program.elf`
2. **One-shot source rsync** → `sunlab.cse.lehigh.edu:cse476/zk-stf/` (NFS home, shared across selected nodes; excludes `target/`, `workloads/`). Same shape as `deploy-remote.sh`.
3. **Select N nodes** via existing `select_nodes`
4. **Per-node prep, parallel SSH** (one background per node):
   - Rsync `workloads/<name>/` → `/scratch/workloads/<name>/`
   - **Bootstrap rust 1.91** to `/scratch/.rustup/$USERNAME/` + `/scratch/.cargo/$USERNAME/` if `rustc --version` doesn't report 1.91 (idempotent; fast when already installed)
   - **Bootstrap protoc** via existing `install-protoc.sh` on NFS home if `/scratch/protoc/bin/protoc` missing
   - `PROTOC=/scratch/protoc/bin/protoc cargo build --release -p consensus-node --target-dir /scratch/.cargo/$USERNAME/target`
5. **Per-node command** built from selected list:
   ```
   consensus-node \
     --node-id <i> --speed <fast|slow> \
     --peers <other,other,other> \
     --port 1895 --mode <reexecute|verify> \
     --workload <name> --workloads-dir /scratch/workloads \
     [--slow-delay-ms 500]
   ```
   - Node IDs: `0..n-1`, assigned in `select_nodes` output order
   - Slow set: IDs `0..ns-1`; fast set: IDs `ns..n-1` (round-robin leader means slow nodes lead first)
   - `--peers`: the other `n-1` hostnames (bare name, e.g. `orcus`); node discovers peer IDs via handshake
6. **SSH launch** each node in background, stream to `logs/<node>.log`
7. **Wait + Ctrl-C trap** — `pkill consensus-node` on each selected node

## Decisions (push back if wrong)
- **Binary + crate**: `consensus-node` at `node/consensus-node/`
- **Slow/fast by node-id**: IDs `0..ns-1` are slow. Flipping to slow-at-end is one line.
- **Rust + protoc install inlined** into `run` prep, idempotent, no separate `setup` command
- **Source rsync every run**: always (rsync no-op when unchanged)
- **Workload rsync per-node every run**: always (same reasoning)
- **Dropping `cmd_exec`**: easy to restore if needed
