#!/usr/bin/env bash
#
# Deploy the remote-runnable parts of zk-stf to Lehigh sunlab.
#
# - repo sources (Cargo.toml, Cargo.lock, crates/, node/, zkvm/,
#   sun.sh, install-protoc.sh) -> sunlab.cse.lehigh.edu:~/cse476/zk-stf/ (NFS home)
# - workloads/ (each workload dir carries its own ledger-program.elf)
#     -> $SUNLAB_MACHINE_NAME.cse.lehigh.edu:/scratch/
#
# The whole repo ships because we're a single cargo workspace — cargo needs every
# member's Cargo.toml to resolve, even though the remote only compiles
# run-workload / consensus-node.
# sun.sh + install-protoc.sh are included so the consensus run can be driven
# from $SUNLAB_MACHINE_NAME after ssh'ing in.
#
# Usage:
#   SUNLAB_MACHINE_NAME=sunlab01 ./deploy-remote.sh

set -euo pipefail

: "${SUNLAB_MACHINE_NAME:?SUNLAB_MACHINE_NAME must be set (e.g. SUNLAB_MACHINE_NAME=sunlab01)}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_ROOT"

HOME_DEST="jod323@sunlab.cse.lehigh.edu:cse476/zk-stf/"
SCRATCH_DEST="jod323@${SUNLAB_MACHINE_NAME}.cse.lehigh.edu:/scratch/"

WORKLOADS="workloads"

if [[ ! -d "$WORKLOADS" ]]; then
    echo "Workloads dir not found at $WORKLOADS" >&2
    exit 1
fi

# Each workload dir should carry its own ELF next to the proofs.
missing_elf=0
for d in "$WORKLOADS"/*/; do
    if [[ ! -f "${d}ledger-program.elf" ]]; then
        echo "ELF missing: ${d}ledger-program.elf" >&2
        missing_elf=1
    fi
done
if (( missing_elf )); then
    echo "After generating proofs, copy the ELF into the workload dir:" >&2
    echo "  cp zkvm/target/elf-compilation/riscv64im-succinct-zkvm-elf/release/ledger-program \\" >&2
    echo "     <workload-dir>/ledger-program.elf" >&2
    exit 1
fi

RSYNC_OPTS=(-avz --human-readable)

echo "==> Syncing workspace sources to $HOME_DEST"
ssh jod323@sunlab.cse.lehigh.edu "mkdir -p cse476/zk-stf"
rsync "${RSYNC_OPTS[@]}" --exclude='target/' --exclude='workloads/' \
    Cargo.toml Cargo.lock crates node zkvm sun.sh install-protoc.sh "$HOME_DEST"

echo
echo "==> Syncing workloads (with bundled ELF) to $SCRATCH_DEST"
rsync "${RSYNC_OPTS[@]}" "$WORKLOADS" "$SCRATCH_DEST"

echo
echo "Done."
echo "On ${SUNLAB_MACHINE_NAME} (where the workload lives in /scratch/workloads/):"
echo "  ssh jod323@${SUNLAB_MACHINE_NAME}.cse.lehigh.edu"
echo "  cd ~/cse476/zk-stf"
echo
echo "  # single-workload local verify:"
echo "  cargo build --release -p run-workload --locked"
echo "  \"\$CARGO_TARGET_DIR/release/run-workload\" \\"
echo "    --workload-dir /scratch/workloads/one_million \\"
echo "    --elf-path /scratch/workloads/one_million/ledger-program.elf"
echo
echo "  # consensus across N nodes (sun.sh propagates workload from this machine):"
echo "  ./sun.sh list"
echo "  ./sun.sh run -n 4 -ns 2 -w one_million"
echo "  ./sun.sh run -n 4 -ns 2 -v -w one_million   # slow nodes verify proofs"
