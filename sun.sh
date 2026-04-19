#!/bin/bash
#
# sun.sh - Deploy and run zk-stf consensus-node across the Sunlab cluster.
#
# Two-phase flow for `run`:
#   1. prep (per-node, parallel): rsync workload to /scratch, ensure rust 1.91
#      + protoc on /scratch, build consensus-node into /scratch target dir
#   2. launch (per-node, parallel): exec consensus-node with peer list
#
# See plans/sun.md for design notes.

set -e

USERNAME="jod323"
DOMAIN="cse.lehigh.edu"
LOG_DIR="logs"
PORT="1895"
NFS_REPO="cse476/zk-stf"
CARGO_HOME_REMOTE="/scratch/.cargo/${USERNAME}"
RUSTUP_HOME_REMOTE="/scratch/.rustup/${USERNAME}"
TARGET_DIR="${CARGO_HOME_REMOTE}/target"
PROTOC_BIN_REMOTE="/scratch/protoc/bin/protoc"
RUST_VERSION="1.91.0"

# All known Sunlab compute nodes (login node `sunlab` excluded — it's per-machine /scratch)
ALL_NODES=(
    ariel caliban callisto ceres
    chiron cupid eris europa hydra
    iapetus io ixion mars mercury
    neptune nereid nix orcus phobos puck
    saturn triton varda vesta xena
)

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

usage() {
    cat <<'EOF'
Usage: sun.sh <command> [options]

Commands:
  list                                            List all nodes with current CPU load + port status
  run -n <N> -ns <NS> -w <workload> [-v] [opts]   Deploy and run consensus-node on N least-loaded free-port nodes
  cleanup                                         Kill consensus-node on all nodes

Run options:
  -n <N>                Total nodes (required)
  -ns <NS>              Number of slow nodes (required, 0 <= NS <= N)
  -w, --workload <name> Workload dir under workloads/ (required), e.g. one_million
  -v                    Proof-verify mode (default: re-execution)
  --slow-delay-ms <ms>  Per-block sleep for slow nodes in re-execute mode (default: 500)
  -h, --help            Show this help

Examples:
  sun.sh list
  sun.sh run -n 4 -ns 2 -w one_million
  sun.sh run -n 4 -ns 2 -v -w one_million
  sun.sh run -n 4 -ns 2 -w one_million --slow-delay-ms 1000
  sun.sh cleanup

Node IDs are assigned 0..N-1 in select order. Slow nodes are IDs 0..NS-1.
Round-robin leader = round % N, so slow nodes lead first.

Logs: logs/<node>-prep.log (setup) and logs/<node>.log (runtime)
EOF
    exit 0
}

# ─────────────────────────────────────────────────────────────────────────────
# Node discovery (unchanged from v1)
# ─────────────────────────────────────────────────────────────────────────────

# Probe a single node: returns "load node port_status" on stdout, or nothing on failure.
probe_node() {
    local node=$1
    local host="${node}.${DOMAIN}"

    local result=$(ssh -o StrictHostKeyChecking=no \
        -o ConnectTimeout=3 \
        -o BatchMode=yes \
        "${USERNAME}@${host}" \
        "load=\$(cat /proc/loadavg | cut -d' ' -f1); \
         if ss -tlnp 2>/dev/null | grep -q ':${PORT} ' || netstat -tlnp 2>/dev/null | grep -q ':${PORT} '; then \
           port_status=busy; \
         else \
           port_status=free; \
         fi; \
         echo \"\$load \$port_status\"" 2>/dev/null)

    if [[ -n "$result" ]]; then
        local load=$(echo "$result" | cut -d' ' -f1)
        local port_status=$(echo "$result" | cut -d' ' -f2)
        echo "$load $node $port_status"
    fi
}

get_sorted_nodes() {
    echo -e "${CYAN}Probing ${#ALL_NODES[@]} nodes for CPU load and port ${PORT}...${NC}" >&2

    local tmp_file=$(mktemp)
    for node in "${ALL_NODES[@]}"; do
        probe_node "$node" >>"$tmp_file" &
    done
    wait

    sort -n "$tmp_file"
    rm -f "$tmp_file"
}

cmd_list() {
    echo -e "${GREEN}=== Sunlab Node Status ===${NC}"
    echo ""
    printf "%-12s %-12s %s\n" "NODE" "LOAD (1min)" "PORT ${PORT}"
    printf "%-12s %-12s %s\n" "----" "----------" "--------"

    get_sorted_nodes | while read load node port_status; do
        if [[ "$port_status" == "busy" ]]; then
            color=$RED
            port_display="BUSY"
        elif (($(echo "$load < 1.0" | bc -l))); then
            color=$GREEN
            port_display="free"
        elif (($(echo "$load < 3.0" | bc -l))); then
            color=$YELLOW
            port_display="free"
        else
            color=$RED
            port_display="free"
        fi
        printf "${color}%-12s %-12s %s${NC}\n" "$node" "$load" "$port_display"
    done
    echo ""
}

select_nodes() {
    local num_nodes=$1

    mapfile -t SORTED < <(get_sorted_nodes | grep ' free$')

    if [[ ${#SORTED[@]} -lt $num_nodes ]]; then
        echo -e "${RED}Error: Only ${#SORTED[@]} available nodes (with free port), but $num_nodes requested${NC}" >&2
        exit 1
    fi

    echo -e "${GREEN}Selected nodes (by lowest load, port ${PORT} free):${NC}" >&2
    printf "  %-12s %s\n" "NODE" "LOAD" >&2

    for i in $(seq 0 $((num_nodes - 1))); do
        load=$(echo "${SORTED[$i]}" | cut -d' ' -f1)
        node=$(echo "${SORTED[$i]}" | cut -d' ' -f2)
        printf "  ${CYAN}%-12s %s${NC}\n" "$node" "$load" >&2
        echo "$node"
    done
}

# ─────────────────────────────────────────────────────────────────────────────
# Cleanup
# ─────────────────────────────────────────────────────────────────────────────

cmd_cleanup() {
    echo -e "${GREEN}=== Killing consensus-node on all nodes ===${NC}"
    echo ""

    for node in "${ALL_NODES[@]}"; do
        local host="${node}.${DOMAIN}"
        (
            result=$(ssh -o StrictHostKeyChecking=no -o ConnectTimeout=3 -o BatchMode=yes \
                "${USERNAME}@${host}" "pkill -u $USERNAME -x consensus-node 2>/dev/null && echo killed" 2>/dev/null)
            if [[ -n "$result" ]]; then
                echo -e "${YELLOW}[$node]${NC} killed"
            fi
        ) &
    done
    wait

    echo ""
    echo -e "${GREEN}Done${NC}"
}

# ─────────────────────────────────────────────────────────────────────────────
# Run: sync → prep → launch
# ─────────────────────────────────────────────────────────────────────────────

# Rsync sources to NFS home (one-shot, shared across all selected nodes).
sync_sources() {
    echo -e "${CYAN}==> Syncing sources to ${USERNAME}@sunlab.${DOMAIN}:${NFS_REPO}${NC}"
    ssh -o StrictHostKeyChecking=no -o BatchMode=no \
        "${USERNAME}@sunlab.${DOMAIN}" "mkdir -p ${NFS_REPO}" >/dev/null
    rsync -az --human-readable \
        --exclude='target/' --exclude='workloads/' --exclude='logs/' --exclude='.git/' \
        Cargo.toml Cargo.lock crates node zkvm install-protoc.sh \
        "${USERNAME}@sunlab.${DOMAIN}:${NFS_REPO}/"
}

# Prep one node: workload rsync + rust/protoc bootstrap + cargo build.
# Logs to ${LOG_DIR}/${node}-prep.log. Returns nonzero on failure.
prep_node() {
    local node=$1
    local workload=$2
    local host="${node}.${DOMAIN}"
    local log_file="${LOG_DIR}/${node}-prep.log"

    {
        echo "=== [$(date +%H:%M:%S)] Workload rsync: $workload → /scratch/workloads/ on $node ==="
        ssh -o StrictHostKeyChecking=no "${USERNAME}@${host}" "mkdir -p /scratch/workloads"
        rsync -az --human-readable "workloads/${workload}" "${USERNAME}@${host}:/scratch/workloads/"

        echo ""
        echo "=== [$(date +%H:%M:%S)] Remote bootstrap + build on $node ==="
        ssh -o StrictHostKeyChecking=no "${USERNAME}@${host}" "bash -s" <<REMOTE_EOF
set -e

export RUSTUP_HOME=${RUSTUP_HOME_REMOTE}
export CARGO_HOME=${CARGO_HOME_REMOTE}

# Source cargo env if present (sets PATH).
if [ -f "\$CARGO_HOME/env" ]; then
    . "\$CARGO_HOME/env"
fi

# Rust 1.91 install (idempotent).
if ! command -v rustc >/dev/null 2>&1 || ! rustc --version | grep -q "${RUST_VERSION}"; then
    echo "Installing rust ${RUST_VERSION} to \$RUSTUP_HOME + \$CARGO_HOME ..."
    mkdir -p "\$RUSTUP_HOME" "\$CARGO_HOME"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain ${RUST_VERSION} --no-modify-path
    . "\$CARGO_HOME/env"
else
    echo "rust already at: \$(rustc --version)"
fi

# Protoc install (idempotent; install-protoc.sh checks for existing install).
if [ ! -x "${PROTOC_BIN_REMOTE}" ]; then
    echo "Installing protoc ..."
    bash "\$HOME/${NFS_REPO}/install-protoc.sh"
else
    echo "protoc already at: ${PROTOC_BIN_REMOTE}"
fi

export PROTOC="${PROTOC_BIN_REMOTE}"
cd "\$HOME/${NFS_REPO}"

echo "=== cargo build --release -p consensus-node ==="
cargo build --release -p consensus-node --target-dir "${TARGET_DIR}"
echo "=== build complete ==="
REMOTE_EOF
    } >"$log_file" 2>&1
}

# Build --peers arg for a given node: comma-separated hostnames of the OTHER nodes.
build_peers() {
    local current_node=$1
    shift
    local peers=""
    for node in "$@"; do
        if [[ "$node" != "$current_node" ]]; then
            if [[ -n "$peers" ]]; then
                peers="${peers},${node}"
            else
                peers="$node"
            fi
        fi
    done
    echo "$peers"
}

cmd_run() {
    local num_nodes=$1
    local num_slow=$2
    local workload=$3
    local mode=$4
    local slow_delay_ms=$5

    # Validate workload.
    if [[ ! -d "workloads/${workload}" ]]; then
        echo -e "${RED}Error: workloads/${workload} not found${NC}" >&2
        exit 1
    fi
    if [[ ! -f "workloads/${workload}/ledger-program.elf" ]]; then
        echo -e "${RED}Error: workloads/${workload}/ledger-program.elf missing${NC}" >&2
        exit 1
    fi

    if (( num_slow < 0 || num_slow > num_nodes )); then
        echo -e "${RED}Error: ns must satisfy 0 <= ns <= n${NC}" >&2
        exit 1
    fi

    rm -rf "$LOG_DIR"
    mkdir -p "$LOG_DIR"

    cat >"$LOG_DIR/run_info.txt" <<EOF
Run started: $(date)
n=${num_nodes} ns=${num_slow} mode=${mode} workload=${workload} slow_delay_ms=${slow_delay_ms}
EOF

    # 1) Source sync.
    sync_sources

    # 2) Node selection.
    echo ""
    mapfile -t nodes < <(select_nodes "$num_nodes")

    # 3) Parallel per-node prep.
    echo ""
    echo -e "${CYAN}==> Prep phase (rsync workload, install rust+protoc, cargo build)${NC}"
    echo -e "${CYAN}    Tailing: tail -f ${LOG_DIR}/<node>-prep.log${NC}"
    echo ""

    declare -A PREP_PIDS
    for node in "${nodes[@]}"; do
        prep_node "$node" "$workload" &
        PREP_PIDS[$node]=$!
    done

    local prep_failed=0
    for node in "${!PREP_PIDS[@]}"; do
        if wait "${PREP_PIDS[$node]}"; then
            echo -e "${GREEN}[$node]${NC} prep done"
        else
            echo -e "${RED}[$node]${NC} prep FAILED (see ${LOG_DIR}/${node}-prep.log)"
            ((prep_failed++))
        fi
    done

    if (( prep_failed > 0 )); then
        echo ""
        echo -e "${RED}Prep failed on ${prep_failed} node(s); aborting${NC}"
        exit 1
    fi

    # 4) Launch phase.
    echo ""
    echo -e "${GREEN}=== Launching consensus-node on ${num_nodes} machines ===${NC}"
    echo -e "  mode:            ${BLUE}${mode}${NC}"
    echo -e "  workload:        ${BLUE}${workload}${NC}"
    echo -e "  slow_delay_ms:   ${BLUE}${slow_delay_ms}${NC}"
    echo ""

    declare -A PIDS

    cleanup() {
        echo ""
        echo -e "${RED}Caught interrupt, stopping all nodes...${NC}"
        for node in "${!PIDS[@]}"; do
            kill "${PIDS[$node]}" 2>/dev/null && echo -e "${YELLOW}[$node]${NC} stopped (local ssh)"
        done
        for node in "${nodes[@]}"; do
            ssh -o StrictHostKeyChecking=no -o ConnectTimeout=5 -o BatchMode=yes \
                "${USERNAME}@${node}.${DOMAIN}" \
                "pkill -u $USERNAME -x consensus-node" 2>/dev/null
        done
        echo ""
        echo -e "${GREEN}=== Cleanup complete ===${NC}"
        echo -e "Logs: ${BLUE}${LOG_DIR}${NC}"
        exit 0
    }
    trap cleanup SIGINT SIGTERM

    for i in "${!nodes[@]}"; do
        local node="${nodes[$i]}"
        local host="${node}.${DOMAIN}"
        local log_file="${LOG_DIR}/${node}.log"
        local peers
        peers=$(build_peers "$node" "${nodes[@]}")

        local speed="fast"
        if (( i < num_slow )); then
            speed="slow"
        fi

        local cmd="${TARGET_DIR}/release/consensus-node"
        cmd+=" --node-id ${i}"
        cmd+=" --speed ${speed}"
        cmd+=" --peers ${peers}"
        cmd+=" --port ${PORT}"
        cmd+=" --mode ${mode}"
        cmd+=" --workload ${workload}"
        cmd+=" --workloads-dir /scratch/workloads"
        if [[ "$speed" == "slow" && "$mode" == "reexecute" ]]; then
            cmd+=" --slow-delay-ms ${slow_delay_ms}"
        fi

        echo -e "${YELLOW}[$node]${NC} id=${i} speed=${speed} peers=${peers}"

        ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 \
            "${USERNAME}@${host}" \
            ". ${CARGO_HOME_REMOTE}/env 2>/dev/null; exec $cmd" \
            >"$log_file" 2>&1 &
        PIDS[$node]=$!
    done

    echo ""
    echo -e "${GREEN}All nodes started. Waiting for completion (Ctrl+C to stop).${NC}"
    echo ""

    local failed=0
    for node in "${!PIDS[@]}"; do
        if wait "${PIDS[$node]}"; then
            echo -e "${GREEN}[$node]${NC} completed"
        else
            echo -e "${RED}[$node]${NC} failed"
            ((failed++))
        fi
    done

    echo ""
    if (( failed > 0 )); then
        echo -e "${RED}${failed} node(s) failed${NC}"
    else
        echo -e "${GREEN}=== Run Complete ===${NC}"
    fi
    echo -e "Logs: ${BLUE}${LOG_DIR}${NC}"
}

# ─────────────────────────────────────────────────────────────────────────────
# Main
# ─────────────────────────────────────────────────────────────────────────────

if [[ $# -eq 0 ]]; then
    usage
fi

COMMAND="$1"
shift

NUM_NODES=""
NUM_SLOW=""
WORKLOAD=""
MODE="reexecute"
SLOW_DELAY_MS="500"

while [[ $# -gt 0 ]]; do
    case $1 in
    -n)
        NUM_NODES="$2"
        shift 2
        ;;
    -ns)
        NUM_SLOW="$2"
        shift 2
        ;;
    -w | --workload)
        WORKLOAD="$2"
        shift 2
        ;;
    -v)
        MODE="verify"
        shift
        ;;
    --slow-delay-ms)
        SLOW_DELAY_MS="$2"
        shift 2
        ;;
    -h | --help)
        usage
        ;;
    *)
        echo -e "${RED}Unknown option: $1${NC}"
        usage
        ;;
    esac
done

case "$COMMAND" in
list)
    cmd_list
    ;;
run)
    if [[ -z "$NUM_NODES" ]]; then
        echo -e "${RED}Error: -n <num> is required for 'run'${NC}"
        usage
    fi
    if [[ -z "$NUM_SLOW" ]]; then
        echo -e "${RED}Error: -ns <num> is required for 'run'${NC}"
        usage
    fi
    if [[ -z "$WORKLOAD" ]]; then
        echo -e "${RED}Error: -w <name> is required for 'run'${NC}"
        usage
    fi
    if ! [[ "$NUM_NODES" =~ ^[0-9]+$ ]] || [[ "$NUM_NODES" -lt 1 ]]; then
        echo -e "${RED}Error: -n must be a positive integer${NC}"
        exit 1
    fi
    if ! [[ "$NUM_SLOW" =~ ^[0-9]+$ ]]; then
        echo -e "${RED}Error: -ns must be a non-negative integer${NC}"
        exit 1
    fi
    if ! [[ "$SLOW_DELAY_MS" =~ ^[0-9]+$ ]]; then
        echo -e "${RED}Error: --slow-delay-ms must be a non-negative integer${NC}"
        exit 1
    fi
    cmd_run "$NUM_NODES" "$NUM_SLOW" "$WORKLOAD" "$MODE" "$SLOW_DELAY_MS"
    ;;
cleanup)
    cmd_cleanup
    ;;
-h | --help)
    usage
    ;;
*)
    echo -e "${RED}Unknown command: $COMMAND${NC}"
    usage
    ;;
esac
