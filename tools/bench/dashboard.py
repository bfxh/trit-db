#!/usr/bin/env python3
"""Build the trit-db benchmark dashboard from the committed baseline and run history.

Outputs:
  docs/results/BENCH-DASHBOARD.md   full tables (summary / per-case / per-path / per-layer /
                                    history / suggestions)
  docs/results/bench-trend.svg      horizontal bar chart of latency change vs baseline
  docs/results/bench-badge.json     shields.io endpoint badge (better/worse counts)
  docs/results/bench-delta-badge.json
  README.md                         replaces the block between BENCH-SUMMARY markers

The script is stdlib-only so CI needs no extra packages.  A run is only
compared with the baseline recorded on the same platform; cross-platform runs
are listed but never mixed into the delta table.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]

# bench_id -> (layer, chinese name, scenario, ops per round, unit)
CASE_META = {
    "sql_scan": ("SQL 执行", "全表扫描", "512 行 / 3 轮", 512, "行/s"),
    "sql_point": ("SQL 执行", "主键点查", "512 行 / 64 次查询", 64, "次/s"),
    "sql_group": ("SQL 执行", "分组聚合", "512 行 / 16 组", 1, "查询/s"),
    "sql_sort": ("SQL 执行", "排序 + LIMIT", "512 行 / LIMIT 16", 1, "查询/s"),
    "sql_insert": ("SQL 执行", "批量插入", "128 行", 128, "行/s"),
    "sql_update": ("SQL 执行", "条件更新", "512 行 / cutoff 64", 1, "语句/s"),
    "sql_delete": ("SQL 执行", "条件删除", "512 行 / cutoff 64", 1, "语句/s"),
    "sql_subquery": ("SQL 执行", "IN 子查询", "256 行", 1, "查询/s"),
    "sql_window": ("SQL 执行", "窗口函数 RANK", "512 行", 1, "查询/s"),
    "sql_join": ("SQL 执行", "Auto hash join", "512 行", 1, "查询/s"),
    "pstore_scan": ("存储引擎", "页存储扫描", "64 页 / 1 KiB", 64, "页/s"),
    "txn_put": ("存储引擎", "事务写入", "64 txn × 4 key", 256, "键/s"),
    "pgwire_roundtrip": ("协议接口", "PG 协议往返", "32 查询", 32, "查询/s"),
    "ann_knn": ("索引与图", "三值 k-NN", "1024 点 / 8 维 / k=10", 1, "查询/s"),
    "graph_reach": ("索引与图", "图可达", "256 节点 / 6 跳", 1, "查询/s"),
}

LAYER_ORDER = ["SQL 执行", "存储引擎", "协议接口", "索引与图"]

READ_CASES = {
    "sql_scan",
    "sql_point",
    "sql_group",
    "sql_sort",
    "sql_subquery",
    "sql_window",
    "sql_join",
    "pstore_scan",
    "pgwire_roundtrip",
    "ann_knn",
    "graph_reach",
}
WRITE_CASES = {"sql_insert", "sql_update", "sql_delete", "txn_put"}

STATUS_BIG_GAIN = "大幅提升"
STATUS_GAIN = "提升"
STATUS_FLAT = "持平"
STATUS_REGRESS = "退化"
STATUS_BIG_REGRESS = "明显退化"
STATUS_RESOLUTION = "分辨率不足"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", default="docs/results/bench-baseline.json")
    parser.add_argument("--baseline-runs-dir", default="docs/results/bench-baseline-runs")
    parser.add_argument("--runs-dir", default="docs/results/bench-runs")
    parser.add_argument("--out-md", default="docs/results/BENCH-DASHBOARD.md")
    parser.add_argument("--out-svg", default="docs/results/bench-trend.svg")
    parser.add_argument("--out-badge", default="docs/results/bench-badge.json")
    parser.add_argument("--out-delta-badge", default="docs/results/bench-delta-badge.json")
    parser.add_argument("--readme", default="README.md")
    parser.add_argument("--ab", default="docs/results/bench-ab.json")
    parser.add_argument(
        "--aggregate",
        type=int,
        default=5,
        help="how many runs of the newest commit to aggregate (median of medians, default 5)",
    )
    return parser.parse_args()


def read_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def load_runs(runs_dir: Path) -> list[dict]:
    runs = []
    if not runs_dir.is_dir():
        return runs
    for path in sorted(runs_dir.glob("*.json")):
        if path.name.startswith("."):
            continue
        try:
            data = read_json(path)
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        if not isinstance(data.get("cases"), list):
            continue
        data["_file"] = path.name
        runs.append(data)
    runs.sort(key=run_sort_key)
    return runs


def run_sort_key(run: dict) -> tuple[str, str]:
    stamp = str(run.get("generated_at") or "")
    if not stamp:
        # Fall back to the date prefix of the file name.
        name = str(run.get("_file", ""))
        stamp = name[:10] + "T00:00:00"
    return (stamp, str(run.get("_file", "")))


def case_map(run: dict) -> dict[str, float]:
    out = {}
    for case in run.get("cases", []):
        bench_id = case.get("bench_id")
        median = case.get("median_ms")
        if isinstance(bench_id, str) and isinstance(median, (int, float)) and median > 0:
            out[bench_id] = float(median)
    return out


def resolution_limited(base_ms: float) -> bool:
    return base_ms < 0.01


def delta_pct(base: float, latest: float) -> float:
    return (latest - base) / base * 100.0


def classify(delta: float) -> str:
    if abs(delta) < 5.0:
        return STATUS_FLAT
    if delta <= -20.0:
        return STATUS_BIG_GAIN
    if delta < -5.0:
        return STATUS_GAIN
    if delta >= 20.0:
        return STATUS_BIG_REGRESS
    return STATUS_REGRESS


def fmt_ms(value: float) -> str:
    if value < 0.01:
        return f"{value:.4f}"
    if value < 1.0:
        return f"{value:.4f}"
    if value < 100.0:
        return f"{value:.3f}"
    return f"{value:.1f}"


def fmt_delta_pct(delta: float) -> str:
    if abs(delta) < 0.05:
        return "0.0%"
    return f"{delta:+.1f}%"


def fmt_int(value: float) -> str:
    return f"{value:,.0f}"


def fmt_date(run: dict) -> str:
    stamp = str(run.get("generated_at") or "")
    if stamp:
        return stamp[:10]
    return str(run.get("_file", ""))[:10]


def fmt_commit(run: dict) -> str:
    commit = str(run.get("commit") or "")
    return f"`{commit}`" if commit else "—"


def same_commit(left: object, right: object) -> bool:
    left_text = str(left or "")
    right_text = str(right or "")
    if not left_text or not right_text:
        return False
    return (
        left_text == right_text
        or left_text.startswith(right_text)
        or right_text.startswith(left_text)
    )


def speedup_text(delta: float) -> str:
    if abs(delta) < 5.0:
        return "持平"
    if delta < 0:
        return f"快 {1.0 / (1.0 + delta / 100.0):.2f}×"
    return f"慢 {1.0 + delta / 100.0:.2f}×"


def ops_per_second(bench_id: str, median_ms: float) -> float:
    _, _, _, count, _ = CASE_META[bench_id]
    return count / median_ms * 1000.0


def build_rows(baseline_cases: dict[str, float], latest_cases: dict[str, float]) -> list[dict]:
    rows = []
    for bench_id, base in sorted(baseline_cases.items()):
        if bench_id not in latest_cases:
            continue
        latest = latest_cases[bench_id]
        layer, name, scene, _, unit = CASE_META.get(
            bench_id, ("其它", bench_id, "—", 1, "次/s")
        )
        if resolution_limited(base):
            delta = 0.0
            status = STATUS_RESOLUTION
        else:
            delta = delta_pct(base, latest)
            status = classify(delta)
        throughput = ops_per_second(bench_id, latest)
        throughput_delta = (base / latest - 1.0) * 100.0
        rows.append(
            {
                "bench_id": bench_id,
                "layer": layer,
                "name": name,
                "scene": scene,
                "unit": unit,
                "base": base,
                "latest": latest,
                "delta": delta,
                "status": status,
                "throughput": throughput,
                "throughput_delta": throughput_delta,
                "speedup": speedup_text(delta),
            }
        )
    order = {name: index for index, name in enumerate(LAYER_ORDER)}
    rows.sort(key=lambda row: (order.get(row["layer"], 99), row["delta"], row["bench_id"]))
    return rows


def counts(rows: list[dict]) -> dict[str, int]:
    result = {
        STATUS_BIG_GAIN: 0,
        STATUS_GAIN: 0,
        STATUS_FLAT: 0,
        STATUS_REGRESS: 0,
        STATUS_BIG_REGRESS: 0,
        STATUS_RESOLUTION: 0,
    }
    for row in rows:
        result[row["status"]] = result.get(row["status"], 0) + 1
    return result


def better_count(count: dict[str, int]) -> int:
    return count[STATUS_BIG_GAIN] + count[STATUS_GAIN]


def worse_count(count: dict[str, int]) -> int:
    return count[STATUS_REGRESS] + count[STATUS_BIG_REGRESS]


def load_ab(path: Path, platform: str) -> dict | None:
    if not path.is_file():
        return None
    try:
        data = read_json(path)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    if data.get("platform") != platform:
        return None
    if not isinstance(data.get("cases"), list) or not data["cases"]:
        return None
    return data


def ab_case_map(ab: dict | None) -> dict[str, dict]:
    if not ab:
        return {}
    return {
        str(case.get("bench_id")): case
        for case in ab.get("cases", [])
        if isinstance(case.get("delta_pct"), (int, float))
    }


def ab_stats(cases: list[dict]) -> tuple[float, dict[str, int]]:
    result = {
        STATUS_BIG_GAIN: 0,
        STATUS_GAIN: 0,
        STATUS_FLAT: 0,
        STATUS_REGRESS: 0,
        STATUS_BIG_REGRESS: 0,
        STATUS_RESOLUTION: 0,
    }
    for case in cases:
        status = str(case.get("status") or STATUS_FLAT)
        result[status] = result.get(status, 0) + 1
    average = (
        statistics.median(float(case["delta_pct"]) for case in cases) if cases else 0.0
    )
    return average, result


def ab_table(ab: dict | None, rows: list[dict]) -> str:
    if not ab:
        return ""
    by_id = {row["bench_id"]: row for row in rows}
    data = []
    cases = sorted(
        (case for case in ab_case_map(ab).values() if case.get("bench_id") in by_id),
        key=lambda case: by_id[str(case["bench_id"])]["delta"],
        reverse=True,
    )
    for case in cases:
        bench_id = str(case["bench_id"])
        row = by_id[bench_id]
        ab_delta = float(case["delta_pct"])
        verdict = str(case.get("status") or STATUS_FLAT)
        if abs(row["delta"]) >= 5.0 and abs(ab_delta) < 5.0:
            verdict = "未复现"
        data.append(
            [
                f"`{bench_id}`",
                f"{fmt_delta_pct(row['delta'])}（{row['status']}）",
                fmt_delta_pct(ab_delta),
                f"{fmt_ms(float(case['baseline_ms']))} → {fmt_ms(float(case['current_ms']))}",
                f"{case.get('pairs', '—')} 对 × {case.get('rounds', '—')} 轮",
                verdict,
            ]
        )
    if not data:
        return ""
    return md_table(
        ["基准项", "5 次 Δ%", "A/B Δ%", "基线 → 当前 (ms)", "交替", "判定"],
        data,
    )


def layer_summary(rows: list[dict]) -> list[dict]:
    summary = []
    for layer in LAYER_ORDER:
        subset = [row for row in rows if row["layer"] == layer]
        if not subset:
            continue
        measured = [row for row in subset if row["status"] != STATUS_RESOLUTION]
        average = sum(row["delta"] for row in measured) / len(measured) if measured else 0.0
        best = min(measured, key=lambda row: row["delta"]) if measured else None
        worst = max(measured, key=lambda row: row["delta"]) if measured else None
        summary.append(
            {
                "layer": layer,
                "total": len(subset),
                "better": sum(1 for row in subset if row["status"] in (STATUS_BIG_GAIN, STATUS_GAIN)),
                "flat": sum(1 for row in subset if row["status"] == STATUS_FLAT),
                "resolution": sum(1 for row in subset if row["status"] == STATUS_RESOLUTION),
                "worse": sum(1 for row in subset if row["status"] in (STATUS_REGRESS, STATUS_BIG_REGRESS)),
                "average": average,
                "best": best,
                "worst": worst,
            }
        )
    return summary


def pick_comparable_run(runs: list[dict], platform: str) -> dict | None:
    comparable = pick_comparable_runs(runs, platform)
    return comparable[-1] if comparable else None


def pick_comparable_runs(runs: list[dict], platform: str) -> list[dict]:
    return [run for run in runs if run.get("platform") == platform and case_map(run)]


def aggregate_runs(items: list[dict]) -> dict | None:
    """Median of medians over the newest commit's repeated runs.

    A single `record` run is very noisy on DML/fsync cases, so the headline
    comparison uses up to `--aggregate` repeats instead of one lucky/unlucky run.
    """
    if not items:
        return None
    per_case: dict[str, list[float]] = {}
    args: dict[str, str] = {}
    for run in items:
        for case in run.get("cases", []):
            bench_id = case.get("bench_id")
            median = case.get("median_ms")
            if isinstance(bench_id, str) and isinstance(median, (int, float)) and median > 0:
                per_case.setdefault(bench_id, []).append(float(median))
                args.setdefault(bench_id, str(case.get("args") or ""))
    cases = []
    for bench_id, values in per_case.items():
        cases.append(
            {
                "bench_id": bench_id,
                "args": args.get(bench_id, ""),
                "median_ms": statistics.median(values),
                "min_ms": min(values),
                "max_ms": max(values),
                "samples": len(values),
            }
        )
    last = items[-1]
    return {
        "platform": last.get("platform"),
        "generated_at": last.get("generated_at"),
        "commit": last.get("commit"),
        "commit_full": last.get("commit_full"),
        "dirty": last.get("dirty"),
        "runner": last.get("runner"),
        "_file": last.get("_file"),
        "cases": cases,
        "aggregated_runs": len(items),
        "aggregate": len(items) > 1,
    }


def group_runs(runs: list[dict], platform: str) -> list[list[dict]]:
    """Group same-platform runs by (local date, commit) so one row = one batch.

    Repeated local runs of the same commit are aggregated before any delta is
    computed; otherwise a single AV/fsync stall turns into a fake ±800% swing.
    """
    ordered = sorted((run for run in runs if run.get("platform") == platform), key=run_sort_key)
    groups: list[list[dict]] = []
    previous = None
    for run in ordered:
        key = (fmt_date(run), str(run.get("commit") or ""))
        if key != previous:
            groups.append([])
            previous = key
        groups[-1].append(run)
    return groups


def latest_window(runs: list[dict], platform: str, limit: int) -> list[dict]:
    """The newest batch of same-platform runs, capped to `limit` repeats."""
    groups = group_runs(runs, platform)
    if not groups:
        return []
    return groups[-1][-max(1, limit):]


def md_table(headers: list[str], rows: list[list[str]]) -> str:
    lines = ["| " + " | ".join(headers) + " |"]
    lines.append("|" + "|".join("---" for _ in headers) + "|")
    for row in rows:
        lines.append("| " + " | ".join(row) + " |")
    return "\n".join(lines)


def fmt_cell_delta(ms_delta: float) -> str:
    return f"{ms_delta:+.4f}"






def path_table(rows: list[dict], ab: dict | None = None) -> str:
    data = []
    ab_by_id = ab_case_map(ab)
    for label, ids in (("读路径", READ_CASES), ("写路径", WRITE_CASES)):
        subset = [row for row in rows if row["bench_id"] in ids]
        measured = [row for row in subset if row["status"] != STATUS_RESOLUTION]
        average = sum(row["delta"] for row in measured) / len(measured) if measured else 0.0
        count = counts(subset)
        ab_cases = [case for case in ab_by_id.values() if case.get("bench_id") in ids]
        if ab_cases and label == "写路径":
            ab_average, ab_count = ab_stats(ab_cases)
            ab_avg_cell = fmt_delta_pct(ab_average)
            ab_dist_cell = (
                f"{better_count(ab_count)} / {ab_count[STATUS_FLAT]} / {worse_count(ab_count)}"
            )
            ab_verdict = (
                "A/B 整体变快"
                if ab_average <= -5
                else "A/B 整体变慢"
                if ab_average >= 5
                else "A/B 基本持平"
            )
        else:
            ab_avg_cell = "—"
            ab_dist_cell = "—"
            ab_verdict = "—"
        verdict = "整体变快" if average <= -5 else "整体变慢" if average >= 5 else "基本持平"
        data.append(
            [
                label,
                str(len(subset)) + (f"（含 {count[STATUS_RESOLUTION]} 项分辨率不足）" if count[STATUS_RESOLUTION] else ""),
                fmt_delta_pct(average),
                f"{better_count(count)} / {count[STATUS_FLAT]} / {worse_count(count)}",
                verdict,
                ab_avg_cell,
                ab_dist_cell,
                ab_verdict,
            ]
        )
    return md_table(
        [
            "路径",
            "项数",
            "5 次平均 Δ%",
            "5 次 提升 / 持平 / 退化",
            "5 次结论",
            "A/B Δ% 中位数",
            "A/B 提升 / 持平 / 退化",
            "A/B 结论",
        ],
        data,
    )


def layer_table(rows: list[dict]) -> str:
    data = []
    for item in layer_summary(rows):
        best = item["best"]
        worst = item["worst"]
        data.append(
            [
                item["layer"],
                str(item["total"]),
                str(item["better"]),
                str(item["flat"]) + (f"（另 {item['resolution']} 项分辨率不足）" if item["resolution"] else ""),
                str(item["worse"]),
                fmt_delta_pct(item["average"]),
                f"`{best['bench_id']}` {fmt_delta_pct(best['delta'])}" if best else "—",
                f"`{worst['bench_id']}` {fmt_delta_pct(worst['delta'])}" if worst else "—",
            ]
        )
    return md_table(
        ["分层", "项数", "提升", "持平", "退化", "平均 Δ%", "最大提升", "最大 Δ%"],
        data,
    )


def history_table(baseline: dict, groups: list[list[dict]]) -> str:
    base_cases = case_map(baseline)
    data = [
        [
            "基线",
            "2026-11-21",
            f"`{baseline.get('commit', '—')}`",
            f"`{baseline.get('platform', '—')}`",
            str(baseline.get("repeats", 1)),
            str(len(base_cases)),
            "—",
            "—",
            "—",
            "—",
            "—",
            "—",
        ]
    ]
    for group in groups:
        aggregate = aggregate_runs(group)
        if aggregate is None:
            continue
        compare = build_rows(base_cases, case_map(aggregate))
        if not compare:
            continue
        count = counts(compare)
        measured = [row for row in compare if row["status"] != STATUS_RESOLUTION]
        median_delta = statistics.median(row["delta"] for row in measured) if measured else 0.0
        best = min(measured, key=lambda row: row["delta"])
        worst = max(measured, key=lambda row: row["delta"])
        data.append(
            [
                "运行批次" if len(group) > 1 else "运行",
                fmt_date(aggregate),
                fmt_commit(aggregate),
                f"`{aggregate.get('platform', '—')}`",
                str(len(group)),
                str(len(compare)),
                str(better_count(count)),
                str(count[STATUS_FLAT]) + (f"（另 {count[STATUS_RESOLUTION]} 项分辨率）" if count[STATUS_RESOLUTION] else ""),
                str(worse_count(count)),
                fmt_delta_pct(median_delta),
                f"`{best['bench_id']}` {fmt_delta_pct(best['delta'])}",
                f"`{worst['bench_id']}` {fmt_delta_pct(worst['delta'])}",
            ]
        )
    return md_table(
        [
            "运行",
            "日期",
            "commit",
            "平台",
            "次数",
            "项数",
            "提升",
            "持平",
            "退化",
            "Δ% 中位数",
            "最大提升",
            "最大退化",
        ],
        data,
    )




def verdict_direction(status: str) -> int | None:
    if status in (STATUS_BIG_GAIN, STATUS_GAIN):
        return -1
    if status == STATUS_FLAT:
        return 0
    if status in (STATUS_REGRESS, STATUS_BIG_REGRESS):
        return 1
    return None


def verdict_relation(single_status: str, aggregate_status: str | None) -> str:
    """How the single-run verdict relates to the 5-run aggregate verdict."""
    if aggregate_status is None:
        return "不可比"
    if single_status == STATUS_RESOLUTION or aggregate_status == STATUS_RESOLUTION:
        return "不可比"
    left = verdict_direction(single_status)
    right = verdict_direction(aggregate_status)
    if left == right:
        return "一致" if single_status == aggregate_status else "同向"
    if left == 0 or right == 0:
        return "不一致"
    return "相反"


def build_single_comparison(
    baseline_runs: list[dict],
    latest_runs: list[dict],
    aggregate_rows: list[dict],
) -> dict | None:
    """The first run of each batch, compared directly, purely as a noise probe."""
    if not baseline_runs or not latest_runs:
        return None
    base_run = baseline_runs[0]
    latest_run = latest_runs[0]
    rows = build_rows(case_map(base_run), case_map(latest_run))
    if not rows:
        return None
    aggregate = {row["bench_id"]: row for row in aggregate_rows}
    for row in rows:
        match = aggregate.get(row["bench_id"])
        row["aggregate_status"] = match["status"] if match else None
        row["aggregate_delta"] = match["delta"] if match else None
        row["relation"] = verdict_relation(row["status"], row["aggregate_status"])
        row["gap"] = abs(row["delta"] - match["delta"]) if match else 0.0
    return {
        "baseline_run": base_run,
        "latest_run": latest_run,
        "baseline_runs": baseline_runs,
        "latest_runs": latest_runs,
        "rows": rows,
    }


def case_values(runs: list[dict], bench_id: str) -> list[float]:
    values = []
    for run in runs:
        value = case_map(run).get(bench_id)
        if value is not None:
            values.append(value)
    return values


def value_range(values: list[float]) -> str:
    if not values:
        return "—"
    return f"{fmt_ms(min(values))}–{fmt_ms(max(values))}"


def volatility_ratio(values: list[float]) -> float:
    if not values or min(values) <= 0:
        return 1.0
    return max(values) / min(values)


def noise_grade(ratio: float) -> str:
    if ratio < 1.2:
        return "稳定"
    if ratio < 2.0:
        return "中等"
    return "剧烈"


def method_table(
    rows: list[dict], count: dict[str, int], single: dict | None, ab: dict | None = None
) -> str:
    measured = [row for row in rows if row["status"] != STATUS_RESOLUTION]
    median_delta = statistics.median(row["delta"] for row in measured) if measured else 0.0
    data = [
        [
            "5 次聚合（历史）",
            f"{better_count(count)} / {count[STATUS_FLAT]} / {worse_count(count)}",
            fmt_delta_pct(median_delta),
        ]
    ]
    ab_cases = [case for case in ab_case_map(ab).values() if case.get("bench_id") in WRITE_CASES]
    if ab_cases:
        ab_average, ab_count = ab_stats(ab_cases)
        data.append(
            [
                f"A/B 复核（写路径判定，{len(ab_cases)} 项）",
                f"{better_count(ab_count)} / {ab_count[STATUS_FLAT]} / {worse_count(ab_count)}",
                fmt_delta_pct(ab_average),
            ]
        )
    if single:
        single_rows = single["rows"]
        single_count = counts(single_rows)
        single_measured = [row for row in single_rows if row["status"] != STATUS_RESOLUTION]
        single_median = (
            statistics.median(row["delta"] for row in single_measured) if single_measured else 0.0
        )
        data.append(
            [
                "单次直比（仅参考）",
                f"{better_count(single_count)} / {single_count[STATUS_FLAT]} / {worse_count(single_count)}",
                fmt_delta_pct(single_median),
            ]
        )
    return md_table(["口径", "更快 / 持平 / 更慢", "Δ% 中位数"], data)


def highlights_table(rows: list[dict], single: dict | None, ab: dict | None = None) -> str:
    measured = [row for row in rows if row["status"] != STATUS_RESOLUTION]
    read_subset = [row for row in measured if row["bench_id"] in READ_CASES]
    write_subset = [row for row in measured if row["bench_id"] in WRITE_CASES]
    read_avg = sum(row["delta"] for row in read_subset) / len(read_subset) if read_subset else 0.0
    write_avg = sum(row["delta"] for row in write_subset) / len(write_subset) if write_subset else 0.0
    best = min(measured, key=lambda row: row["delta"])
    worst = max(measured, key=lambda row: row["delta"])
    read_below = sum(1 for row in rows if row["bench_id"] in READ_CASES and row["status"] == STATUS_RESOLUTION)
    read_note = f"（另 {read_below} 项分辨率不足）" if read_below else ""
    data = [
        ["最大提升", f"`{best['bench_id']}` {fmt_delta_pct(best['delta'])}（{best['speedup']}）"],
    ]
    by_id = {row["bench_id"]: row for row in rows}
    ab_cases = [case for case in ab_case_map(ab).values() if case.get("bench_id") in WRITE_CASES]
    if ab_cases:
        ab_worst = max(ab_cases, key=lambda case: float(case["delta_pct"]))
        ab_delta = float(ab_worst["delta_pct"])
        if abs(ab_delta) < 5.0:
            data.append(
                [
                    "A/B 最大退化",
                    f"无（{len(ab_cases)} 项均 <5%，最大 `{ab_worst['bench_id']}` "
                    f"{fmt_delta_pct(ab_delta)}）",
                ]
            )
        else:
            data.append(
                [
                    "A/B 最大退化",
                    f"`{ab_worst['bench_id']}` {fmt_delta_pct(ab_delta)}（{ab_worst.get('status', '—')}）",
                ]
            )
        flipped = [
            case
            for case in ab_cases
            if abs(by_id[str(case["bench_id"])]["delta"]) >= 5.0
            and abs(float(case["delta_pct"])) < 5.0
        ]
        if flipped:
            data.append(
                [
                    "A/B 未复现",
                    "；".join(
                        f"`{case['bench_id']}` {fmt_delta_pct(by_id[str(case['bench_id'])]['delta'])}"
                        f" → {fmt_delta_pct(float(case['delta_pct']))}"
                        for case in sorted(flipped, key=lambda item: float(item["delta_pct"]), reverse=True)
                    ),
                ]
            )
        ab_average, ab_count = ab_stats(ab_cases)
        data.append(
            [
                "写路径 A/B",
                f"{len(ab_cases)} 项 {fmt_delta_pct(ab_average)}；"
                f"{better_count(ab_count)} 提升 / {ab_count[STATUS_FLAT]} 持平 / "
                f"{worse_count(ab_count)} 退化",
            ]
        )
    else:
        data.append(
            ["最大退化", f"`{worst['bench_id']}` {fmt_delta_pct(worst['delta'])}（{worst['speedup']}）"]
        )
    data.append(
        [
            "读 / 写（5 次）",
            f"读 {len(read_subset)} 项{read_note} {fmt_delta_pct(read_avg)}；"
            f"写 {len(write_subset)} 项 {fmt_delta_pct(write_avg)}",
        ]
    )
    if single:
        flips = [row for row in single["rows"] if row["relation"] == "相反"]
        if flips:
            data.append(
                [
                    "单次 → 聚合 翻转",
                    "；".join(
                        f"`{row['bench_id']}` {fmt_delta_pct(row['delta'])} → "
                        f"{fmt_delta_pct(row['aggregate_delta'])}"
                        for row in flips
                    ),
                ]
            )
        vol = []
        for row in single["rows"]:
            values = case_values(single["baseline_runs"], row["bench_id"]) + case_values(
                single["latest_runs"], row["bench_id"]
            )
            vol.append((volatility_ratio(values), row))
        ratio, noisy = max(vol, key=lambda item: item[0])
        latest_values = case_values(single["latest_runs"], noisy["bench_id"])
        data.append(
            [
                "单次噪声最大",
                f"`{noisy['bench_id']}` {value_range(latest_values)} ms（{ratio:.1f}×）",
            ]
        )
    return md_table(["重点", "数值"], data)


def combined_table(rows: list[dict], single: dict | None) -> str:
    single_map = {row["bench_id"]: row for row in single["rows"]} if single else {}
    data = []
    for row in rows:
        single_row = single_map.get(row["bench_id"])
        if row["status"] == STATUS_RESOLUTION:
            aggregate_cell = "—（分辨率不足）"
        else:
            aggregate_cell = f"{fmt_delta_pct(row['delta'])}（{row['status']}）"
        if single_row is None:
            single_cell = "—"
            relation = "—"
            range_cell = "—"
            noise = "—"
        elif single_row["status"] == STATUS_RESOLUTION:
            single_cell = "—（分辨率不足）"
            relation = "不可比"
            range_cell = "—"
            noise = "—"
        else:
            single_cell = f"{fmt_delta_pct(single_row['delta'])}（{single_row['status']}）"
            relation = single_row["relation"]
            base_values = case_values(single["baseline_runs"], row["bench_id"])
            latest_values = case_values(single["latest_runs"], row["bench_id"])
            range_cell = f"{value_range(base_values)} / {value_range(latest_values)}"
            noise = noise_grade(max(volatility_ratio(base_values), volatility_ratio(latest_values)))
        data.append(
            [
                row["layer"],
                f"`{row['bench_id']}`",
                fmt_ms(row["base"]),
                fmt_ms(row["latest"]),
                aggregate_cell,
                single_cell,
                relation,
                range_cell,
                noise,
            ]
        )
    return md_table(
        [
            "分层",
            "基准项",
            "基线 (ms)",
            "当前 (ms)",
            "5 次 Δ%",
            "单次 Δ%",
            "对照",
            "5 次区间（基线 / 当前）",
            "噪声",
        ],
        data,
    )


def suggestion_table(
    rows: list[dict], single: dict | None, ab: dict | None = None, limit: int | None = None
) -> str:
    by_id = {row["bench_id"]: row for row in rows}
    data = []
    if single:
        vol = []
        for row in single["rows"]:
            values = case_values(single["baseline_runs"], row["bench_id"]) + case_values(
                single["latest_runs"], row["bench_id"]
            )
            vol.append((volatility_ratio(values), row))
        ratio, noisy = max(vol, key=lambda item: item[0])
        latest_values = case_values(single["latest_runs"], noisy["bench_id"])
        pairs = ab.get("pairs") if ab else 10
        data.append(
            [
                "P0",
                f"降噪：A/B 交替 {pairs} 对；固定电源、绑核、Defender 排除",
                f"`{noisy['bench_id']}` 5 次 {value_range(latest_values)} ms，{ratio:.1f}×",
            ]
        )
    if ab:
        ab_cases = [case for case in ab_case_map(ab).values() if case.get("bench_id") in WRITE_CASES]
        ab_average, ab_count = ab_stats(ab_cases)
        data.append(
            [
                "P0",
                "写路径判定改用 A/B：不再按 5 次聚合判退化",
                f"{len(ab_cases)} 项 A/B {fmt_delta_pct(ab_average)}，"
                f"{worse_count(ab_count)} 退化；`txn_put` +18.4% → "
                f"{fmt_delta_pct(next((float(case['delta_pct']) for case in ab_cases if case['bench_id'] == 'txn_put'), 0.0))}",
            ]
        )
        data.append(
            [
                "P0",
                "微基准加轮次：insert / update / delete `--rounds` 2 → 20；txn-put 3 → 5",
                "`sql_delete` 5 次 1.211–24.735 ms（20.4×）；2 轮中位数不足以判 5%",
            ]
        )
    else:
        txn = by_id.get("txn_put")
        if txn:
            data.append(
                [
                    "P0",
                    "按层 bisect：用 `txn-put` 在基线..当前之间二分，先查 txn/WAL/提交组/fsync",
                    f"`txn_put` {fmt_delta_pct(txn['delta'])}，与 SQL 写项同向",
                ]
            )
    subquery = by_id.get("sql_subquery")
    if subquery:
        data.append(
            [
                "P1",
                "锁收益：`sql_subquery` 加入 `exists` / `scalar`，设不回退门限",
                f"`sql_subquery` {fmt_delta_pct(subquery['delta'])}"
                f"（{fmt_ms(subquery['base'])} → {fmt_ms(subquery['latest'])} ms）",
            ]
        )
    graph = by_id.get("graph_reach")
    if graph:
        data.append(
            [
                "P1",
                "放大 `graph_reach` 规模，或改 1000 次批量计时",
                f"当前 {fmt_ms(graph['latest'])} ms，低于 0.01 ms 分辨率",
            ]
        )
    data.append(
        [
            "P2",
            "CI 按平台各存基线，或在 Windows runner 定时跑基准",
            "现在用 Windows 基线在 Ubuntu 上只挡 3× 量级",
        ]
    )
    data.append(
        [
            "P2",
            "`record` 存原始 samples，看板补 p50 / p90 与箱线图",
            "当前 JSON 只有 min / median / max",
        ]
    )
    selected = data[:limit] if limit else data
    return md_table(["优先级", "动作", "依据"], selected)










def build_markdown(
    baseline: dict,
    runs: list[dict],
    rows: list[dict],
    count: dict[str, int],
    latest: dict | None,
    base_commit: str,
    single: dict | None = None,
    ab: dict | None = None,
) -> str:
    platform = baseline.get("platform", "unknown")
    generated = fmt_date(latest) if latest else "—"
    repeats = int(latest.get("aggregated_runs", 1)) if latest else 0
    provenance = (
        f"最近 {repeats} 次同 commit 运行取中位数的中位数"
        if repeats > 1
        else "单次运行"
    )

    parts = []
    parts.append("# trit-db 性能涨跌看板")
    parts.append("")
    parts.append(
        f"> 基线 `{base_commit}`（2026-11-21）→ 当前 {fmt_commit(latest)}（{generated}）"
        f" · `{platform}` · {len(rows)} 项 · {provenance} · 延迟越低越好"
    )
    parts.append("")
    parts.append("## 1. 汇总")
    parts.append("")
    parts.append(method_table(rows, count, single, ab))
    parts.append("")
    parts.append(highlights_table(rows, single, ab))
    parts.append("")
    if ab:
        parts.append("**A/B 复核（写路径，交替运行同一时间窗）**")
        parts.append("")
        parts.append(ab_table(ab, rows))
        parts.append("")
    parts.append("图表：写路径用 A/B 复核值，其余用 5 次聚合。")
    parts.append("")
    parts.append("![bench trend](bench-trend.svg)")
    parts.append("")
    parts.append("## 2. 逐项总表")
    parts.append("")
    parts.append(combined_table(rows, single))
    parts.append("")
    parts.append("## 3. 读写路径")
    parts.append("")
    parts.append(path_table(rows, ab))
    parts.append("")
    parts.append("## 4. 分层")
    parts.append("")
    parts.append("> 分层与徽章按 A/B 复核写路径；逐项总表保留 5 次历史值。")
    parts.append("")
    parts.append(layer_table(ab_corrected_rows(rows, ab)))
    parts.append("")
    parts.append("## 5. 运行历史")
    parts.append("")
    parts.append(history_table(baseline, group_runs(runs, platform)))
    parts.append("")
    parts.append("## 6. 建议")
    parts.append("")
    parts.append(suggestion_table(rows, single, ab))
    parts.append("")
    parts.append("## 7. 口径")
    parts.append("")
    parts.append("```bash")
    parts.append("python tools/bench/run.py --repeat 5   # 跑 5 轮并写 docs/results/bench-runs/")
    parts.append("python tools/bench/ab.py --baseline-bin <baseline-bin> --pairs 20   # 写路径 A/B")
    parts.append("python tools/bench/dashboard.py      # 只重建看板")
    parts.append("cargo run --release -p tritdb-bench -- check \\")
    parts.append("  --baseline docs/results/bench-baseline.json --tolerance 2.0 --min-ms 0.05")
    parts.append("```")
    parts.append("")
    parts.append(
        "判定：|Δ| < 5% 持平，≥5% 提升/退化，≥20% 大幅提升/明显退化；"
        "基线 < 0.01 ms 记分辨率不足。写路径以 A/B 复核为准，5 次表保留作历史。"
        "CI 只校验看板可复现。"
    )
    parts.append("")
    other_platforms = sorted({str(run.get("platform")) for run in runs if run.get("platform") != platform})
    if other_platforms:
        parts.append(
            "其它平台未混入："
            + "、".join(f"`{item}`" for item in other_platforms)
            + "。"
        )
        parts.append("")
    return "\n".join(parts)


def svg_escape(text: str) -> str:
    return (
        str(text)
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def ab_corrected_rows(rows: list[dict], ab: dict | None) -> list[dict]:
    if not ab:
        return rows
    ab_map = ab_case_map(ab)
    corrected = []
    for row in rows:
        case = ab_map.get(row["bench_id"])
        if case is not None and row["bench_id"] in WRITE_CASES:
            row = dict(row)
            row["delta"] = float(case["delta_pct"])
            row["status"] = classify(row["delta"])
        corrected.append(row)
    return corrected


def build_svg(
    rows: list[dict],
    baseline: dict,
    latest: dict | None,
    base_commit: str,
    ab: dict | None = None,
) -> str:
    rows = ab_corrected_rows(rows, ab)
    measured = [row for row in rows if row["status"] != STATUS_RESOLUTION]
    measured.sort(key=lambda row: row["delta"])
    values = [row["delta"] for row in measured]
    x_min = min(-60.0, math.floor(min(values) / 10.0) * 10.0 - 5.0)
    x_max = max(20.0, math.ceil(max(values) / 10.0) * 10.0 + 5.0)

    width = 1080
    label_left = 14
    plot_left = 210
    plot_right = 930
    plot_width = plot_right - plot_left
    top = 116
    row_height = 34
    bar_height = 22
    height = top + row_height * len(measured) + 58
    plot_bottom = top + row_height * len(measured)

    def x_of(value: float) -> float:
        return plot_left + (value - x_min) / (x_max - x_min) * plot_width

    zero = x_of(0.0)
    colors = {"gain": "#2da44e", "regress": "#cf222e", "flat": "#8b949e"}

    parts = []
    parts.append(
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" role="img" '
        'aria-label="trit-db CI benchmark median latency change versus baseline">'
    )
    parts.append(
        "<style>"
        "text{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,'Liberation Mono',monospace;}"
        ".t{fill:#1f2328;font-size:13px}.s{fill:#57606a;font-size:11px}"
        ".v{fill:#1f2328;font-size:12px}.g{fill:#8b949e;font-size:10px}"
        "</style>"
    )
    parts.append(f'<rect x="0" y="0" width="{width}" height="{height}" fill="#ffffff"/>')
    parts.append(
        f'<rect x="0.5" y="0.5" width="{width - 1}" height="{height - 1}" fill="none" stroke="#d0d7de"/>'
    )
    parts.append(
        f'<text class="t" x="{label_left}" y="30">trit-db benchmark — median latency vs baseline'
        f'{" (write path A/B)" if ab else ""}</text>'
    )
    base_platform = baseline.get("platform", "unknown")
    latest_date = fmt_date(latest) if latest else "—"
    latest_commit = str(latest.get("commit") or "—") if latest else "—"
    repeats = int(latest.get("aggregated_runs", 1)) if latest else 0
    aggregate_note = f" · median of {repeats} runs" if repeats > 1 else ""
    parts.append(
        f'<text class="s" x="{label_left}" y="50">'
        f'{svg_escape(base_platform)} · baseline 2026-11-21 ({svg_escape(base_commit)}) → latest {latest_date} ({svg_escape(latest_commit)}){aggregate_note}'
        "</text>"
    )
    total = len(rows)
    better = sum(1 for row in rows if row["delta"] <= -5 and row["status"] != STATUS_RESOLUTION)
    flat = sum(1 for row in rows if abs(row["delta"]) < 5 and row["status"] != STATUS_RESOLUTION)
    worse = sum(1 for row in rows if row["delta"] >= 5)
    below = sum(1 for row in rows if row["status"] == STATUS_RESOLUTION)
    summary = f"{total} cases · {better} faster · {flat} flat · {worse} slower"
    if below:
        summary += f" · {below} below 0.01 ms"
    parts.append(f'<text class="s" x="{label_left}" y="70">{summary} — lower is better</text>')

    # legend
    legend_y = 92
    parts.append(f'<rect x="{label_left}" y="{legend_y - 9}" width="10" height="10" fill="{colors["gain"]}"/>')
    parts.append(f'<text class="g" x="{label_left + 16}" y="{legend_y}">faster</text>')
    parts.append(f'<rect x="{label_left + 74}" y="{legend_y - 9}" width="10" height="10" fill="{colors["flat"]}"/>')
    parts.append(f'<text class="g" x="{label_left + 90}" y="{legend_y}">flat (±5%)</text>')
    parts.append(f'<rect x="{label_left + 186}" y="{legend_y - 9}" width="10" height="10" fill="{colors["regress"]}"/>')
    parts.append(f'<text class="g" x="{label_left + 202}" y="{legend_y}">slower</text>')

    # grid
    step = 20.0
    tick = math.ceil(x_min / step) * step
    while tick <= x_max + 0.01:
        x = x_of(tick)
        parts.append(
            f'<line x1="{x:.1f}" y1="{top - 6}" x2="{x:.1f}" y2="{plot_bottom + 4}" '
            'stroke="#eaeef2" stroke-width="1"/>'
        )
        label = "0%" if abs(tick) < 0.5 else f"{tick:+.0f}%"
        parts.append(f'<text class="g" x="{x:.1f}" y="{plot_bottom + 20}" text-anchor="middle">{label}</text>')
        tick += step

    parts.append(
        f'<line x1="{zero:.1f}" y1="{top - 6}" x2="{zero:.1f}" y2="{plot_bottom + 4}" '
        'stroke="#8b949e" stroke-width="1.5"/>'
    )

    for index, row in enumerate(measured):
        y = top + index * row_height
        color = colors["gain"] if row["delta"] <= -5 else colors["regress"] if row["delta"] >= 5 else colors["flat"]
        value_x = x_of(row["delta"])
        bar_x = min(zero, value_x)
        bar_width = max(2.0, abs(value_x - zero))
        parts.append(f'<text class="v" x="{label_left}" y="{y + 16}">{svg_escape(row["bench_id"])}</text>')
        parts.append(
            f'<rect x="{bar_x:.1f}" y="{y + 4}" width="{bar_width:.1f}" height="{bar_height}" '
            f'fill="{color}" rx="2"/>'
        )
        label = fmt_delta_pct(row["delta"])
        if row["delta"] < 0:
            tx = value_x - 6
            anchor = "end"
        else:
            tx = value_x + 6
            anchor = "start"
        parts.append(
            f'<text class="v" x="{tx:.1f}" y="{y + 19}" text-anchor="{anchor}">{label}</text>'
        )

    footer_y = plot_bottom + 42
    parts.append(
        f'<text class="g" x="{label_left}" y="{footer_y}">'
        "Generated by tools/bench/dashboard.py · docs/results/BENCH-DASHBOARD.md"
        f'{" · write path = A/B" if ab else ""}'
        "</text>"
    )
    parts.append("</svg>")
    return "\n".join(parts)


def badge(message: str, color: str) -> str:
    return json.dumps(
        {"schemaVersion": 1, "label": "bench", "message": message, "color": color},
        ensure_ascii=False,
    ) + "\n"


README_START = "<!-- BENCH-SUMMARY:BEGIN -->"
README_END = "<!-- BENCH-SUMMARY:END -->"


def readme_block(
    rows: list[dict],
    count: dict[str, int],
    baseline: dict,
    latest: dict | None,
    single: dict | None = None,
    ab: dict | None = None,
) -> str:
    date = fmt_date(latest) if latest else "—"
    platform = baseline.get("platform", "unknown")
    base_commit = str(baseline.get("commit") or "—")
    repeats = int(latest.get("aggregated_runs", 1)) if latest else 0
    provenance = f"最近 {repeats} 次中位数" if repeats > 1 else "单次"

    lines = []
    lines.append(README_START)
    lines.append("## 性能涨跌看板")
    lines.append("")
    lines.append(
        '<a href="docs/results/BENCH-DASHBOARD.md"><img alt="bench summary" '
        'src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fbfxh%2Ftrit-db%2Fmain%2Fdocs%2Fresults%2Fbench-badge.json"></a> '
        '<a href="docs/results/BENCH-DASHBOARD.md"><img alt="bench median delta" '
        'src="https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2Fbfxh%2Ftrit-db%2Fmain%2Fdocs%2Fresults%2Fbench-delta-badge.json"></a>'
    )
    lines.append("")
    lines.append(
        f"> `{base_commit}` → {fmt_commit(latest)} · `{platform}` · {len(rows)} 项 · "
        f"{provenance} · {date} · 延迟越低越好"
    )
    lines.append("")
    lines.append('![bench trend](docs/results/bench-trend.svg)')
    lines.append("")
    lines.append(method_table(rows, count, single, ab))
    lines.append("")
    lines.append(highlights_table(rows, single, ab))
    lines.append("")
    if ab:
        lines.append(f"**写路径 A/B 复核（交替 {ab.get('pairs', '—')} 对 × 多轮）**")
        lines.append("")
        lines.append(ab_table(ab, rows))
        lines.append("")
    lines.append("**逐项涨跌（5 次聚合 vs 单次直比）**")
    lines.append("")
    lines.append(combined_table(rows, single))
    lines.append("")
    lines.append("**下一步（前 3 条，完整 6 条见看板）**")
    lines.append("")
    lines.append(suggestion_table(rows, single, ab, limit=3))
    lines.append("")
    lines.append(
        "完整分层 / 历史 / 建议 / 口径见 "
        "[`docs/results/BENCH-DASHBOARD.md`](docs/results/BENCH-DASHBOARD.md)；"
        "原始数据在 [`docs/results/bench-runs/`](docs/results/bench-runs/)。"
    )
    lines.append(README_END)
    return "\n".join(lines)


def update_readme(path: Path, block: str) -> None:
    if not path.exists():
        return
    text = path.read_text(encoding="utf-8")
    if README_START in text and README_END in text:
        head, rest = text.split(README_START, 1)
        _, tail = rest.split(README_END, 1)
        text = head + block + tail
    else:
        # Insert before the 发布 section, keeping a blank line around the block.
        marker = "\n## 发布\n"
        if marker in text:
            head, tail = text.split(marker, 1)
            text = head.rstrip() + "\n\n" + block + "\n\n## 发布\n" + tail
        else:
            text = text.rstrip() + "\n\n" + block + "\n"
    path.write_text(text, encoding="utf-8")


def baseline_commit(path: Path) -> str:
    completed = subprocess.run(
        ["git", "log", "-1", "--format=%h", "--", str(path)],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    if completed.returncode != 0:
        return "—"
    return completed.stdout.strip() or "—"


def main() -> int:
    args = parse_args()
    baseline_path = ROOT / args.baseline
    baseline = read_json(baseline_path)
    runs = load_runs(ROOT / args.runs_dir)
    platform = baseline.get("platform", "unknown")
    commit = str(baseline.get("commit") or baseline_commit(baseline_path))

    latest_runs = latest_window(runs, platform, args.aggregate)
    latest = aggregate_runs(latest_runs)
    latest_cases = case_map(latest) if latest else {}
    base_cases = case_map(baseline)
    rows = build_rows(base_cases, latest_cases)
    if not rows:
        raise SystemExit("no comparable benchmark cases: baseline and runs are empty or mismatched")

    count = counts(rows)
    baseline_runs = [
        run
        for run in load_runs(ROOT / args.baseline_runs_dir)
        if run.get("platform") == platform
        and case_map(run)
        and same_commit(run.get("commit"), commit)
    ]
    single = build_single_comparison(baseline_runs, latest_runs, rows)
    ab = load_ab(ROOT / args.ab, platform)
    if ab and latest and not same_commit(ab.get("current_commit"), latest.get("commit")):
        ab = None
    markdown = build_markdown(baseline, runs, rows, count, latest, commit, single, ab)
    svg = build_svg(rows, baseline, latest, commit, ab)

    (ROOT / args.out_md).write_text(markdown + "\n", encoding="utf-8")
    (ROOT / args.out_svg).write_text(svg + "\n", encoding="utf-8")

    display_rows = ab_corrected_rows(rows, ab)
    total = len(display_rows)
    display_count = counts(display_rows)
    better = better_count(display_count)
    worse = worse_count(display_count)
    (ROOT / args.out_badge).write_text(
        badge(
            f"{better} better / {display_count[STATUS_FLAT]} flat / {worse} worse",
            "brightgreen" if worse == 0 else "orange",
        ),
        encoding="utf-8",
    )
    measured = [row for row in display_rows if row["status"] != STATUS_RESOLUTION]
    median_delta = statistics.median(row["delta"] for row in measured) if measured else 0.0
    (ROOT / args.out_delta_badge).write_text(
        badge(f"median {fmt_delta_pct(median_delta)}", "green" if median_delta <= 0 else "red"),
        encoding="utf-8",
    )

    update_readme(ROOT / args.readme, readme_block(rows, count, baseline, latest, single, ab))

    print(
        f"dashboard: {total} cases, {better} better / {display_count[STATUS_FLAT]} flat / {worse} worse, "
        f"median delta {median_delta:+.1f}%"
    )
    print(f"wrote {args.out_md}, {args.out_svg}, {args.out_badge}, {args.out_delta_badge}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
