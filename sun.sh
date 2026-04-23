#!/bin/bash
#
# sun.sh - Deploy and run zk-stf consensus-node across the Sunlab cluster.
#
# RUN THIS FROM A SUNLAB COMPUTE NODE, not locally. Passwordless SSH to other
# compute nodes is only available from inside the cluster.
#
# Prereq: run ./deploy-remote.sh locally first to push sources to NFS home
# (~/cse476/zk-stf) and the target workload to THIS machine's /scratch.
# Then ssh to that machine, cd into ~/cse476/zk-stf, and run sun.sh.
#
# Two-phase `run`:
#   1. prep (per-node, parallel): rsync workload from local /scratch to target
#      /scratch, ensure rust 1.91 + protoc on /scratch, build consensus-node
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

# Shared SSH options: never prompt, don't check host IP (sunlab hostname/IP
# pairs drift), and quietly accept unknown host keys (intra-cluster trust).
SSH_OPTS="-o StrictHostKeyChecking=no -o CheckHostIP=no -o BatchMode=yes"
# rsync invocations must pipe ssh options through -e since rsync forks its
# own ssh and doesn't inherit command-line options otherwise.
RSYNC_SSH="ssh ${SSH_OPTS}"

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
  list                                                 List nodes with current CPU load + port status
  run -n <N> -ns <NS> -w <workload> [-v] [opts]        Run consensus-node once on the N least-loaded free-port nodes
  sweep -n <N> -ns <NS> -w <w1,w2,...> [opts]          Run each workload twice (reexecute + verify) on the same selected nodes
  cleanup                                              Kill consensus-node on all nodes

Run/sweep options:
  -n <N>                     Total nodes (required)
  -ns <NS>                   Number of slow nodes (required, 0 <= NS <= N)
  -w, --workload <name[,…]>  Workload(s) under workloads/; run takes one, sweep takes a comma-separated list
  -v                         (run only) proof-verify mode; sweep always does both modes
  --slow-delay-per-tx-ns <n> Per-tx sleep (ns) for slow nodes in re-execute mode;
                             total per-block sleep = n * txs_total (default: 500,
                             i.e. 500ms for a 1M-tx block)
  -h, --help                 Show this help

Examples:
  sun.sh list
  sun.sh run -n 4 -ns 2 -w one_million
  sun.sh run -n 4 -ns 2 -v -w one_million
  sun.sh sweep -n 4 -ns 2 -w one_k,ten_k,one_million
  sun.sh sweep -n 4 -ns 2 -w one_k,ten_k --slow-delay-per-tx-ns 1000
  sun.sh cleanup

Node IDs are assigned 0..N-1 in select order. Slow nodes are IDs 0..NS-1.
Round-robin leader = round % N, so slow nodes lead first.

Logs:
  run:   logs/<node>-prep.log (setup) and logs/<node>.log (runtime)
  sweep: logs/sweep-<timestamp>/{progress.log,summary.csv,<workload>-<mode>/<node>.log}
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

    local result=$(ssh $SSH_OPTS -o ConnectTimeout=3 \
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
            result=$(ssh $SSH_OPTS -o ConnectTimeout=3 \
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
# Run: prep → launch
#
# Sources are expected to already be on NFS home (~/cse476/zk-stf), placed
# there by deploy-remote.sh running locally. The workload is expected to
# already be at /scratch/workloads/<name>/ on THIS machine (same origin).
# prep_node propagates that workload dir to each selected peer's /scratch.
# ─────────────────────────────────────────────────────────────────────────────

# Prep one node: workload rsync (from this machine) + rust/protoc bootstrap + cargo build.
# Logs to ${LOG_DIR}/${node}-prep.log. Returns nonzero on failure.
prep_node() {
    local node=$1
    local workload=$2
    local log_dir=$3
    local host="${node}.${DOMAIN}"
    local log_file="${log_dir}/${node}-prep.log"
    local self_short
    self_short="$(hostname -s)"

    {
        if [[ "$node" == "$self_short" ]]; then
            echo "=== [$(date +%H:%M:%S)] Workload rsync skipped (this is the source machine) ==="
        else
            echo "=== [$(date +%H:%M:%S)] Workload rsync: /scratch/workloads/$workload → $node:/scratch/workloads/ ==="
            ssh $SSH_OPTS "${USERNAME}@${host}" "mkdir -p /scratch/workloads"
            rsync -az --human-readable -e "$RSYNC_SSH" "/scratch/workloads/${workload}" "${USERNAME}@${host}:/scratch/workloads/"
        fi

        echo ""
        echo "=== [$(date +%H:%M:%S)] Remote bootstrap + build on $node ==="
        ssh $SSH_OPTS "${USERNAME}@${host}" "bash -s" <<REMOTE_EOF
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

_validate_workload_local() {
    local workload=$1
    if [[ ! -d "/scratch/workloads/${workload}" ]]; then
        echo -e "${RED}Error: /scratch/workloads/${workload} not found on $(hostname -s)${NC}" >&2
        echo -e "${RED}Run deploy-remote.sh locally first (SUNLAB_MACHINE_NAME=$(hostname -s))${NC}" >&2
        exit 1
    fi
    if [[ ! -f "/scratch/workloads/${workload}/ledger-program.elf" ]]; then
        echo -e "${RED}Error: /scratch/workloads/${workload}/ledger-program.elf missing${NC}" >&2
        exit 1
    fi
}

# Best-effort pkill of consensus-node on every host in SELECTED_NODES.
_kill_remote_nodes() {
    for node in "${SELECTED_NODES[@]}"; do
        ssh $SSH_OPTS -o ConnectTimeout=5 \
            "${USERNAME}@${node}.${DOMAIN}" \
            "pkill -u $USERNAME -x consensus-node" 2>/dev/null
    done
}

# Run one experiment (prep + launch + wait) against the current SELECTED_NODES.
# Returns 0 on success, nonzero if prep or any launched node failed.
# Args: log_dir workload mode slow_delay_per_tx_ns num_slow
_do_run() {
    local log_dir=$1
    local workload=$2
    local mode=$3
    local slow_delay_per_tx_ns=$4
    local num_slow=$5

    mkdir -p "$log_dir"

    cat >"$log_dir/run_info.txt" <<EOF
Run started: $(date)
source_host=$(hostname -s)
nodes=${SELECTED_NODES[*]}
n=${#SELECTED_NODES[@]} ns=${num_slow} mode=${mode} workload=${workload} slow_delay_per_tx_ns=${slow_delay_per_tx_ns}
EOF

    # Prep phase
    echo ""
    echo -e "${CYAN}==> [${workload}/${mode}] Prep (rsync workload + build)${NC}"
    echo -e "${CYAN}    Tailing: tail -f ${log_dir}/<node>-prep.log${NC}"

    declare -A PREP_PIDS
    for node in "${SELECTED_NODES[@]}"; do
        prep_node "$node" "$workload" "$log_dir" &
        PREP_PIDS[$node]=$!
    done

    local prep_failed=0
    for node in "${!PREP_PIDS[@]}"; do
        if wait "${PREP_PIDS[$node]}"; then
            echo -e "${GREEN}[$node]${NC} prep done"
        else
            echo -e "${RED}[$node]${NC} prep FAILED (see ${log_dir}/${node}-prep.log)"
            ((prep_failed++))
        fi
    done

    if (( prep_failed > 0 )); then
        echo -e "${RED}Prep failed on ${prep_failed} node(s)${NC}"
        return 1
    fi

    # Launch phase
    echo ""
    echo -e "${GREEN}=== Launching consensus-node on ${#SELECTED_NODES[@]} machines ===${NC}"
    echo -e "  mode:                  ${BLUE}${mode}${NC}"
    echo -e "  workload:              ${BLUE}${workload}${NC}"
    echo -e "  slow_delay_per_tx_ns:  ${BLUE}${slow_delay_per_tx_ns}${NC}"
    echo ""

    declare -A PIDS

    _run_cleanup() {
        echo ""
        echo -e "${RED}Caught interrupt, stopping all nodes...${NC}"
        for node in "${!PIDS[@]}"; do
            kill "${PIDS[$node]}" 2>/dev/null && echo -e "${YELLOW}[$node]${NC} stopped (local ssh)"
        done
        _kill_remote_nodes
        echo -e "${GREEN}=== Cleanup complete ===${NC}"
        echo -e "Logs: ${BLUE}${log_dir}${NC}"
        exit 130
    }
    trap _run_cleanup SIGINT SIGTERM

    for i in "${!SELECTED_NODES[@]}"; do
        local node="${SELECTED_NODES[$i]}"
        local host="${node}.${DOMAIN}"
        local log_file="${log_dir}/${node}.log"
        local peers
        peers=$(build_peers "$node" "${SELECTED_NODES[@]}")

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
            cmd+=" --slow-delay-per-tx-ns ${slow_delay_per_tx_ns}"
        fi

        echo -e "${YELLOW}[$node]${NC} id=${i} speed=${speed} peers=${peers}"

        ssh $SSH_OPTS -o ConnectTimeout=10 \
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

    trap - SIGINT SIGTERM

    if (( failed > 0 )); then
        # Best-effort kill of any lingering consensus-node still running on the hosts.
        _kill_remote_nodes
        return 1
    fi
    return 0
}

cmd_run() {
    local num_nodes=$1
    local num_slow=$2
    local workload=$3
    local mode=$4
    local slow_delay_per_tx_ns=$5

    _validate_workload_local "$workload"

    if (( num_slow < 0 || num_slow > num_nodes )); then
        echo -e "${RED}Error: ns must satisfy 0 <= ns <= n${NC}" >&2
        exit 1
    fi

    rm -rf "$LOG_DIR"
    mkdir -p "$LOG_DIR"

    echo ""
    mapfile -t SELECTED_NODES < <(select_nodes "$num_nodes")

    if _do_run "$LOG_DIR" "$workload" "$mode" "$slow_delay_per_tx_ns" "$num_slow"; then
        echo ""
        echo -e "${GREEN}=== Run Complete ===${NC}"
    else
        echo ""
        echo -e "${RED}=== Run FAILED (see logs) ===${NC}"
    fi
    echo -e "Logs: ${BLUE}${LOG_DIR}${NC}"
}

# ─────────────────────────────────────────────────────────────────────────────
# Sweep: run each workload twice (reexecute + verify) on the same node set.
# ─────────────────────────────────────────────────────────────────────────────

# Append one CSV row per node for this run. Parses the `===== summary:` line
# written by consensus-node at exit time; missing/failed runs get blank fields.
_append_summary_rows() {
    local csv=$1 workload=$2 mode=$3 status=$4 run_log_dir=$5 num_slow=$6

    for i in "${!SELECTED_NODES[@]}"; do
        local node="${SELECTED_NODES[$i]}"
        local log_file="${run_log_dir}/${node}.log"
        local speed="fast"
        (( i < num_slow )) && speed="slow"

        local summary_line=""
        if [[ -f "$log_file" ]]; then
            summary_line=$(grep "===== summary:" "$log_file" | head -1)
        fi
        local blocks="" txs="" wall="" throughput=""
        if [[ -n "$summary_line" ]]; then
            blocks=$(echo "$summary_line"     | grep -oE 'blocks=[0-9]+'         | cut -d= -f2)
            txs=$(echo "$summary_line"        | grep -oE 'txs=[0-9]+'            | cut -d= -f2)
            wall=$(echo "$summary_line"       | grep -oE 'wall=[0-9.]+[a-zµ]+'   | cut -d= -f2)
            throughput=$(echo "$summary_line" | grep -oE 'throughput=[0-9.]+'    | cut -d= -f2)
        fi
        echo "${workload},${mode},${node},${i},${speed},${status},${blocks},${txs},${wall},${throughput}" >>"$csv"
    done
}

cmd_sweep() {
    local num_nodes=$1
    local num_slow=$2
    local workloads_csv=$3
    local slow_delay_per_tx_ns=$4

    IFS=',' read -r -a WORKLOADS <<<"$workloads_csv"

    if [[ ${#WORKLOADS[@]} -eq 0 ]]; then
        echo -e "${RED}Error: -w requires at least one workload${NC}" >&2
        exit 1
    fi

    # Validate every workload locally before spending time on node selection.
    for w in "${WORKLOADS[@]}"; do
        _validate_workload_local "$w"
    done

    if (( num_slow < 0 || num_slow > num_nodes )); then
        echo -e "${RED}Error: ns must satisfy 0 <= ns <= n${NC}" >&2
        exit 1
    fi

    local ts
    ts=$(date +%Y%m%d-%H%M%S)
    local sweep_dir="${LOG_DIR}/sweep-${ts}"
    mkdir -p "$sweep_dir"

    local progress_log="${sweep_dir}/progress.log"
    local summary_csv="${sweep_dir}/summary.csv"

    cat >"${sweep_dir}/sweep_info.txt" <<EOF
Sweep started: $(date)
source_host=$(hostname -s)
workloads=${workloads_csv}
n=${num_nodes} ns=${num_slow} slow_delay_per_tx_ns=${slow_delay_per_tx_ns}
EOF

    echo "workload,mode,node,node_id,speed,status,blocks,txs,wall,throughput_tx_per_s" >"$summary_csv"

    _log_progress() {
        local line="[$(date +%H:%M:%S)] $1"
        echo -e "${CYAN}${line}${NC}"
        echo "$line" >>"$progress_log"
    }

    _log_progress "sweep start: workloads=${workloads_csv} n=${num_nodes} ns=${num_slow} slow_delay_per_tx_ns=${slow_delay_per_tx_ns}"

    # Select nodes ONCE — same set reused across every (workload × mode) for an
    # apples-to-apples comparison.
    echo ""
    mapfile -t SELECTED_NODES < <(select_nodes "$num_nodes")
    _log_progress "nodes selected: ${SELECTED_NODES[*]}"

    local modes=(reexecute verify)
    local total=$(( ${#WORKLOADS[@]} * ${#modes[@]} ))
    local idx=0
    local fail_count=0

    for workload in "${WORKLOADS[@]}"; do
        for mode in "${modes[@]}"; do
            ((idx++))
            local run_log_dir="${sweep_dir}/${workload}-${mode}"
            _log_progress "[${idx}/${total}] workload=${workload} mode=${mode} starting..."

            local status="OK"
            if _do_run "$run_log_dir" "$workload" "$mode" "$slow_delay_per_tx_ns" "$num_slow"; then
                _log_progress "[${idx}/${total}] workload=${workload} mode=${mode} DONE"
            else
                status="FAILED"
                ((fail_count++))
                touch "${run_log_dir}/FAILED"
                _log_progress "[${idx}/${total}] workload=${workload} mode=${mode} FAILED (see ${run_log_dir})"
            fi

            _append_summary_rows "$summary_csv" "$workload" "$mode" "$status" "$run_log_dir" "$num_slow"
        done
    done

    _log_progress "sweep complete: ${total} runs, ${fail_count} failed"
    echo ""
    if (( fail_count > 0 )); then
        echo -e "${RED}=== Sweep complete with ${fail_count}/${total} failed runs ===${NC}"
    else
        echo -e "${GREEN}=== Sweep complete: all ${total} runs OK ===${NC}"
    fi
    echo -e "Sweep dir: ${BLUE}${sweep_dir}${NC}"
    echo -e "Summary:   ${BLUE}${summary_csv}${NC}"
    echo -e "Progress:  ${BLUE}${progress_log}${NC}"
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
SLOW_DELAY_PER_TX_NS="500"

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
    --slow-delay-per-tx-ns)
        SLOW_DELAY_PER_TX_NS="$2"
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
    if ! [[ "$SLOW_DELAY_PER_TX_NS" =~ ^[0-9]+$ ]]; then
        echo -e "${RED}Error: --slow-delay-per-tx-ns must be a non-negative integer${NC}"
        exit 1
    fi
    cmd_run "$NUM_NODES" "$NUM_SLOW" "$WORKLOAD" "$MODE" "$SLOW_DELAY_PER_TX_NS"
    ;;
sweep)
    if [[ -z "$NUM_NODES" ]]; then
        echo -e "${RED}Error: -n <num> is required for 'sweep'${NC}"
        usage
    fi
    if [[ -z "$NUM_SLOW" ]]; then
        echo -e "${RED}Error: -ns <num> is required for 'sweep'${NC}"
        usage
    fi
    if [[ -z "$WORKLOAD" ]]; then
        echo -e "${RED}Error: -w <w1,w2,...> is required for 'sweep'${NC}"
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
    if ! [[ "$SLOW_DELAY_PER_TX_NS" =~ ^[0-9]+$ ]]; then
        echo -e "${RED}Error: --slow-delay-per-tx-ns must be a non-negative integer${NC}"
        exit 1
    fi
    cmd_sweep "$NUM_NODES" "$NUM_SLOW" "$WORKLOAD" "$SLOW_DELAY_PER_TX_NS"
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
