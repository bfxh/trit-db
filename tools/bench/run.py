#!/usr/bin/env python3
"""Run the CI benchmark suite, record it under docs/results/bench-runs/, rebuild the dashboard.

Usage:
    python tools/bench/run.py                 # local run, label inferred
    python tools/bench/run.py --label ci      # CI runner
    python tools/bench/run.py --no-dashboard  # only record

The Rust `record` command writes a JSON without provenance, so this wrapper adds
generated_at / commit / runner fields before the file is committed.  Keeping the
raw `tritdb-bench record` behaviour untouched means the existing baseline
workflow and `check` command keep working.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", default="docs/results/bench-runs")
    parser.add_argument("--label", default="", help="optional filename suffix, e.g. ci or local")
    parser.add_argument("--repeat", type=int, default=1, help="record the suite N times (default 1)")
    parser.add_argument(
        "--rounds",
        type=int,
        default=0,
        help="override --rounds for cases that support it (0 keeps suite defaults)",
    )
    parser.add_argument("--no-dashboard", action="store_true")
    parser.add_argument("--keep-temp", action="store_true")
    return parser.parse_args()


def git(*args: str) -> str:
    completed = subprocess.run(
        ["git", *args],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    if completed.returncode != 0:
        return ""
    return completed.stdout.strip()


def run_record(out_dir: Path, rounds: int = 0) -> Path:
    out_dir.mkdir(parents=True, exist_ok=True)
    temp = out_dir / ".tmp-bench-run.json"
    if temp.exists():
        temp.unlink()
    print("running cargo record ...", flush=True)
    command = [
        "cargo",
        "run",
        "--quiet",
        "--release",
        "-p",
        "tritdb-bench",
        "--",
        "record",
        "--out",
        str(temp),
    ]
    if rounds > 0:
        command.extend(["--rounds", str(rounds)])
    completed = subprocess.run(command, cwd=ROOT, check=False)
    if completed.returncode != 0:
        raise SystemExit(f"cargo record failed with exit code {completed.returncode}")
    if not temp.exists():
        raise SystemExit("cargo record finished but wrote no JSON")
    return temp


def stamp_metadata(data: dict, label: str) -> dict:
    # Keep the runner's local offset so the date in the file name matches the
    # date a human sees; CI runners are UTC, local runs keep +08:00.
    now = datetime.now().astimezone().replace(microsecond=0)
    data["generated_at"] = now.isoformat()
    data["commit"] = git("rev-parse", "--short", "HEAD")
    data["commit_full"] = git("rev-parse", "HEAD")
    # Ignore untracked files: the run JSON itself is untracked while it is
    # being written, and that should not mark the tree as dirty.
    data["dirty"] = bool(git("status", "--porcelain", "--untracked-files=no"))
    data["runner"] = "github-actions" if os.environ.get("GITHUB_ACTIONS") else "local"
    if label:
        data["label"] = label
    return data


def target_path(out_dir: Path, data: dict) -> Path:
    date = str(data["generated_at"])[:10]
    platform = str(data.get("platform", "unknown"))
    label = str(data.get("label") or "")
    commit = str(data.get("commit") or "")
    parts = [date, platform]
    if label:
        parts.append(label)
    if commit:
        parts.append(commit)
    stem = "-".join(parts)
    candidate = out_dir / f"{stem}.json"
    index = 2
    while candidate.exists():
        candidate = out_dir / f"{stem}-{index}.json"
        index += 1
    return candidate


def main() -> int:
    args = parse_args()
    out_dir = ROOT / args.out_dir
    written = []
    for index in range(max(1, args.repeat)):
        if args.repeat > 1:
            print(f"--- repeat {index + 1}/{args.repeat} ---", flush=True)
        temp = run_record(out_dir, args.rounds)
        data = json.loads(temp.read_text(encoding="utf-8"))
        data = stamp_metadata(data, args.label)
        path = target_path(out_dir, data)
        path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        if not args.keep_temp:
            temp.unlink(missing_ok=True)
        written.append(path)
        try:
            shown = path.relative_to(ROOT)
        except ValueError:
            shown = path
        print(f"wrote {shown}", flush=True)

    if not args.no_dashboard:
        completed = subprocess.run(
            [sys.executable, str(ROOT / "tools/bench/dashboard.py")],
            cwd=ROOT,
            check=False,
        )
        if completed.returncode != 0:
            raise SystemExit(f"dashboard generation failed with exit code {completed.returncode}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
