#!/usr/bin/env python3
"""Cross-check the tritdb SQL engine against Python's sqlite3.

The comparison is deliberately value-only: SQLite and tritdb use different
default labels for expressions, and SQLite has no `IS UNKNOWN` spelling.
Both engines run the same setup and the same SELECT statements; rows are
normalized to strings before comparison.
"""

from __future__ import annotations

import argparse
import os
import sqlite3
import subprocess
import sys
import tempfile
from pathlib import Path

SETUP = """
CREATE TABLE t (id INT NOT NULL, score INT, name TEXT, flag BOOL);
INSERT INTO t VALUES
  (1, 10, 'a', TRUE),
  (2, NULL, NULL, NULL),
  (3, 20, 'c', FALSE),
  (4, 30, 'd', TRUE),
  (5, NULL, '', NULL);
"""

CASES = [
    "SELECT id FROM t WHERE flag ORDER BY id;",
    "SELECT id FROM t WHERE NOT flag ORDER BY id;",
    "SELECT id FROM t WHERE flag IS NULL ORDER BY id;",
    "SELECT id FROM t WHERE flag = NULL ORDER BY id;",
    "SELECT id FROM t WHERE score > 10 AND score < 30 ORDER BY id;",
    "SELECT id FROM t WHERE score IS NULL OR flag IS NULL ORDER BY id;",
    "SELECT id, score FROM t ORDER BY score ASC;",
    "SELECT id, score FROM t ORDER BY score DESC;",
    "SELECT COUNT(*), COUNT(score), SUM(score), MIN(score), MAX(score) FROM t;",
    "SELECT id, score + 1 AS s FROM t ORDER BY s DESC LIMIT 2;",
    "SELECT id FROM t WHERE (score = 10 OR score = 20) AND flag IS NOT NULL ORDER BY id;",
    "SELECT id FROM t WHERE NOT (flag IS NULL) ORDER BY id;",
    "SELECT id FROM t WHERE (flag OR FALSE) AND (flag OR TRUE) ORDER BY id;",
    "SELECT id FROM t WHERE flag AND TRUE ORDER BY id;",
    "SELECT id FROM t WHERE flag AND FALSE ORDER BY id;",
    "SELECT id FROM t WHERE flag OR FALSE ORDER BY id;",
    "SELECT id FROM t WHERE flag OR TRUE ORDER BY id;",
    "SELECT id FROM t WHERE NOT flag OR flag IS NULL ORDER BY id;",
    "SELECT id FROM t WHERE score <> 10 AND score IS NOT NULL ORDER BY id;",
    "SELECT id FROM t WHERE score >= 20 OR score IS NULL ORDER BY id;",
    "SELECT id FROM t WHERE name = 'a' OR name IS NULL ORDER BY id;",
    "SELECT id FROM t WHERE name <> '' AND name IS NOT NULL ORDER BY id;",
    "SELECT id FROM t WHERE NOT (flag AND FALSE) ORDER BY id;",
    "SELECT id FROM t WHERE NOT (flag OR TRUE) ORDER BY id;",
    "SELECT id FROM t WHERE score = 10 OR score = 20 ORDER BY id;",
    "SELECT id FROM t WHERE score <> 10 ORDER BY id;",
    "SELECT id FROM t WHERE score IS NOT NULL AND flag IS NOT NULL ORDER BY id;",
    "SELECT id FROM t WHERE name IS NULL OR name = '' ORDER BY id;",
    "SELECT id FROM t WHERE name IS NOT NULL AND name <> '' ORDER BY id;",
    "SELECT id FROM t WHERE (score + 1) > 11 ORDER BY id;",
    "SELECT id FROM t WHERE (score - 10) = 0 ORDER BY id;",
    "SELECT id FROM t WHERE (score * 2) >= 40 ORDER BY id;",
    "SELECT id FROM t WHERE (score / 10) = 2 ORDER BY id;",
    "SELECT id FROM t WHERE -(score) < 0 ORDER BY id;",
    "SELECT score, COUNT(*) FROM t GROUP BY score ORDER BY score;",
    "SELECT score, COUNT(*), SUM(id), MIN(id), MAX(id) FROM t GROUP BY score ORDER BY score;",
    "SELECT flag, COUNT(*) FROM t GROUP BY flag ORDER BY flag;",
    "SELECT name, COUNT(*) FROM t GROUP BY name HAVING COUNT(*) >= 2 ORDER BY name;",
    "SELECT name, SUM(score) FROM t GROUP BY name HAVING SUM(score) > 10 ORDER BY name;",
    "SELECT score AS s, COUNT(*) AS n FROM t GROUP BY score ORDER BY n DESC, s ASC LIMIT 2;",
    "SELECT name FROM t GROUP BY name ORDER BY name;",
    "SELECT score + 1 AS s, COUNT(*) FROM t GROUP BY score ORDER BY s;",
    "SELECT COUNT(*) FROM t HAVING COUNT(*) > 0;",
    "SELECT score FROM t GROUP BY score HAVING score IS NULL;",
    "SELECT COUNT(*), score FROM t GROUP BY score HAVING COUNT(*) > 1 ORDER BY COUNT(*) DESC, score;",
]


def resolve_binary(path: str) -> str:
    candidate = Path(path)
    if candidate.exists():
        return str(candidate)
    if os.name == "nt":
        exe = Path(path + ".exe")
        if exe.exists():
            return str(exe)
    raise FileNotFoundError(f"tritdb binary not found: {path}")


def run_tritdb(binary: str, db: str, sql: str) -> list[tuple[str, ...]]:
    result = subprocess.run(
        [binary, "sql", "--db", db, "--sql", sql],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"tritdb failed for {sql!r}: {result.stderr.strip()}")
    lines = result.stdout.splitlines()
    if not lines:
        raise RuntimeError(f"tritdb returned no output for {sql!r}")
    rows = []
    for line in lines[1:]:
        rows.append(tuple(normalize_tritdb(value) for value in line.split("\t")))
    return rows


def normalize_tritdb(value: str) -> str:
    if value == "TRUE":
        return "1"
    if value == "FALSE":
        return "0"
    return value


def run_sqlite(connection: sqlite3.Connection, sql: str) -> list[tuple[str, ...]]:
    cursor = connection.execute(sql)
    rows = []
    for row in cursor.fetchall():
        rows.append(tuple(normalize_sqlite(value) for value in row))
    return rows


def normalize_sqlite(value: object) -> str:
    if value is None:
        return "NULL"
    if isinstance(value, str):
        return value
    return str(value)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tritdb", default="target/debug/tritdb")
    args = parser.parse_args()

    binary = resolve_binary(args.tritdb)
    failures = 0
    with tempfile.TemporaryDirectory(prefix="tritdb-sql-oracle-") as temp_dir:
        db = str(Path(temp_dir) / "sql.db")
        run_tritdb(binary, db, SETUP)

        sqlite = sqlite3.connect(":memory:")
        sqlite.executescript(SETUP)

        for sql in CASES:
            try:
                actual = run_tritdb(binary, db, sql)
                expected = run_sqlite(sqlite, sql)
            except Exception as error:  # noqa: BLE001 - report and continue
                failures += 1
                print(f"FAIL {sql}\n  error: {error}")
                continue
            if actual != expected:
                failures += 1
                print(f"FAIL {sql}\n  tritdb: {actual}\n  sqlite: {expected}")

        sqlite.close()

    total = len(CASES)
    if failures:
        print(f"sql oracle: {total - failures}/{total} passed, {failures} failed")
        return 1
    print(f"sql oracle: {total}/{total} passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
