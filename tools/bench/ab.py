#!/usr/bin/env python3
"""Interleaved A/B benchmark for cases whose deltas are dominated by I/O jitter.

Sequential baseline/current batches drift with disk cache, Defender and power
state.  This runner alternates the two binaries in the same time window, uses
more rounds for sub-10 ms cases, and reports paired medians.  The output feeds
the "A/B 复核" table in the dashboard.

Writes docs/results/bench-ab.json (stdlib only).
"""

from __future__ import annotations

import argparse
import json
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
        "bench_id": "txn_put",
        "args": ["txn-put", "--txns", "64", "--keys-per-txn", "4", "--rounds", "5"],
        "field": "put_ms",
        "rounds": 5,
    },
    {
        "bench_id": "sql_update",
        "args": ["sql-update", "--rows", "512", "--cutoff", "64", "--rounds", "20"],
        "field": "update_ms",
        "rounds": 20,
    },
    {
        "bench_id": "sql_delete",
        "args": ["sql-delete", "--rows", "512", "--cutoff", "64", "--rounds", "20"],
        "field": "delete_ms",
        "rounds": 20,
    },
    {
        "bench_id": "sql_insert",
        "args": ["sql-insert", "--rows", "128", "--rounds", "20"],
        "field": "insert_ms",
        "rounds": 20,
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


def run_case(binary: Path, case: dict) -> float:
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
    for line in reversed(completed.stdout.splitlines()):
        if line.startswith("{"):
            payload = json.loads(line)
            sample = payload.get(case["field"])
            if isinstance(sample, list) and len(sample) >= 2:
                return float(sample[1])
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

    current_commit = args.current_commit or git_commit()
    cases = []
    for case in CASES:
        baseline_values: list[float] = []
        current_values: list[float] = []
        current_faster = baseline_faster = ties = 0
        for pair in range(args.pairs):
            order = (("base", baseline_bin), ("cur", current_bin))
            if pair % 2:
                order = order[::-1]
            for side, binary in order:
                value = run_case(binary, case)
                if side == "base":
                    baseline_values.append(value)
                else:
                    current_values.append(value)
            if current_values[-1] < baseline_values[-1]:
                current_faster += 1
            elif current_values[-1] > baseline_values[-1]:
                baseline_faster += 1
            else:
                ties += 1
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
            }
        )
        print(
            f"{case['bench_id']:<12} {base_ms:10.4f} -> {cur_ms:10.4f}  "
            f"{delta:+6.2f}%  {classify(delta)}  "
            f"(cur faster {current_faster}/{args.pairs})"
        )

    payload = {
        "schema": 1,
        "generated_at": datetime.now().astimezone().isoformat(timespec="seconds"),
        "platform": platform_tag(),
        "baseline_commit": args.baseline_commit,
        "current_commit": current_commit,
        "method": (
            "同一时间窗内基线/当前二进制交替运行；每对先跑基线再跑当前，"
            "下一对交换顺序；取每侧 median-of-medians"
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
