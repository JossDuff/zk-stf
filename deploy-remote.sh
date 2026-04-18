#!/usr/bin/env bash
#
# Deploy the remote-runnable parts of zk-stf to Lehigh sunlab.
#
# - repo sources (Cargo.toml, Cargo.lock, crates/, node/, zkvm/) -> sunlab.cse.lehigh.edu:~/cse476/zk-stf/  (NFS home)
# - workloads/ and the prebuilt ELF -> $SUNLAB_MACHINE_NAME.cse.lehigh.edu:/scratch/
#
# The whole repo ships because we're a single cargo workspace — cargo needs every
# member's Cargo.toml to resolve, even though the remote only compiles run-workload.
#
# Usage:
#   SUNLAB_MACHINE_NAME=sunlab01 ./deploy-remote.sh

set -euo pipefail

: "${SUNLAB_MACHINE_NAME:?SUNLAB_MACHINE_NAME must be set (e.g. SUNLAB_MACHINE_NAME=sunlab01)}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

HOME_DEST="jod323@sunlab.cse.lehigh.edu:cse476/zk-stf/"
SCRATCH_DEST="jod323@${SUNLAB_MACHINE_NAME}.cse.lehigh.edu:/scratch/"

ELF="zkvm/target/elf-compilation/riscv64im-succinct-zkvm-elf/release/ledger-program"
WORKLOADS="zkvm/workloads"

if [[ ! -f "$ELF" ]]; then
    echo "ELF not found at $ELF" >&2
    echo "Build it first: (cd zkvm && cargo build --release -p ledger-script)" >&2
    exit 1
fi

if [[ ! -d "$WORKLOADS" ]]; then
    echo "Workloads dir not found at $WORKLOADS" >&2
    exit 1
fi

RSYNC_OPTS=(-avz --human-readable)

echo "==> Syncing workspace sources to $HOME_DEST"
ssh jod323@sunlab.cse.lehigh.edu "mkdir -p cse476/zk-stf"
rsync "${RSYNC_OPTS[@]}" --exclude='target/' --exclude='workloads/' \
    Cargo.toml Cargo.lock crates node zkvm "$HOME_DEST"

echo
echo "==> Syncing workloads/ and ELF to $SCRATCH_DEST"
rsync "${RSYNC_OPTS[@]}" "$WORKLOADS" "$SCRATCH_DEST"
rsync "${RSYNC_OPTS[@]}" "$ELF" "${SCRATCH_DEST}ledger-program.elf"

echo
echo "Done."
echo "On the remote compute node, run:"
echo "  cd ~/cse476/zk-stf && cargo build --release -p run-workload --locked"
echo "  \"\$CARGO_TARGET_DIR/release/run-workload\" --workload-dir /scratch/workloads/one_million --elf-path /scratch/ledger-program.elf"
echo "  # (binary lands under \$CARGO_TARGET_DIR because it's redirected off NFS home)"
