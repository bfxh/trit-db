#!/usr/bin/env python3
"""Build the trit-db benchmark dashboard from the committed baseline and run history.

Outputs:
  docs/results/BENCH-DASHBOARD.md   full tables (KPI / per-case / single-run / volatility /
                                    per-path / per-layer / history / trend / next steps)
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
from datetime import datetime
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


def detail_table(rows: list[dict]) -> str:
    data = []
    for index, row in enumerate(rows, 1):
        if row["status"] == STATUS_RESOLUTION:
            delta_ms = "—"
            delta_text = "—"
            throughput = "—"
            throughput_delta = "—"
        else:
            delta_ms = fmt_cell_delta(row["latest"] - row["base"])
            delta_text = fmt_delta_pct(row["delta"])
            throughput = f"{fmt_int(row['throughput'])} {row['unit']}"
            throughput_delta = f"{row['throughput_delta']:+.1f}%"
        data.append(
            [
                str(index),
                row["layer"],
                f"`{row['bench_id']}`",
                row["name"],
                row["scene"],
                fmt_ms(row["base"]),
                fmt_ms(row["latest"]),
                delta_ms,
                delta_text,
                f"{row['status']}（{row['speedup']}）"
                if row["status"] not in (STATUS_RESOLUTION, STATUS_FLAT)
                else row["status"],
                throughput,
                throughput_delta,
            ]
        )
    return md_table(
        [
            "#",
            "分层",
            "基准项",
            "场景",
            "参数",
            "基线 median (ms)",
            "最新 median (ms)",
            "Δ (ms)",
            "Δ%",
            "判定",
            "最新吞吐",
            "吞吐变化",
        ],
        data,
    )


def kpi_table(rows: list[dict], count: dict[str, int]) -> str:
    measured = [row for row in rows if row["status"] != STATUS_RESOLUTION]
    best = min(measured, key=lambda row: row["delta"])
    worst = max(measured, key=lambda row: row["delta"])
    median_delta = statistics.median(row["delta"] for row in measured) if measured else 0.0
    data = [
        ["对比项", f"{len(rows)}"],
        ["延迟下降（提升）", f"{better_count(count)}"],
        ["持平", f"{count[STATUS_FLAT]}（另有 {count[STATUS_RESOLUTION]} 项低于 0.01 ms 分辨率）"],
        ["延迟上升（退化）", f"{worse_count(count)}"],
        ["最大提升", f"`{best['bench_id']}` {fmt_delta_pct(best['delta'])}（{best['speedup']}）"],
        ["最大退化", f"`{worst['bench_id']}` {fmt_delta_pct(worst['delta'])}（{worst['speedup']}）"],
        ["各项 Δ% 中位数", fmt_delta_pct(median_delta)],
    ]
    return md_table(["指标", "数值"], data)


def path_table(rows: list[dict]) -> str:
    data = []
    for label, ids in (("读路径", READ_CASES), ("写路径", WRITE_CASES)):
        subset = [row for row in rows if row["bench_id"] in ids]
        measured = [row for row in subset if row["status"] != STATUS_RESOLUTION]
        average = sum(row["delta"] for row in measured) / len(measured) if measured else 0.0
        count = counts(subset)
        verdict = "整体变快" if average <= -5 else "整体变慢" if average >= 5 else "基本持平"
        data.append(
            [
                label,
                str(len(subset)) + (f"（含 {count[STATUS_RESOLUTION]} 项分辨率不足）" if count[STATUS_RESOLUTION] else ""),
                fmt_delta_pct(average),
                f"{better_count(count)} / {count[STATUS_FLAT]} / {worse_count(count)}",
                verdict,
            ]
        )
    return md_table(["路径", "项数", "平均 Δ%", "提升 / 持平 / 退化", "结论"], data)


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
        ["分层", "项数", "提升", "持平", "退化", "平均 Δ%", "最大提升", "最大退化"],
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


def trend_table(baseline: dict, groups: list[list[dict]], limit: int = 8) -> str:
    base_cases = case_map(baseline)
    selected = groups[-limit:]
    headers = ["基准项", "基线"]
    for group in selected:
        aggregate = aggregate_runs(group)
        label = f"{fmt_date(aggregate)}<br>`{aggregate.get('commit') or '—'}`"
        if len(group) > 1:
            label += f"<br>{len(group)} 次"
        headers.append(label)
    data = []
    for bench_id in sorted(base_cases):
        cells = [f"`{bench_id}`", fmt_ms(base_cases[bench_id])]
        for group in selected:
            aggregate = aggregate_runs(group)
            latest = case_map(aggregate).get(bench_id)
            if latest is None:
                cells.append("—")
            elif resolution_limited(base_cases[bench_id]):
                cells.append(f"{fmt_ms(latest)}<br>分辨率不足")
            else:
                cells.append(
                    f"{fmt_ms(latest)}<br>{fmt_delta_pct(delta_pct(base_cases[bench_id], latest))}"
                )
        data.append(cells)
    return md_table(headers, data)


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


def method_table(rows: list[dict], count: dict[str, int], single: dict | None) -> str:
    measured = [row for row in rows if row["status"] != STATUS_RESOLUTION]
    median_delta = statistics.median(row["delta"] for row in measured) if measured else 0.0
    data = [
        [
            "5 次聚合（本页判定）",
            f"{better_count(count)} / {count[STATUS_FLAT]} / {worse_count(count)}",
            fmt_delta_pct(median_delta),
        ]
    ]
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


def single_table(single: dict) -> str:
    data = []
    for row in single["rows"]:
        delta = "—" if row["status"] == STATUS_RESOLUTION else fmt_delta_pct(row["delta"])
        data.append(
            [
                f"`{row['bench_id']}`",
                row["layer"],
                fmt_ms(row["base"]),
                fmt_ms(row["latest"]),
                delta,
                row["status"],
                row["aggregate_status"] or "—",
                row["relation"],
            ]
        )
    return md_table(
        [
            "基准项",
            "分层",
            "基线单次 (ms)",
            "当前单次 (ms)",
            "Δ%",
            "单次判定",
            "5 次聚合判定",
            "对照",
        ],
        data,
    )


def volatility_table(single: dict) -> str:
    records = []
    for row in single["rows"]:
        base_values = case_values(single["baseline_runs"], row["bench_id"])
        latest_values = case_values(single["latest_runs"], row["bench_id"])
        base_ratio = volatility_ratio(base_values)
        latest_ratio = volatility_ratio(latest_values)
        ratio = max(base_ratio, latest_ratio)
        records.append(
            (
                ratio,
                [
                    f"`{row['bench_id']}`",
                    row["layer"],
                    value_range(base_values),
                    value_range(latest_values),
                    f"{base_ratio:.1f}× / {latest_ratio:.1f}×",
                    noise_grade(ratio),
                ],
            )
        )
    records.sort(key=lambda item: item[0], reverse=True)
    return md_table(
        ["基准项", "分层", "基线 5 次区间 (ms)", "当前 5 次区间 (ms)", "波动倍数（基线 / 当前）", "噪声"],
        [row for _, row in records],
    )


def readme_single_table(single: dict) -> str:
    data = []
    for row in single["rows"]:
        delta = "—" if row["status"] == STATUS_RESOLUTION else fmt_delta_pct(row["delta"])
        if row["aggregate_status"] == STATUS_RESOLUTION:
            aggregate_delta = "—"
        elif row["aggregate_delta"] is None:
            aggregate_delta = "—"
        else:
            aggregate_delta = fmt_delta_pct(row["aggregate_delta"])
        data.append(
            [
                f"`{row['bench_id']}`",
                fmt_ms(row["base"]),
                fmt_ms(row["latest"]),
                delta,
                aggregate_delta,
                row["relation"],
            ]
        )
    return md_table(
        ["基准项", "基线单次 (ms)", "当前单次 (ms)", "单次 Δ%", "聚合 Δ%", "对照"],
        data,
    )


def recommendation_lines(rows: list[dict], single: dict | None, write_avg: float) -> list[str]:
    by_id = {row["bench_id"]: row for row in rows}
    lines = []
    if single:
        vol = []
        for row in single["rows"]:
            values = case_values(single["baseline_runs"], row["bench_id"]) + case_values(
                single["latest_runs"], row["bench_id"]
            )
            vol.append((volatility_ratio(values), row))
        ratio, noisy = max(vol, key=lambda item: item[0])
        base_values = case_values(single["baseline_runs"], noisy["bench_id"])
        latest_values = case_values(single["latest_runs"], noisy["bench_id"])
        flips = [row for row in single["rows"] if row["relation"] == "相反"]
        gap = max(single["rows"], key=lambda row: row["gap"])
        contrast = ""
        if flips:
            flip_ids = "、".join(f"`{row['bench_id']}`" for row in flips)
            contrast = (
                f"单次直比与 5 次聚合在 {flip_ids} 上方向相反，"
                f"`{gap['bench_id']}` 的幅度差 {gap['gap']:.1f} 个百分点。"
            )
        lines.append(
            f"1. **先降噪，再谈优化。** 同一 commit 连跑 {len(single['latest_runs'])} 次，"
            f"`{noisy['bench_id']}` 的 median 在 "
            f"{value_range(base_values)} / {value_range(latest_values)} ms（基线 / 当前）之间摆动，"
            f"最高 {ratio:.1f}×；写路径 4 项平均 {fmt_delta_pct(write_avg)}，但{contrast}"
            "建议固定电源计划、绑核运行、把 `target/` 与基准库目录加入 Defender 排除，"
            "并让基线构建 / 当前构建 A/B 交替各跑 10 次；同时把 `ci` 套件的 DML 样本量从 "
            "128 行 / 64 txn 提到 512 行 / 256 txn，让单次样本本身更抗噪。"
        )
    txn = by_id.get("txn_put")
    if txn:
        lines.append(
            f"{len(lines) + 1}. **先查公共写路径，不要先改 SQL。** `txn_put` 不经过 SQL 解析也 "
            f"{fmt_delta_pct(txn['delta'])}，和 `sql_insert` / `sql_update` / `sql_delete` 同向；"
            "优先对比基线与当前之间的 `tritdb-txn` / `tritdb-wal` / 提交组 / fsync 路径，"
            "用 `txn-put` 作为 bisect 判据，定位到层再改代码。"
        )
    subquery = by_id.get("sql_subquery")
    if subquery:
        lines.append(
            f"{len(lines) + 1}. **把 `sql_subquery` 的收益锁住。** 当前 {fmt_delta_pct(subquery['delta'])}"
            f"（{fmt_ms(subquery['base'])} → {fmt_ms(subquery['latest'])} ms）是唯一大幅且稳定的提升；"
            "套件现在只跑 `--query in`，建议把 `exists` / `scalar` 一并纳入，"
            "并在 `check` 里给它单独设不回退门限，避免后续改执行计划时丢掉。"
        )
    graph = by_id.get("graph_reach")
    if graph:
        lines.append(
            f"{len(lines) + 1}. **给 `graph_reach` 一个能测的规模。** 当前 "
            f"{fmt_ms(graph['latest'])} ms 低于 0.01 ms 分辨率，涨跌只能记 0.0%；"
            "把 `--nodes` / `--hops` 提高一个数量级，或改成 1000 次批量计时，"
            "让结果落到可比较的毫秒级。"
        )
    lines.append(
        f"{len(lines) + 1}. **CI 门限按平台拆开。** `bench-regress` 现在拿 Windows 基线在 Ubuntu 上"
        "用 `--tolerance 2.0` 只挡 3× 量级，挡不住 10–20% 的漂移；建议按平台各存一份基线，"
        "或在 Windows runner 上定时跑 benchmark，PR 只比较同一 runner 上 main 的相对值。"
    )
    lines.append(
        f"{len(lines) + 1}. **看板下一版补分布。** 当前 JSON 只有 min / median / max，"
        "只能做单次表和区间表；建议 `record` 额外存原始 samples，看板加 p50 / p90 与箱线图，"
        "这样“涨了多少”与“抖了多少”能同时看见。"
    )
    return lines


def build_markdown(
    baseline: dict,
    runs: list[dict],
    rows: list[dict],
    count: dict[str, int],
    latest: dict | None,
    base_commit: str,
    single: dict | None = None,
) -> str:
    platform = baseline.get("platform", "unknown")
    measured = [row for row in rows if row["status"] != STATUS_RESOLUTION]
    median_delta = statistics.median(row["delta"] for row in measured) if measured else 0.0
    generated = fmt_date(latest) if latest else "—"
    repeats = int(latest.get("aggregated_runs", 1)) if latest else 0
    provenance = (
        f"最近 {repeats} 次同 commit 运行取各项中位数的中位数"
        if repeats > 1
        else "单次运行"
    )

    parts = []
    parts.append("# trit-db 性能涨跌看板")
    parts.append("")
    parts.append(
        "> 自动生成，请勿手改。数据来自 `docs/results/bench-baseline.json` 与 "
        "`docs/results/bench-runs/*.json`，生成脚本 `tools/bench/dashboard.py`。"
    )
    parts.append("")
    parts.append(f"- **基线**：2026-11-21（`{platform}`，commit `{base_commit}`）")
    parts.append(
        f"- **最新**：{generated}（`{platform}`，commit {fmt_commit(latest) if latest else '—'}；{provenance}）"
    )
    parts.append("- **口径**：同一平台、同一 `ci` 套件；指标为 6 轮交错的 median 延迟，**越低越好**")
    parts.append("- **判定**：|Δ| < 5% 记持平；≥5% 记提升/退化；≥20% 记大幅提升/明显退化；基线 < 0.01 ms 记分辨率不足")
    parts.append("")
    parts.append("## 1. 一眼看涨跌")
    parts.append("")
    parts.append(
        f"**{len(rows)} 项基准：{better_count(count)} 项更快、"
        f"{count[STATUS_FLAT]} 项持平、{worse_count(count)} 项更慢；"
        f"各项 Δ% 的中位数 {fmt_delta_pct(median_delta)}。**"
    )
    parts.append("")
    parts.append(kpi_table(rows, count))
    parts.append("")
    parts.append("![bench trend](bench-trend.svg)")
    parts.append("")
    parts.append("## 2. 逐项涨跌明细")
    parts.append("")
    parts.append("按分层、再按 Δ% 从快到慢排列。Δ 为负表示延迟下降（变快），为正表示延迟上升（变慢）。")
    parts.append("")
    parts.append(detail_table(rows))
    parts.append("")
    read_subset = [row for row in measured if row["bench_id"] in READ_CASES]
    write_subset = [row for row in measured if row["bench_id"] in WRITE_CASES]
    read_avg = sum(row["delta"] for row in read_subset) / len(read_subset) if read_subset else 0.0
    write_avg = sum(row["delta"] for row in write_subset) / len(write_subset) if write_subset else 0.0
    if single:
        single_rows = single["rows"]
        single_count = counts(single_rows)
        single_measured = [row for row in single_rows if row["status"] != STATUS_RESOLUTION]
        single_median = (
            statistics.median(row["delta"] for row in single_measured) if single_measured else 0.0
        )
        flips = [row for row in single_rows if row["relation"] == "相反"]
        gap_row = max(single_rows, key=lambda row: row["gap"])
        flip_text = "、".join(f"`{row['bench_id']}`" for row in flips) or "无"
        agg_gap = (
            fmt_delta_pct(gap_row["aggregate_delta"])
            if gap_row["aggregate_delta"] is not None
            else "—"
        )
        parts.append("## 3. 单次直比（参考）")
        parts.append("")
        parts.append(
            "取基线批次第 1 次与当前批次第 1 次直接相减。**单次结果只用于观察抖动，不参与判定**："
            f"当前批次同一 commit 的 {len(single['latest_runs'])} 次运行里，DML 的 median 能差一个数量级。"
        )
        parts.append("")
        parts.append(
            f"单次直比：{better_count(single_count)} 项更快、{single_count[STATUS_FLAT]} 项持平、"
            f"{worse_count(single_count)} 项更慢，Δ% 中位数 {fmt_delta_pct(single_median)}。"
            f"与 5 次聚合相比，方向相反的项：{flip_text}；偏差最大的是 `{gap_row['bench_id']}`"
            f"（单次 {fmt_delta_pct(gap_row['delta'])} vs 聚合 {agg_gap}）。"
        )
        parts.append("")
        parts.append(method_table(rows, count, single))
        parts.append("")
        parts.append("**逐项直比**")
        parts.append("")
        parts.append(single_table(single))
        parts.append("")
        parts.append(
            "**同 commit 5 次波动区间**"
            "（每项 median 的 min–max；倍数越大，单次结论越不可信）"
        )
        parts.append("")
        parts.append(volatility_table(single))
        parts.append("")
    best_row = min(measured, key=lambda row: row["delta"]) if measured else None
    parts.append("## 4. 读写路径")
    parts.append("")
    parts.append(
        f"读路径平均 {fmt_delta_pct(read_avg)}，写路径平均 {fmt_delta_pct(write_avg)}；"
        f"最大单项是 `{best_row['bench_id']}` {fmt_delta_pct(best_row['delta'])}。"
        if best_row
        else "没有可比较的基准项。"
    )
    parts.append("")
    parts.append(path_table(rows))
    parts.append("")
    parts.append("## 5. 分层涨跌")
    parts.append("")
    parts.append(layer_table(rows))
    parts.append("")
    parts.append("## 6. 运行历史")
    parts.append("")
    parts.append(
        f"只把 `{platform}` 的运行与同平台基线比较；同一天、同一 commit 的多轮运行先取"
        f"各项 median 的中位数，再算涨跌（避免单次 AV/fsync 抖动变成假涨跌）。"
        "其它平台的运行保留在 `docs/results/bench-runs/`，但不混入本表（跨平台绝对延迟不可比）。"
    )
    parts.append("")
    parts.append(history_table(baseline, group_runs(runs, platform)))
    parts.append("")
    parts.append("## 7. 逐项趋势（最近 8 个运行批次）")
    parts.append("")
    parts.append(trend_table(baseline, group_runs(runs, platform)))
    parts.append("")
    parts.append("## 8. 后续修改建议")
    parts.append("")
    parts.append("按优先级：先降噪，再定位，最后才改代码。")
    parts.append("")
    for line in recommendation_lines(rows, single, write_avg):
        parts.append(line)
        parts.append("")
    parts.append("## 9. 口径与复现")
    parts.append("")
    parts.append("```bash")
    parts.append("# 本地跑 5 轮（同一 commit 的中位数聚合）并写进 docs/results/bench-runs/")
    parts.append("python tools/bench/run.py --repeat 5")
    parts.append("")
    parts.append("# 只重建看板（不跑基准）")
    parts.append("python tools/bench/dashboard.py")
    parts.append("")
    parts.append("# 回归门限（与基线比较，超过 3 倍才失败）")
    parts.append("cargo run --release -p tritdb-bench -- check \\")
    parts.append("  --baseline docs/results/bench-baseline.json --tolerance 2.0 --min-ms 0.05")
    parts.append("```")
    parts.append("")
    parts.append("CI 不跑这套微基准：GitHub 共享 runner 与本机绝对延迟不可比，")
    parts.append("`bench.yml` 只在 PR 上重新生成看板并要求与提交内容一致；")
    parts.append("新数据由本机 `python tools/bench/run.py --repeat 5` 记录后提交。")
    parts.append("")
    other_platforms = sorted({str(run.get("platform")) for run in runs if run.get("platform") != platform})
    if other_platforms:
        parts.append(
            "已记录但未混入上表的其它平台："
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


def build_svg(rows: list[dict], baseline: dict, latest: dict | None, base_commit: str) -> str:
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
    parts.append(f'<text class="t" x="{label_left}" y="30">trit-db benchmark — median latency vs baseline</text>')
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
) -> str:
    measured = [row for row in rows if row["status"] != STATUS_RESOLUTION]
    median_delta = statistics.median(row["delta"] for row in measured) if measured else 0.0
    best = min(measured, key=lambda row: row["delta"])
    worst = max(measured, key=lambda row: row["delta"])
    date = fmt_date(latest) if latest else "—"
    platform = baseline.get("platform", "unknown")
    repeats = int(latest.get("aggregated_runs", 1)) if latest else 0
    provenance = f"最近 {repeats} 次同 commit 的中位数" if repeats > 1 else "单次运行"

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
        f"> 基线 2026-11-21 → 最新 {date}（同平台 `{platform}`，{len(rows)} 项 `ci` 套件基准，{provenance}；"
        "median 延迟越低越好）。"
    )
    lines.append("")
    lines.append('![bench trend](docs/results/bench-trend.svg)')
    lines.append("")
    lines.append(
        f"**{better_count(count)} 项更快 / {count[STATUS_FLAT]} 项持平 / "
        f"{worse_count(count)} 项更慢；各项 Δ% 的中位数 {fmt_delta_pct(median_delta)}。**"
    )
    lines.append("")
    read_subset = [row for row in measured if row["bench_id"] in READ_CASES]
    write_subset = [row for row in measured if row["bench_id"] in WRITE_CASES]
    read_avg = sum(row["delta"] for row in read_subset) / len(read_subset) if read_subset else 0.0
    write_avg = sum(row["delta"] for row in write_subset) / len(write_subset) if write_subset else 0.0
    read_below = sum(1 for row in rows if row["bench_id"] in READ_CASES and row["status"] == STATUS_RESOLUTION)
    read_note = f"（另 {read_below} 项低于 0.01 ms）" if read_below else ""
    lines.append(
        f"> 读路径 {len(read_subset)} 项{read_note}平均 {fmt_delta_pct(read_avg)}，"
        f"写路径 {len(write_subset)} 项平均 {fmt_delta_pct(write_avg)}。"
    )
    lines.append("")
    lines.append(
        md_table(
            ["一眼看", "数值"],
            [
                ["最大提升", f"`{best['bench_id']}` {fmt_delta_pct(best['delta'])}（{best['speedup']}）"],
                ["最大退化", f"`{worst['bench_id']}` {fmt_delta_pct(worst['delta'])}（{worst['speedup']}）"],
                ["提升 / 持平 / 退化", f"{better_count(count)} / {count[STATUS_FLAT]} / {worse_count(count)}"],
                ["各项 Δ% 中位数", fmt_delta_pct(median_delta)],
            ],
        )
    )
    lines.append("")
    if single:
        lines.append(method_table(rows, count, single))
        lines.append("")
    lines.append("<details>")
    lines.append(f"<summary>逐项涨跌明细（{len(rows)} 项，点开看每一项涨了多少 / 跌了多少）</summary>")
    lines.append("")
    lines.append(
        md_table(
            ["基准项", "场景", "基线 (ms)", "最新 (ms)", "Δ%", "判定"],
            [
                [
                    f"`{row['bench_id']}`",
                    row["name"],
                    fmt_ms(row["base"]),
                    fmt_ms(row["latest"]),
                    "—" if row["status"] == STATUS_RESOLUTION else fmt_delta_pct(row["delta"]),
                    row["status"] if row["status"] != STATUS_RESOLUTION else row["status"],
                ]
                for row in rows
            ],
        )
    )
    lines.append("")
    lines.append("</details>")
    if single:
        lines.append("")
        lines.append("<details>")
        lines.append(
            "<summary>单次直比（仅参考：同一 commit 连跑 5 次，DML 的 median 可差一个数量级）</summary>"
        )
        lines.append("")
        lines.append(readme_single_table(single))
        lines.append("")
        lines.append("</details>")
    lines.append("")
    lines.append(
        "> **下一步**：先降噪（A/B 各 10 次），再按层定位写路径（`txn_put` 与 SQL 写项同向）；"
        "单次表只作噪声参考。完整建议见看板第 8 节。"
    )
    lines.append("")
    lines.append(
        "分层汇总、单次波动区间、后续修改建议与口径见 "
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
    markdown = build_markdown(baseline, runs, rows, count, latest, commit, single)
    svg = build_svg(rows, baseline, latest, commit)

    (ROOT / args.out_md).write_text(markdown + "\n", encoding="utf-8")
    (ROOT / args.out_svg).write_text(svg + "\n", encoding="utf-8")

    total = len(rows)
    better = better_count(count)
    worse = worse_count(count)
    (ROOT / args.out_badge).write_text(
        badge(
            f"{better} better / {count[STATUS_FLAT]} flat / {worse} worse",
            "brightgreen" if worse == 0 else "orange",
        ),
        encoding="utf-8",
    )
    measured = [row for row in rows if row["status"] != STATUS_RESOLUTION]
    median_delta = statistics.median(row["delta"] for row in measured) if measured else 0.0
    (ROOT / args.out_delta_badge).write_text(
        badge(f"median {fmt_delta_pct(median_delta)}", "green" if median_delta <= 0 else "red"),
        encoding="utf-8",
    )

    update_readme(ROOT / args.readme, readme_block(rows, count, baseline, latest, single))

    print(
        f"dashboard: {total} cases, {better} better / {count[STATUS_FLAT]} flat / {worse} worse, "
        f"median delta {median_delta:+.1f}%"
    )
    print(f"wrote {args.out_md}, {args.out_svg}, {args.out_badge}, {args.out_delta_badge}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
