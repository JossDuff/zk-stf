# Consensus plan

A minimal HotStuff-inspired BFT consensus that demonstrates the re-execution
vs. zk-verification throughput gap on 4 sunlab machines. Deployed by
`sun.sh` (see `plans/sun.md`).

## Framing
- n = 4, f = 1, quorum = 2f+1 = 3
- Round-robin leader, always live, no view change, no timeouts
- Not testing safety under Byzantine behavior — testing sustained tx/sec when 2
  of 4 nodes are slow at block validation
- "BFT" label comes from n=3f+1 + quorum=2f+1; no signatures (trusted TCP, node IDs in messages)

## Binary layout
- New crate: `node/consensus-node/` (parallels `node/run-workload/`)
- One binary, no build script
- Deps: `ledger-core`, `sp1-sdk` (blocking features for `verify`), `tokio`, `bincode`, `serde`, `clap`, `sha2`, `hex`

## CLI (what sun.sh invokes)
```
consensus-node \
  --node-id <N>                 # unique, 0..n-1
  --speed <fast|slow>
  --peers <host,host,host>      # bare hostnames, the OTHER n-1 nodes
  --port 1895
  --mode <reexecute|verify>     # global; all nodes agree
  --workload <name>
  --workloads-dir /scratch/workloads
  [--slow-delay-ms 500]         # per-block; only used in slow+reexecute
```
Peer IDs are discovered via handshake (each side sends its `--node-id` once on connect), so `--peers` is just a name list and order doesn't matter.

## Workload consumption
- Read `/scratch/workloads/<name>/` — same layout `run-workload` uses
- Enumerate blocks in order: N blocks = N consensus rounds, then exit
- Shared I/O helpers (see "Shared plumbing" below) so `run-workload` and `consensus-node` use the same loaders

## Per-node state (across rounds within a workload)
| Variant | Full `State` | `state_root` | Validate step |
|---|---|---|---|
| fast, reexecute mode | yes | derived | `apply_block` |
| fast, verify mode | yes | derived | `apply_block` (fast nodes always re-execute) |
| slow, reexecute mode | yes | derived | `apply_block` + `sleep(slow_delay_ms)` |
| slow, verify mode | **no** | yes, chained from `BlockCommit.post_state_root` | `client.verify(&proof, &vk)` |

Other per-node fields: `round: u64`, `vk: Option<SP1VerifyingKey>` (one-time `client.setup(elf)` for slow+verify), `peer_conns: HashMap<NodeId, Connection>`, vote tallies per `(round, block_hash)`.

## BlockCommit enables root chaining without State
`zkvm/program/src/main.rs` commits `BlockCommit { pre_state_root, post_state_root, tx_hash }` as public values. Slow-verify nodes:
1. Call `client.verify(&proof, &vk)` — checks the proof is valid
2. Deserialize public values → `BlockCommit`
3. Check `commit.pre_state_root == self.state_root` (chain continuity)
4. Check `commit.tx_hash` matches the leader's proposed `block_hash` (consistency with propose)
5. Advance `self.state_root = commit.post_state_root`

No `State` struct needed on slow-verify nodes.

## Message types
```rust
#[derive(Serialize, Deserialize)]
enum Msg {
    Hello  { node_id: u32 },                                           // once per connection
    Propose { round: u64, block_num: u64, block_hash: [u8; 32] },
    Vote    { round: u64, block_num: u64, block_hash: [u8; 32], voter_id: u32 },
}
```
Votes are broadcast to all peers (not just leader). Every node collects its own quorum and commits independently.

`block_hash` is `sha256` of the serialized block file on disk (what all nodes have locally under `workloads/<name>/`). Keeps the propose message tiny while committing validators to the exact content.

## Main loop per round
```
leader_id = round % n
if self.node_id == leader_id:
    block_hash = sha256(local block file for round)
    broadcast Propose { round, block_num: round, block_hash }

await Propose for current round
validate(block_num, block_hash)          // dispatches on (speed, mode) per table above
broadcast Vote { round, block_num, block_hash, voter_id: self.node_id }

# Concurrently receiving votes; commit when 3 matching votes seen (including self)
when votes[round, block_hash].len() >= 3:
    commit (update state or state_root, log timestamps, bump round)
```

Leader validates its own block (same dispatch table as all other nodes). The leader *role* is just select+hash+broadcast; every node including the leader also plays the validator role.

## Networking
- tokio runtime, single-threaded logic via tokio channels
- Each node: bind TCP listener on `--port` at startup, then dial all peers with retry+backoff (peers come up in parallel)
- Per connection: length-prefixed bincode frames
- Handshake: first frame is `Msg::Hello { node_id }` both directions → build `peer_conns: HashMap<NodeId, _>`
- Broadcast = iterate `peer_conns`, write; self-vote is delivered in-process (no loopback)

## Logging (per node → `logs/<hostname>.log`)
JSON-per-line for easy post-run aggregation:
```json
{"round":0,"phase":"propose_recv","ts_ns":...}
{"round":0,"phase":"validate_start","ts_ns":...}
{"round":0,"phase":"validate_end","ts_ns":...,"validate_kind":"reexec"|"verify"}
{"round":0,"phase":"vote_sent","ts_ns":...}
{"round":0,"phase":"quorum_reached","ts_ns":...}
{"round":0,"phase":"committed","ts_ns":...,"state_root":"<hex>","tx_count":1000000}
```

Post-run (offline): `throughput_tx_per_s = sum(tx_count) / (last_committed_ts - first_propose_recv_ts)`. Per-node validate-duration distributions fall out of `validate_end - validate_start`.

## Termination
After committing the final block in the workload:
1. Flush log
2. Keep serving for a 2s grace period so late peers reach their own quorum
3. Close connections, exit 0

## Shared plumbing with run-workload
Extract into a small `workload-io` helper (new crate in `crates/` or a module in `ledger-core`):
- `load_block(dir, num) -> Vec<Tx>`
- `load_proof(dir, num) -> SP1ProofWithPublicValues`
- `block_file_hash(dir, num) -> [u8; 32]`
- `decode_commit(&proof) -> BlockCommit`

Refactor `run-workload` to use them too — no behavior change, just dedup.

## Decisions locked in
- **Slow leader validates its own block** (confirmed): same dispatch table as any other node
- **Fast nodes always re-execute**, in both modes — only slow nodes change behavior with `-v`
- **Slow + verify nodes hold only `state_root`** (no `State`) — chained from `BlockCommit.post_state_root`
- **Votes carry only `(round, block_num, block_hash, voter_id)`** — no state root in the vote (safety not under test)
- **Quorum = 3 matching (round, block_num, block_hash)**, self-vote counts
- **`--slow-delay-ms` applied per-block** after `apply_block`
- **No signatures** — trusted TCP within the cluster
- **No view change, no timeouts, no mempool, no tx gossip** — leader always live, blocks pre-materialized

## Experiment narrative for writeup
"Simplified HotStuff-inspired quorum protocol (n=4, f=1). Slow nodes simulate
resource-constrained participants via per-block sleep in the re-execute branch.
Under re-execution mode, the 3-vote quorum requires at least one slow node's
vote per round, bottlenecking throughput on slow validation. Under verify mode,
slow nodes substitute `client.verify(proof)` for `apply_block`, decoupling
validator cost from block size. Comparison: tx/sec across workload sizes under
both modes, same 4-node assignment."
