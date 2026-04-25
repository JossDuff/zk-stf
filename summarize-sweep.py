#!/usr/bin/env python3
"""Summarize a sun.sh sweep directory.

Prints per-workload throughput (reexecute vs verify) and per-node block-time
stats, and writes a detailed block_times.csv alongside the sweep.

Usage:
    ./summarize-sweep.py logs/sweep-YYYYMMDD-HHMMSS
"""
import csv
import re
import statistics
import sys
from collections import OrderedDict, defaultdict
from pathlib import Path

SUMMARY_RE = re.compile(
    r"===== summary:\s+blocks=(\d+)\s+txs=(\d+)\s+"
    r"validate_total=(\S+)\s+wall=(\S+)\s+throughput=([\d.]+)"
)
EVENT_RE = re.compile(
    r"round=(\d+)\s+(round_start|validate_start|validate_end|committed)\s+ts_ns=(\d+)"
)
DURATION_RE = re.compile(r"^([\d.]+)(ns|µs|us|ms|s)$")


def duration_to_ms(s):
    m = DURATION_RE.match(s)
    if not m:
        return None
    val, unit = float(m.group(1)), m.group(2)
    return {
        "ns": val / 1_000_000,
        "µs": val / 1_000,
        "us": val / 1_000,
        "ms": val,
        "s": val * 1_000,
    }[unit]


def parse_node_log(path):
    """Extract summary line + per-round event timestamps from one node log."""
    summary = None
    rounds = defaultdict(dict)
    if not path.is_file():
        return summary, rounds
    with path.open() as f:
        for line in f:
            m = SUMMARY_RE.search(line)
            if m:
                summary = {
                    "blocks": int(m.group(1)),
                    "txs": int(m.group(2)),
                    "validate_total_ms": duration_to_ms(m.group(3)),
                    "wall_ms": duration_to_ms(m.group(4)),
                    "throughput": float(m.group(5)),
                }
                continue
            m = EVENT_RE.search(line)
            if m:
                rounds[int(m.group(1))][m.group(2)] = int(m.group(3))
    return summary, rounds


def fmt_int(n):
    return "-" if n is None else f"{int(n):,}"


def fmt_ms(n):
    if n is None:
        return "-"
    if n < 10:
        return f"{n:.2f}"
    if n < 1000:
        return f"{n:.1f}"
    return f"{n:,.0f}"


def stats3(values):
    """min / median / max as formatted strings (ms)."""
    if not values:
        return "-", "-", "-"
    return fmt_ms(min(values)), fmt_ms(statistics.median(values)), fmt_ms(max(values))


def main():
    if len(sys.argv) != 2:
        print(__doc__, file=sys.stderr)
        sys.exit(2)

    sweep = Path(sys.argv[1]).resolve()
    if not sweep.is_dir():
        print(f"Not a directory: {sweep}", file=sys.stderr)
        sys.exit(1)

    info_path = sweep / "sweep_info.txt"
    summary_csv = sweep / "summary.csv"
    if not summary_csv.is_file():
        print(f"Missing {summary_csv}", file=sys.stderr)
        sys.exit(1)

    # Load CSV rows, preserving workload order of first appearance.
    rows_by_run = defaultdict(list)  # (workload, mode) -> list[row]
    workload_order = OrderedDict()
    with summary_csv.open() as f:
        for row in csv.DictReader(f):
            rows_by_run[(row["workload"], row["mode"])].append(row)
            workload_order.setdefault(row["workload"], None)

    # Parse every node log once.
    round_data = {}  # (workload, mode, node) -> {round: {event: ts_ns}}
    for (workload, mode), rows in rows_by_run.items():
        for row in rows:
            node = row["node"]
            log = sweep / f"{workload}-{mode}" / f"{node}.log"
            _, rounds = parse_node_log(log)
            round_data[(workload, mode, node)] = rounds

    # ── Header ─────────────────────────────────────────────────────────────
    print(f"=== Sweep: {sweep.name} ===")
    if info_path.is_file():
        print(info_path.read_text().strip())
    print()

    # ── Throughput table ───────────────────────────────────────────────────
    def run_stats(rows):
        """Median throughput + aggregate status for one run."""
        throughputs = [float(r["throughput_tx_per_s"]) for r in rows if r["throughput_tx_per_s"]]
        med = statistics.median(throughputs) if throughputs else None
        ok = bool(rows) and all(r["status"] == "OK" for r in rows)
        return med, "OK" if ok else "FAILED"

    print("=== Throughput (median across nodes, tx/s) ===")
    print(f"{'Workload':<16} {'Tx/block':>10} {'Re-execute':>14} {'Verify':>14} {'V/R':>7}  Status")
    for w in workload_order:
        r_rows = rows_by_run.get((w, "reexecute"), [])
        v_rows = rows_by_run.get((w, "verify"), [])
        tp_r, st_r = run_stats(r_rows)
        tp_v, st_v = run_stats(v_rows)

        # Tx/block: pull from any OK row; blocks × tx_per_block = txs
        tx_per_block = None
        for r in r_rows + v_rows:
            if r["status"] == "OK" and r["blocks"] and r["txs"]:
                tx_per_block = int(r["txs"]) // int(r["blocks"])
                break

        ratio = f"{tp_v/tp_r:.2f}x" if tp_r and tp_v else "-"
        status = f"R:{st_r} V:{st_v}"
        print(f"{w:<16} {fmt_int(tx_per_block):>10} {fmt_int(tp_r):>14} {fmt_int(tp_v):>14} {ratio:>7}  {status}")
    print()

    # ── Per-node block-time stats ──────────────────────────────────────────
    print("=== Per-node block times (ms) ===")
    for w in workload_order:
        for mode in ("reexecute", "verify"):
            rows = rows_by_run.get((w, mode), [])
            if not rows:
                continue
            print(f"\n--- {w} / {mode} ---")
            print(
                f"{'Node':<12} {'Speed':<5} {'Blks':>4}  "
                f"{'Validate min / med / max':>26}  "
                f"{'Round min / med / max':>26}   Status"
            )
            # Sort by node_id so slow nodes come first
            rows_sorted = sorted(rows, key=lambda r: int(r["node_id"]))
            for r in rows_sorted:
                node = r["node"]
                rounds = round_data.get((w, mode, node), {})
                v_times, r_times = [], []
                for events in rounds.values():
                    vs, ve = events.get("validate_start"), events.get("validate_end")
                    rs, cm = events.get("round_start"), events.get("committed")
                    if vs is not None and ve is not None:
                        v_times.append((ve - vs) / 1e6)
                    if rs is not None and cm is not None:
                        r_times.append((cm - rs) / 1e6)
                vmin, vmed, vmax = stats3(v_times)
                rmin, rmed, rmax = stats3(r_times)
                v_str = f"{vmin:>7} / {vmed:>7} / {vmax:>7}"
                r_str = f"{rmin:>7} / {rmed:>7} / {rmax:>7}"
                print(
                    f"{node:<12} {r['speed']:<5} {len(r_times):>4}  "
                    f"{v_str}  {r_str}   {r['status']}"
                )

    # ── Detail CSV ─────────────────────────────────────────────────────────
    out_csv = sweep / "block_times.csv"
    with out_csv.open("w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(
            ["workload", "mode", "node", "node_id", "speed", "round", "validate_ms", "round_ms"]
        )
        for (workload, mode), rows in rows_by_run.items():
            for row in rows:
                rounds = round_data.get((workload, mode, row["node"]), {})
                for rnum in sorted(rounds):
                    events = rounds[rnum]
                    vs, ve = events.get("validate_start"), events.get("validate_end")
                    rs, cm = events.get("round_start"), events.get("committed")
                    v_ms = (ve - vs) / 1e6 if vs is not None and ve is not None else ""
                    r_ms = (cm - rs) / 1e6 if rs is not None and cm is not None else ""
                    writer.writerow(
                        [workload, mode, row["node"], row["node_id"], row["speed"], rnum, v_ms, r_ms]
                    )

    print()
    print(f"Wrote per-block detail → {out_csv.relative_to(sweep.parent.parent) if sweep.is_relative_to(sweep.parent.parent) else out_csv}")


if __name__ == "__main__":
    main()
