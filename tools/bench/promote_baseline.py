#!/usr/bin/env python3
"""Promote repeated benchmark runs into the committed baseline.

A single `tritdb-bench record` run is noisy on Windows (filesystem/AV stalls),
so the baseline is the median of several runs of the same commit.  `min_ms` is
the smallest observed min, `max_ms` the largest observed max, and `median_ms`
the median of the per-run medians — exactly the numbers `tritdb-bench check`
compares against.

Usage:
    python tools/bench/promote_baseline.py --commit 3e90ba0
    python tools/bench/promote_baseline.py --commit HEAD --aggregate 5
"""

from __future__ import annotations

import argparse
import json
import statistics
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs-dir", default="docs/results/bench-runs")
    parser.add_argument("--out", default="docs/results/bench-baseline.json")
    parser.add_argument("--markdown", default="docs/results/BENCH.md")
    parser.add_argument("--commit", default="", help="only use runs of this commit (short or full)")
    parser.add_argument("--platform", default="windows-x86_64")
    parser.add_argument("--aggregate", type=int, default=5)
    return parser.parse_args()


def load_runs(directory: Path) -> list[dict]:
    runs = []
    for path in sorted(directory.glob("*.json")):
        if path.name.startswith("."):
            continue
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        if isinstance(data.get("cases"), list):
            data["_file"] = path.name
            runs.append(data)
    return runs


def sort_key(run: dict) -> tuple[str, str]:
    return (str(run.get("generated_at") or ""), str(run.get("_file", "")))


def main() -> int:
    args = parse_args()
    runs_dir = ROOT / args.runs_dir
    runs = load_runs(runs_dir)
    selected = [run for run in runs if run.get("platform") == args.platform]
    if args.commit:
        wanted = args.commit
        selected = [
            run for run in selected
            if str(run.get("commit") or "").startswith(wanted)
            or str(run.get("commit_full") or "") == wanted
        ]
    selected.sort(key=sort_key)
    selected = selected[-max(1, args.aggregate):]
    if not selected:
        raise SystemExit("no matching runs")

    per_case: dict[str, dict] = {}
    for run in selected:
        for case in run.get("cases", []):
            bench_id = case.get("bench_id")
            if not isinstance(bench_id, str):
                continue
            entry = per_case.setdefault(
                bench_id,
                {"args": str(case.get("args") or ""), "medians": [], "mins": [], "maxs": []},
            )
            entry["medians"].append(float(case["median_ms"]))
            entry["mins"].append(float(case["min_ms"]))
            entry["maxs"].append(float(case["max_ms"]))

    cases = []
    for bench_id, entry in per_case.items():
        cases.append(
            {
                "bench_id": bench_id,
                "args": entry["args"],
                "min_ms": min(entry["mins"]),
                "median_ms": statistics.median(entry["medians"]),
                "max_ms": max(entry["maxs"]),
            }
        )

    last = selected[-1]
    baseline = {
        "format": 1,
        "suite": str(last.get("suite") or "ci"),
        "platform": str(last.get("platform") or args.platform),
        "repeats": len(selected),
        "aggregate": "median-of-medians",
        "commit": str(last.get("commit") or ""),
        "recorded_at": datetime.now().astimezone().replace(microsecond=0).isoformat(),
        "cases": cases,
    }
    (ROOT / args.out).write_text(json.dumps(baseline, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    if args.markdown:
        lines = []
        lines.append("# trit-db 基准基线")
        lines.append("")
        lines.append(
            f"由 `tools/bench/promote_baseline.py` 从 **{len(selected)} 次**同 commit 运行聚合；"
            "CI 用 `tritdb-bench check` 与它比较。"
        )
        lines.append("")
        lines.append(f"- 平台：`{baseline['platform']}`")
        lines.append(f"- 套件：`{baseline['suite']}`（小规模、秒级；不代表性能上限，只用于捕获数量级回归）")
        lines.append(f"- 基线 commit：`{baseline['commit']}`")
        lines.append(f"- 口径：每项 `median_ms` 取 {len(selected)} 次运行 median 的中位数；`min_ms` 取最小值，`max_ms` 取最大值")
        lines.append("")
        lines.append("| case | 参数 | min (ms) | median (ms) | max (ms) |")
        lines.append("|---|---|---:|---:|---:|")
        for case in cases:
            lines.append(
                f"| `{case['bench_id']}` | `{case['args']}` | {case['min_ms']:.4f} | "
                f"{case['median_ms']:.4f} | {case['max_ms']:.4f} |"
            )
        lines.append("")
        lines.append("## 如何更新基线")
        lines.append("")
        lines.append("```bash")
        lines.append("# 在当前 commit 上跑 5 次，再把 5 次的中位数提升为基线")
        lines.append("python tools/bench/run.py --repeat 5 --no-dashboard")
        lines.append("python tools/bench/promote_baseline.py --aggregate 5")
        lines.append("```")
        lines.append("")
        lines.append("## CI 门限")
        lines.append("")
        lines.append("```bash")
        lines.append("cargo run -p tritdb-bench --release -- check \\")
        lines.append("  --baseline docs/results/bench-baseline.json \\")
        lines.append("  --tolerance 2.0 --min-ms 0.05")
        lines.append("```")
        lines.append("")
        lines.append("共享 runner 噪声大，CI 只拦“超过基线 3 倍”的数量级回归；本地可用更紧的 `--tolerance 0.5`。")
        (ROOT / args.markdown).write_text("\n".join(lines) + "\n", encoding="utf-8")

    print(
        f"baseline: {len(cases)} cases from {len(selected)} runs of "
        f"{baseline['commit'] or 'unknown'} -> {args.out}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
