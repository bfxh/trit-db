#!/usr/bin/env python3
"""Stdlib tests for the benchmark dashboard and A/B verdicts."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import dashboard


class DashboardTests(unittest.TestCase):
    def test_ranges_overlap(self) -> None:
        self.assertTrue(dashboard.ranges_overlap([1.0, 3.0], [2.0, 4.0]))
        self.assertFalse(dashboard.ranges_overlap([1.0, 2.0], [3.0, 4.0]))

    def test_ab_verdict_is_not_reproduced(self) -> None:
        rows = dashboard.build_rows(
            {"txn_put": 195.0, "sql_scan": 0.09},
            {"txn_put": 230.0, "sql_scan": 0.08},
        )
        ab = {
            "cases": [
                {
                    "bench_id": "txn_put",
                    "delta_pct": -1.9,
                    "status": "持平",
                    "baseline_ms": 192.0,
                    "current_ms": 188.0,
                    "pairs": 20,
                    "rounds": 5,
                }
            ]
        }
        table = dashboard.ab_table(ab, rows)
        self.assertIn("未复现", table)
        self.assertIn("-1.9%", table)

    def test_corrected_rows_apply_ab_to_read_cases(self) -> None:
        rows = dashboard.build_rows(
            {"txn_put": 195.0, "sql_scan": 0.09},
            {"txn_put": 230.0, "sql_scan": 0.08},
        )
        ab = {
            "cases": [
                {
                    "bench_id": "txn_put",
                    "delta_pct": -1.9,
                    "status": "持平",
                    "baseline_ms": 192.0,
                    "current_ms": 188.0,
                },
                {
                    "bench_id": "sql_scan",
                    "delta_pct": -1.5,
                    "status": "持平",
                    "baseline_ms": 0.09,
                    "current_ms": 0.088,
                },
            ]
        }
        corrected = {row["bench_id"]: row for row in dashboard.ab_corrected_rows(rows, ab)}
        self.assertEqual(corrected["txn_put"]["status"], dashboard.STATUS_FLAT)
        self.assertEqual(corrected["sql_scan"]["status"], dashboard.STATUS_FLAT)
        self.assertAlmostEqual(corrected["sql_scan"]["delta"], -1.5)

    def test_percentile(self) -> None:
        self.assertEqual(dashboard.percentile([1.0, 2.0, 3.0], 0.5), 2.0)
        self.assertAlmostEqual(dashboard.percentile([1.0, 2.0, 3.0, 4.0], 0.9), 3.7)

    def test_distribution_table_uses_samples_and_rss(self) -> None:
        rows = dashboard.build_rows({"sql_scan": 0.09}, {"sql_scan": 0.08})
        baseline = {
            "cases": [
                {
                    "bench_id": "sql_scan",
                    "samples": [0.08, 0.09, 0.10],
                    "rss_before_kb": 9_216,
                    "rss_kb": 10_240,
                }
            ]
        }
        latest = {
            "cases": [
                {
                    "bench_id": "sql_scan",
                    "samples": [0.07, 0.08, 0.09, 0.10],
                    "p50_ms": 0.085,
                    "p90_ms": 0.095,
                    "max_ms": 0.10,
                    "rss_before_kb": 9_216,
                    "rss_kb": 10_240,
                    "rss_max_kb": 10_752,
                    "rss_delta_kb": 1_024,
                }
            ]
        }
        table = dashboard.distribution_table(rows, latest, baseline)
        self.assertIn("p90 (ms)", table)
        self.assertIn("9.0 → 10.0 | 9.0 → 10.0", table)
        self.assertIn("+1.0 / +1.0 MiB", table)
        self.assertIn("10.5", table)
        self.assertIn("4", table)

    def test_ab_samples_csv(self) -> None:
        import tempfile

        ab = {
            "cases": [
                {
                    "bench_id": "sql_scan",
                    "baseline_samples": [0.1, 0.11],
                    "current_samples": [0.09, 0.10],
                    "baseline_rss_kb": [10_240, 10_240],
                    "current_rss_kb": [10_752, 10_752],
                }
            ]
        }
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "ab.csv"
            dashboard.write_ab_samples_csv(path, ab)
            text = path.read_text(encoding="utf-8")
        self.assertIn("bench_id,side,pair,median_ms,rss_kb", text)
        self.assertIn("sql_scan,baseline,1,0.100000,10240", text)


if __name__ == "__main__":
    unittest.main()
