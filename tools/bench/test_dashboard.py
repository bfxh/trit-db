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

    def test_corrected_rows_only_touch_write_cases(self) -> None:
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
                }
            ]
        }
        corrected = {row["bench_id"]: row for row in dashboard.ab_corrected_rows(rows, ab)}
        self.assertEqual(corrected["txn_put"]["status"], dashboard.STATUS_FLAT)
        scan = next(row for row in rows if row["bench_id"] == "sql_scan")
        self.assertAlmostEqual(corrected["sql_scan"]["delta"], scan["delta"])

    def test_percentile(self) -> None:
        self.assertEqual(dashboard.percentile([1.0, 2.0, 3.0], 0.5), 2.0)
        self.assertAlmostEqual(dashboard.percentile([1.0, 2.0, 3.0, 4.0], 0.9), 3.7)

    def test_distribution_table_uses_samples_and_rss(self) -> None:
        rows = dashboard.build_rows({"sql_scan": 0.09}, {"sql_scan": 0.08})
        latest = {
            "cases": [
                {
                    "bench_id": "sql_scan",
                    "samples": [0.07, 0.08, 0.09, 0.10],
                    "p50_ms": 0.085,
                    "p90_ms": 0.095,
                    "max_ms": 0.10,
                    "rss_kb": 10_240,
                }
            ]
        }
        table = dashboard.distribution_table(rows, latest)
        self.assertIn("p90 (ms)", table)
        self.assertIn("10.0", table)
        self.assertIn("4", table)


if __name__ == "__main__":
    unittest.main()
