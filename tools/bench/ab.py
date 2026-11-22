#!/usr/bin/env python3
"""Interleaved A/B benchmark for every comparable CI case.

Sequential baseline/current batches drift with disk cache, Defender and power
state.  This runner alternates the two binaries in the same time window, uses
more rounds for sub-10 ms cases, and reports paired medians plus the raw
per-pair samples and process RSS.  The output feeds the "A/B 复核" table and
the read/write path verdict in the dashboard.

Writes docs/results/bench-ab.json (stdlib only).
"""

from __future__ import annotations

import argparse
import json
import math
import platform as host_platform
import statistics
import subprocess
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUT = "docs/results/bench-ab.json"

# Keep the commands identical to the ci suite, except for more rounds on the
# micro cases.  A 2-round median is too noisy to call a 5% regression.
CASES = [
    {
        "bench_id": "sql_scan",
        "args": ["sql-scan", "--rows", "512", "--rounds", "10"],
        "field": "scan_ms",
        "rounds": 10,
    },
    {
        "bench_id": "sql_point",
        "args": ["sql-point", "--rows", "512", "--lookups", "64"],
        "field": "point_ms",
        "rounds": 64,
    },
    {
        "bench_id": "sql_group",
        "args": ["sql-group", "--rows", "512", "--groups", "16", "--rounds", "10"],
        "field": "group_ms",
        "rounds": 10,
    },
    {
        "bench_id": "sql_sort",
        "args": ["sql-sort", "--rows", "512", "--limit", "16", "--rounds", "10"],
        "field": "sort_ms",
        "rounds": 10,
    },
    {
        "bench_id": "sql_insert",
        "args": [
            "sql-insert",
            "--rows",
            "128",
            "--rounds",
            "10",
            "--reset-each-round",
        ],
        "field": "insert_ms",
        "rounds": 10,
    },
    {
        "bench_id": "sql_update",
        "args": ["sql-update", "--rows", "512", "--cutoff", "64", "--rounds", "10"],
        "field": "update_ms",
        "rounds": 10,
    },
    {
        "bench_id": "sql_delete",
        "args": ["sql-delete", "--rows", "512", "--cutoff", "64", "--rounds", "10"],
        "field": "delete_ms",
        "rounds": 10,
    },
    {
        "bench_id": "sql_subquery",
        "args": [
            "sql-subquery",
            "--rows",
            "256",
            "--rounds",
            "10",
            "--query",
            "in",
        ],
        "field": "subquery_ms",
        "rounds": 10,
    },
    {
        "bench_id": "sql_window",
        "args": [
            "sql-window",
            "--rows",
            "512",
            "--rounds",
            "10",
            "--query",
            "rank",
        ],
        "field": "window_ms",
        "rounds": 10,
    },
    {
        "bench_id": "sql_join",
        "args": [
            "sql-join",
            "--rows",
            "512",
            "--rounds",
            "10",
            "--strategy",
            "auto",
        ],
        "field": "join_ms",
        "rounds": 10,
    },
    {
        "bench_id": "pstore_scan",
        "args": [
            "pstore-scan",
            "--pages",
            "64",
            "--payload",
            "1024",
            "--rounds",
            "10",
        ],
        "field": "scan_ms",
        "rounds": 10,
    },
    {
        "bench_id": "txn_put",
        "args": [
            "txn-put",
            "--txns",
            "64",
            "--keys-per-txn",
            "4",
            "--rounds",
            "10",
        ],
        "field": "put_ms",
        "rounds": 10,
    },
    {
        "bench_id": "pgwire_roundtrip",
        "args": ["pgwire", "--rows", "128", "--queries", "32", "--rounds", "10"],
        "field": "roundtrip_ms",
        "rounds": 10,
    },
    {
        "bench_id": "ann_knn",
        "args": [
            "ann-knn",
            "--points",
            "1024",
            "--dims",
            "8",
            "--k",
            "10",
            "--rounds",
            "10",
        ],
        "field": "knn_ms",
        "rounds": 10,
    },
]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-bin", required=True, help="baseline tritdb-bench executable")
    parser.add_argument(
        "--current-bin",
        default="target/release/tritdb-bench.exe",
        help="current tritdb-bench executable",
    )
    parser.add_argument("--pairs", type=int, default=20, help="number of interleaved pairs")
    parser.add_argument(
        "--cases",
        default="",
        help="comma-separated bench ids (default: all comparable cases)",
    )
    parser.add_argument("--out", default=DEFAULT_OUT)
    parser.add_argument("--baseline-commit", default="3e90ba0")
    parser.add_argument("--current-commit", default="")
    return parser.parse_args()


def platform_tag() -> str:
    system = host_platform.system().lower()
    system = {"windows": "windows", "darwin": "darwin", "linux": "linux"}.get(system, system)
    machine = host_platform.machine().lower()
    machine = {
        "amd64": "x86_64",
        "x86_64": "x86_64",
        "arm64": "arm64",
        "aarch64": "arm64",
    }.get(machine, machine)
    return f"{system}-{machine}"


def git_commit() -> str:
    completed = subprocess.run(
        ["git", "rev-parse", "--short", "HEAD"],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    return completed.stdout.strip() if completed.returncode == 0 else ""


def percentile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    weight = position - lower
    return ordered[lower] * (1.0 - weight) + ordered[upper] * weight


def run_case(binary: Path, case: dict) -> dict:
    completed = subprocess.run(
        [str(binary), *case["args"]],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
    )
    if completed.returncode != 0:
        raise SystemExit(
            f"{binary.name} {case['bench_id']} exited {completed.returncode}: "
            f"{completed.stderr.strip()}"
        )
    found_rss: int | None = None
    for line in reversed(completed.stdout.splitlines()):
        if not line.startswith("{"):
            continue
        payload = json.loads(line)
        rss_kb = payload.get("rss_kb")
        if found_rss is None and isinstance(rss_kb, (int, float)):
            found_rss = int(rss_kb)
        sample = payload.get(case["field"])
        if isinstance(sample, list) and len(sample) >= 2:
            values = [float(value) for value in sample]
            median = float(statistics.median(values))
            return {
                "median": median,
                "samples": values,
                "rss_kb": found_rss,
            }
    raise SystemExit(f"{binary.name} {case['bench_id']}: no JSON median in output")


def median(values: list[float]) -> float:
    return float(statistics.median(values))


def classify(delta: float) -> str:
    if abs(delta) < 5.0:
        return "持平"
    if delta <= -20.0:
        return "大幅提升"
    if delta < -5.0:
        return "提升"
    if delta >= 20.0:
        return "明显退化"
    return "退化"


def main() -> int:
    args = parse_args()
    if args.pairs < 1:
        raise SystemExit("--pairs must be >= 1")
    baseline_bin = Path(args.baseline_bin).resolve()
    current_bin = Path(args.current_bin).resolve()
    for binary in (baseline_bin, current_bin):
        if not binary.is_file():
            raise SystemExit(f"missing benchmark binary: {binary}")

    selected = CASES
    if args.cases:
        wanted = {item.strip() for item in args.cases.split(",") if item.strip()}
        selected = [case for case in CASES if case["bench_id"] in wanted]
        missing = wanted - {case["bench_id"] for case in selected}
        if missing:
            raise SystemExit(f"unknown cases: {', '.join(sorted(missing))}")

    current_commit = args.current_commit or git_commit()
    cases = []
    for case in selected:
        baseline_values: list[float] = []
        current_values: list[float] = []
        baseline_samples: list[float] = []
        current_samples: list[float] = []
        baseline_rss: list[int] = []
        current_rss: list[int] = []
        pair_deltas: list[float] = []
        current_faster = baseline_faster = ties = 0
        for pair in range(args.pairs):
            order = (("base", baseline_bin), ("cur", current_bin))
            if pair % 2:
                order = order[::-1]
            for side, binary in order:
                result = run_case(binary, case)
                value = result["median"]
                if side == "base":
                    baseline_values.append(value)
                    baseline_samples.extend(result["samples"])
                    if result["rss_kb"] is not None:
                        baseline_rss.append(result["rss_kb"])
                else:
                    current_values.append(value)
                    current_samples.extend(result["samples"])
                    if result["rss_kb"] is not None:
                        current_rss.append(result["rss_kb"])
            if current_values[-1] < baseline_values[-1]:
                current_faster += 1
            elif current_values[-1] > baseline_values[-1]:
                baseline_faster += 1
            else:
                ties += 1
            pair_deltas.append(
                (current_values[-1] - baseline_values[-1]) / baseline_values[-1] * 100.0
            )
        base_ms = median(baseline_values)
        cur_ms = median(current_values)
        delta = (cur_ms - base_ms) / base_ms * 100.0 if base_ms else 0.0
        cases.append(
            {
                "bench_id": case["bench_id"],
                "pairs": args.pairs,
                "rounds": case["rounds"],
                "args": case["args"],
                "baseline_ms": round(base_ms, 4),
                "current_ms": round(cur_ms, 4),
                "delta_pct": round(delta, 2),
                "status": classify(delta),
                "current_faster": current_faster,
                "baseline_faster": baseline_faster,
                "ties": ties,
                "baseline_p50_ms": percentile(baseline_values, 0.50),
                "baseline_p90_ms": percentile(baseline_values, 0.90),
                "current_p50_ms": percentile(current_values, 0.50),
                "current_p90_ms": percentile(current_values, 0.90),
                "pair_delta_p50_pct": percentile(pair_deltas, 0.50),
                "pair_delta_p90_pct": percentile(pair_deltas, 0.90),
                "baseline_samples": baseline_values,
                "current_samples": current_values,
                "baseline_raw_samples": baseline_samples,
                "current_raw_samples": current_samples,
                "baseline_rss_kb": baseline_rss,
                "current_rss_kb": current_rss,
            }
        )
        print(
            f"{case['bench_id']:<16} {base_ms:10.4f} -> {cur_ms:10.4f}  "
            f"{delta:+6.2f}%  {classify(delta)}  "
            f"(cur faster {current_faster}/{args.pairs})"
        )

    payload = {
        "schema": 2,
        "generated_at": datetime.now().astimezone().isoformat(timespec="seconds"),
        "platform": platform_tag(),
        "baseline_commit": args.baseline_commit,
        "current_commit": current_commit,
        "method": (
            "同一时间窗内基线/当前二进制交替运行；每对先跑基线再跑当前，"
            "下一对交换顺序；取每侧 median-of-medians；同时保留每对中位数、"
            "每轮原始样本和进程 RSS"
        ),
        "pairs": args.pairs,
        "cases": cases,
    }
    out = ROOT / args.out
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
