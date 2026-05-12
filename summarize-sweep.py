#!/usr/bin/env python3
"""Summarize a sun.sh sweep directory.

Console output:
  - Per-workload throughput table (reexecute vs verify, median tx/s).
  - Per-(workload, mode, node) summary: blocks committed, total views used,
    NEXTVIEW count, median validation and per-block times.

CSV output (written into the sweep directory):
  - block_times.csv:  per-(workload, mode, node, height) — first_view,
                      commit_view, views_used, time_to_commit_ms, validate_ms.
  - view_times.csv:   per-(workload, mode, node, view) — leader_id,
                      leaf_height, start_ts_ns, end_ts_ns, duration_ms,
                      outcome (decided / timed_out).
  - view_votes.csv:   per-(workload, mode, node, view, phase) — whether the
                      node sent the phase's vote, when, or why it skipped.
                      Phases: prepare, pre_commit, commit.

These three CSVs are tidy data — one row per observation — so they plug
directly into pandas/ggplot/matplotlib for visualization.

Usage:
    ./summarize-sweep.py logs/sweep-YYYYMMDD-HHMMSS
"""
import csv
import re
import statistics
import sys
from collections import OrderedDict, defaultdict
from pathlib import Path


LINE_RE = re.compile(
    r"\[node (?P<node>\d+)\] view=(?P<view>\d+) (?P<event>\w+) "
    r"ts_ns=(?P<ts>\d+)(?P<extras>.*)$"
)
EXTRA_RE = re.compile(r"(\w+)=(\S+)")
SUMMARY_RE = re.compile(
    r"===== summary:\s+blocks=(\d+)\s+txs=(\d+)\s+wall=(\S+)\s+"
    r"throughput=([\d.]+)"
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
    """Read one node's log; return (summary_dict_or_None, list-of-events)."""
    summary = None
    events = []
    if not path.is_file():
        return summary, events
    with path.open() as f:
        for line in f:
            m = SUMMARY_RE.search(line)
            if m:
                summary = {
                    "blocks": int(m.group(1)),
                    "txs": int(m.group(2)),
                    "wall_ms": duration_to_ms(m.group(3)),
                    "throughput": float(m.group(4)),
                }
                continue
            m = LINE_RE.search(line)
            if m:
                events.append((
                    int(m.group("view")),
                    m.group("event"),
                    int(m.group("ts")),
                    dict(EXTRA_RE.findall(m.group("extras"))),
                ))
    return summary, events


def per_view_state(events):
    """Roll events up by view: leader, leaf height, start/end timestamps,
    outcome, and per-phase vote timestamps emitted by *this* node.

    Phase-skip events (e.g., `prepare_skip`) are recorded too so we can
    distinguish "didn't get there" from "voted no on purpose."
    """
    views = {}
    for view, event, ts, extras in events:
        v = views.setdefault(view, {
            "start_ts": None,
            "end_ts": None,
            "leader": None,
            "leaf_height": None,
            "outcome": None,
            "phase_votes": {},          # phase -> ts_ns
            "phase_skip_reasons": {},   # phase -> reason string
        })
        if event == "view_start":
            v["start_ts"] = ts
            try:
                v["leader"] = int(extras.get("leader", -1))
            except ValueError:
                v["leader"] = -1
        elif event in ("propose_send", "propose_recv"):
            if v["leaf_height"] is None:
                try:
                    v["leaf_height"] = int(extras.get("leaf_height", -1))
                except ValueError:
                    pass
        elif event == "view_decide":
            v["end_ts"] = ts
            v["outcome"] = "decided"
        elif event == "next_view":
            v["end_ts"] = ts
            v["outcome"] = "timed_out"
        elif event == "prepare_vote_send":
            v["phase_votes"]["prepare"] = ts
        elif event == "pre_commit_vote_send":
            v["phase_votes"]["pre_commit"] = ts
        elif event == "commit_vote_send":
            v["phase_votes"]["commit"] = ts
        elif event == "prepare_skip":
            v["phase_skip_reasons"]["prepare"] = extras.get("reason", "?")
    return views


def per_height_state(events):
    """Roll events up by blockchain height: when first proposed, when
    committed (executed), validation timing.
    """
    heights = {}

    def hint(d, key):
        try:
            return int(d.get(key, -1))
        except ValueError:
            return -1

    for view, event, ts, extras in events:
        if event in ("propose_send", "propose_recv"):
            h = hint(extras, "leaf_height")
            if h < 0:
                continue
            d = heights.setdefault(h, {})
            if "first_view" not in d:
                d["first_view"] = view
                d["first_ts"] = ts
        elif event == "execute":
            h = hint(extras, "height")
            if h < 0:
                continue
            d = heights.setdefault(h, {})
            d["commit_view"] = view
            d["commit_ts"] = ts
        elif event == "validate_start":
            h = hint(extras, "height")
            if h < 0:
                continue
            d = heights.setdefault(h, {})
            if "validate_start_ts" not in d:
                d["validate_start_ts"] = ts
        elif event == "validate_end":
            h = hint(extras, "height")
            if h < 0:
                continue
            d = heights.setdefault(h, {})
            d["validate_end_ts"] = ts
            d["validate_kind"] = extras.get("kind")
            d["validate_valid"] = extras.get("valid") == "true"

    for d in heights.values():
        if "first_ts" in d and "commit_ts" in d:
            d["time_to_commit_ms"] = (d["commit_ts"] - d["first_ts"]) / 1e6
        if "validate_start_ts" in d and "validate_end_ts" in d:
            d["validate_ms"] = (d["validate_end_ts"] - d["validate_start_ts"]) / 1e6
        if "first_view" in d and "commit_view" in d:
            d["views_used"] = d["commit_view"] - d["first_view"] + 1
    return heights


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

    rows_by_run = defaultdict(list)
    workload_order = OrderedDict()
    with summary_csv.open() as f:
        for row in csv.DictReader(f):
            rows_by_run[(row["workload"], row["mode"])].append(row)
            workload_order.setdefault(row["workload"], None)

    # Parse every node log once; cache per (workload, mode, node).
    node_data = {}  # -> (summary, views_dict, heights_dict)
    for (workload, mode), rows in rows_by_run.items():
        for row in rows:
            node = row["node"]
            log = sweep / f"{workload}-{mode}" / f"{node}.log"
            summary, events = parse_node_log(log)
            node_data[(workload, mode, node)] = (
                summary,
                per_view_state(events),
                per_height_state(events),
            )

    # ── Header ────────────────────────────────────────────────────────────
    print(f"=== Sweep: {sweep.name} ===")
    if info_path.is_file():
        print(info_path.read_text().strip())
    print()

    # ── Per-node block & view stats ───────────────────────────────────────
    print("=== Per-node block + view stats ===")
    print("    Blks  = heights this node committed")
    print("    Views = total views entered (incl. NEXTVIEW failures)")
    print("    NextV = NEXTVIEW (timed-out) count")
    print()
    for w in workload_order:
        for mode in ("reexecute", "verify"):
            rows = rows_by_run.get((w, mode), [])
            if not rows:
                continue
            print(f"--- {w} / {mode} ---")
            print(
                f"{'Node':<12} {'Speed':<5} {'Blks':>4} {'Views':>5} {'NextV':>5}  "
                f"{'Validate min/med/max (ms)':>30}  "
                f"{'Block-time min/med/max (ms)':>30}   Status"
            )
            rows_sorted = sorted(rows, key=lambda r: int(r["node_id"]))
            for r in rows_sorted:
                node = r["node"]
                _, views, heights = node_data.get(
                    (w, mode, node), (None, {}, {})
                )
                v_times = [
                    d["validate_ms"] for d in heights.values() if "validate_ms" in d
                ]
                t_times = [
                    d["time_to_commit_ms"]
                    for d in heights.values()
                    if "time_to_commit_ms" in d
                ]
                num_views_total = len(views)
                num_next_view = sum(
                    1 for vd in views.values() if vd["outcome"] == "timed_out"
                )
                vmin, vmed, vmax = stats3(v_times)
                tmin, tmed, tmax = stats3(t_times)
                print(
                    f"{node:<12} {r['speed']:<5} {len(t_times):>4} "
                    f"{num_views_total:>5} {num_next_view:>5}  "
                    f"{vmin:>9} / {vmed:>9} / {vmax:>9}  "
                    f"{tmin:>9} / {tmed:>9} / {tmax:>9}   {r['status']}"
                )
            print()

    # ── CSV: per-block ────────────────────────────────────────────────────
    block_csv = sweep / "block_times.csv"
    with block_csv.open("w", newline="") as f:
        wr = csv.writer(f)
        wr.writerow([
            "workload", "mode", "node", "node_id", "speed",
            "height", "first_view", "commit_view", "views_used",
            "time_to_commit_ms", "validate_ms", "validate_kind", "validate_valid",
        ])
        for (workload, mode), rows in rows_by_run.items():
            for row in rows:
                node = row["node"]
                _, _, heights = node_data.get(
                    (workload, mode, node), (None, {}, {})
                )
                for h in sorted(heights):
                    d = heights[h]
                    wr.writerow([
                        workload, mode, node, row["node_id"], row["speed"], h,
                        d.get("first_view", ""),
                        d.get("commit_view", ""),
                        d.get("views_used", ""),
                        d.get("time_to_commit_ms", ""),
                        d.get("validate_ms", ""),
                        d.get("validate_kind", ""),
                        d.get("validate_valid", ""),
                    ])

    # ── CSV: per-view ─────────────────────────────────────────────────────
    view_csv = sweep / "view_times.csv"
    with view_csv.open("w", newline="") as f:
        wr = csv.writer(f)
        wr.writerow([
            "workload", "mode", "node", "node_id", "speed",
            "view", "leader", "leaf_height",
            "start_ts_ns", "end_ts_ns", "duration_ms", "outcome",
        ])
        for (workload, mode), rows in rows_by_run.items():
            for row in rows:
                node = row["node"]
                _, views, _ = node_data.get(
                    (workload, mode, node), (None, {}, {})
                )
                for v in sorted(views):
                    d = views[v]
                    duration_ms = (
                        (d["end_ts"] - d["start_ts"]) / 1e6
                        if d.get("start_ts") and d.get("end_ts")
                        else ""
                    )
                    wr.writerow([
                        workload, mode, node, row["node_id"], row["speed"], v,
                        d.get("leader", ""),
                        d.get("leaf_height", ""),
                        d.get("start_ts", ""),
                        d.get("end_ts", ""),
                        duration_ms,
                        d.get("outcome", ""),
                    ])

    # ── CSV: per-(node, view, phase) participation ────────────────────────
    votes_csv = sweep / "view_votes.csv"
    with votes_csv.open("w", newline="") as f:
        wr = csv.writer(f)
        wr.writerow([
            "workload", "mode", "node", "node_id", "speed",
            "view", "phase", "voted", "ts_ns", "skip_reason",
        ])
        for (workload, mode), rows in rows_by_run.items():
            for row in rows:
                node = row["node"]
                _, views, _ = node_data.get(
                    (workload, mode, node), (None, {}, {})
                )
                for v in sorted(views):
                    d = views[v]
                    for phase in ("prepare", "pre_commit", "commit"):
                        ts = d["phase_votes"].get(phase)
                        skip_reason = d["phase_skip_reasons"].get(phase, "")
                        if ts is not None:
                            voted = "yes"
                        elif skip_reason:
                            voted = "skipped"
                        else:
                            voted = "missed"
                        wr.writerow([
                            workload, mode, node, row["node_id"], row["speed"],
                            v, phase, voted, ts or "", skip_reason,
                        ])

    print(f"Wrote {block_csv.name}")
    print(f"Wrote {view_csv.name}")
    print(f"Wrote {votes_csv.name}")
    print()

    # ── Aggregate summaries ───────────────────────────────────────────────
    # Helper: per-(workload, mode) median across nodes of each node's median
    # of the values extracted by `extract(heights_dict)`.
    def median_across_nodes(rows, mode, workload, extract):
        per_node = []
        for r in rows:
            _, _, heights = node_data.get(
                (workload, mode, r["node"]), (None, {}, {})
            )
            vals = extract(heights)
            if vals:
                per_node.append(statistics.median(vals))
        return statistics.median(per_node) if per_node else None

    def heights_block_times(heights):
        return [d["time_to_commit_ms"] for d in heights.values() if "time_to_commit_ms" in d]

    def heights_validate_times(heights):
        return [d["validate_ms"] for d in heights.values() if "validate_ms" in d]

    # ── Throughput table ─────────────────────────────────────────────────
    def run_throughput(rows):
        throughputs = [
            float(r["throughput_tx_per_s"])
            for r in rows
            if r["throughput_tx_per_s"]
        ]
        med = statistics.median(throughputs) if throughputs else None
        ok = bool(rows) and all(r["status"] == "OK" for r in rows)
        return med, "OK" if ok else "FAILED"

    print("=== Throughput (median across nodes, tx/s) ===")
    print(
        f"{'Workload':<16} {'Tx/block':>10} {'Re-execute':>14} "
        f"{'Verify':>14} {'V/R':>7}  Status"
    )
    for w in workload_order:
        r_rows = rows_by_run.get((w, "reexecute"), [])
        v_rows = rows_by_run.get((w, "verify"), [])
        tp_r, st_r = run_throughput(r_rows)
        tp_v, st_v = run_throughput(v_rows)

        tx_per_block = None
        for r in r_rows + v_rows:
            if r["status"] == "OK" and r["blocks"] and r["txs"]:
                tx_per_block = int(r["txs"]) // int(r["blocks"])
                break

        ratio = f"{tp_v/tp_r:.2f}x" if tp_r and tp_v else "-"
        status = f"R:{st_r} V:{st_v}"
        print(
            f"{w:<16} {fmt_int(tx_per_block):>10} {fmt_int(tp_r):>14} "
            f"{fmt_int(tp_v):>14} {ratio:>7}  {status}"
        )
    print()

    # ── Block-time table ─────────────────────────────────────────────────
    print("=== Median block-time (median across nodes of each node's median, ms) ===")
    print(
        f"{'Workload':<16} {'Tx/block':>10} {'Re-execute':>14} "
        f"{'Verify':>14} {'R/V':>7}"
    )
    for w in workload_order:
        r_rows = rows_by_run.get((w, "reexecute"), [])
        v_rows = rows_by_run.get((w, "verify"), [])
        bt_r = median_across_nodes(r_rows, "reexecute", w, heights_block_times)
        bt_v = median_across_nodes(v_rows, "verify", w, heights_block_times)

        tx_per_block = None
        for r in r_rows + v_rows:
            if r["status"] == "OK" and r["blocks"] and r["txs"]:
                tx_per_block = int(r["txs"]) // int(r["blocks"])
                break

        ratio = f"{bt_r/bt_v:.2f}x" if bt_r and bt_v else "-"
        print(
            f"{w:<16} {fmt_int(tx_per_block):>10} {fmt_ms(bt_r):>14} "
            f"{fmt_ms(bt_v):>14} {ratio:>7}"
        )
    print()

    # ── Validate-time table ──────────────────────────────────────────────
    print("=== Median validate-time (median across nodes of each node's median, ms) ===")
    print(
        f"{'Workload':<16} {'Tx/block':>10} {'Re-execute':>14} "
        f"{'Verify':>14} {'R/V':>7}"
    )
    for w in workload_order:
        r_rows = rows_by_run.get((w, "reexecute"), [])
        v_rows = rows_by_run.get((w, "verify"), [])
        vt_r = median_across_nodes(r_rows, "reexecute", w, heights_validate_times)
        vt_v = median_across_nodes(v_rows, "verify", w, heights_validate_times)

        tx_per_block = None
        for r in r_rows + v_rows:
            if r["status"] == "OK" and r["blocks"] and r["txs"]:
                tx_per_block = int(r["txs"]) // int(r["blocks"])
                break

        ratio = f"{vt_r/vt_v:.2f}x" if vt_r and vt_v else "-"
        print(
            f"{w:<16} {fmt_int(tx_per_block):>10} {fmt_ms(vt_r):>14} "
            f"{fmt_ms(vt_v):>14} {ratio:>7}"
        )
    print()


if __name__ == "__main__":
    main()
