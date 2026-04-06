# ZK-STF: Zero Knowledge State Transition Function

Proves execution of a token ledger (balance transfers) inside SP1's zkVM. Used to benchmark native STF execution time vs ZK proof verification time.

## Prerequisites

- Rust (stable)
- [SP1 toolchain](https://docs.succinct.xyz/docs/getting-started/install) (`cargo prove` must be available)

## Building

Build the SP1 guest program first, then the script:

```bash
cd program && cargo prove build
cd ..
cargo build --release -p ledger-script
```

## Usage

You must specify exactly one of `--execute` or `--prove`.

### Execute mode

Runs the STF natively (baseline timing) and inside the SP1 VM (for cycle count, no proof generated):

```bash
RUST_LOG=info cargo run --release -- --execute --num-txs 500
```

### Prove mode

Runs the STF natively, generates an SP1 proof, then verifies it. Reports all three timings:

```bash
RUST_LOG=info cargo run --release -- --prove --num-txs 500
```

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `--num-txs` | 100 | Number of transactions in the block |
| `--num-accounts` | 100 | Number of accounts in the ledger |
| `--initial-balance` | 1000 | Starting balance for each account |

### Verification key

Extract the program's verification key (needed by validators):

```bash
cargo run --release --bin vkey
```

## Benchmarking

Sweep `--num-txs` to find the point where native execution time exceeds proof verification time:

```bash
for n in 100 500 1000 5000; do
    echo "=== $n txs ==="
    RUST_LOG=warn cargo run --release -- --prove --num-txs $n
    echo
done
```

Key output to look for in `--prove` mode:

- **Native execution** — time a re-executing validator would spend
- **Prove time** — time the leader spends generating the proof
- **Verify time** — time a ZK-verifying validator would spend
- **Native/Verify ratio** — if >1, ZK verification is faster than re-execution
