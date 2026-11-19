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
import re
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
CREATE TABLE u (id INT, dept INT, label TEXT);
INSERT INTO u VALUES
  (10, 1, 'eng'),
  (20, 2, 'sales'),
  (30, 4, 'ops'),
  (40, 99, 'idle'),
  (50, NULL, 'nil');
CREATE TABLE e (id INT);
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
    "SELECT t.id, u.label FROM t JOIN u ON t.id = u.dept ORDER BY t.id;",
    "SELECT t.id FROM t WHERE t.id = 1;",
    "SELECT x.id FROM t AS x WHERE x.id = 2;",
    "SELECT x.id, y.label FROM t AS x JOIN u AS y ON x.id = y.dept ORDER BY x.id;",
    "SELECT u.label, COUNT(*), SUM(t.id) FROM t JOIN u ON t.id = u.dept GROUP BY u.label ORDER BY u.label;",
    "SELECT t.id, u.label FROM t JOIN u ON t.id = u.dept WHERE u.label <> 'sales' ORDER BY t.id;",
    "SELECT t.id, u.id FROM t JOIN u ON t.id + 1 = u.dept ORDER BY t.id;",
    "SELECT COUNT(*) FROM t JOIN u ON t.id = u.dept;",
    "SELECT t.id FROM t JOIN u ON t.id = u.dept AND u.label = 'eng' ORDER BY t.id;",
    "SELECT t.id, u.label FROM t JOIN u ON t.id = u.dept WHERE u.label IS NULL ORDER BY t.id;",
    "SELECT COUNT(*) FROM t JOIN u ON t.score = u.dept;",
    "SELECT a.id, b.id FROM t AS a JOIN t AS b ON a.id < b.id AND a.id <= 2 ORDER BY a.id, b.id;",
    "SELECT t.id, u.label FROM t LEFT JOIN u ON t.id = u.dept ORDER BY t.id;",
    "SELECT t.id, u.label FROM t LEFT OUTER JOIN u ON t.id = u.dept WHERE u.label IS NULL ORDER BY t.id;",
    "SELECT t.id, u.label FROM t LEFT JOIN u ON t.id = u.dept WHERE u.label = 'eng' ORDER BY t.id;",
    "SELECT u.label, COUNT(*) FROM t LEFT JOIN u ON t.id = u.dept GROUP BY u.label ORDER BY u.label;",
    "SELECT COUNT(*) FROM t LEFT JOIN u ON t.id = u.dept;",
    "SELECT x.id, y.label FROM t AS x LEFT JOIN u AS y ON x.id = y.dept ORDER BY x.id;",
    "SELECT t.id, u.id, u.label FROM t RIGHT JOIN u ON t.id = u.dept ORDER BY u.id;",
    "SELECT u.id, t.id FROM u RIGHT JOIN t ON u.dept = t.id ORDER BY t.id;",
    "SELECT COUNT(*) FROM t RIGHT JOIN u ON t.id = u.dept;",
    "SELECT t.id, u.label FROM t RIGHT JOIN u ON t.id = u.dept WHERE t.id IS NULL ORDER BY u.id;",
    "SELECT COUNT(*) FROM t RIGHT JOIN e ON t.id = e.id;",
    "SELECT t.id AS tid, u.id AS uid FROM t FULL JOIN u ON t.id = u.dept ORDER BY tid, uid;",
    "SELECT COUNT(*) FROM t FULL JOIN u ON t.id = u.dept;",
    "SELECT t.id AS tid, u.id AS uid FROM t FULL JOIN u ON t.id = u.dept WHERE t.id IS NULL OR u.id IS NULL ORDER BY tid, uid;",
    "SELECT u.label, COUNT(*) FROM t FULL JOIN u ON t.id = u.dept GROUP BY u.label ORDER BY u.label;",
    "SELECT t.id, e.id FROM t FULL JOIN e ON t.id = e.id ORDER BY t.id;",
    "SELECT t.id, u.id FROM t CROSS JOIN u ORDER BY t.id, u.id;",
    "SELECT x.id, y.label FROM t AS x CROSS JOIN u AS y WHERE x.id = y.dept ORDER BY x.id, y.id;",
    "SELECT COUNT(*) FROM t CROSS JOIN u;",
    "SELECT t.id, u.label FROM t, u WHERE u.dept = t.id ORDER BY t.id;",
    "SELECT t.id, u.id FROM t, u WHERE t.id = 1 ORDER BY u.id;",
    "SELECT u.label, COUNT(*) FROM t, u WHERE t.id = u.dept GROUP BY u.label ORDER BY u.label;",
    "SELECT COUNT(*) FROM t, u, e;",
    "SELECT t.id, e.id FROM t CROSS JOIN e ORDER BY t.id;",
    "SELECT t.id, u.label FROM t, u JOIN e ON u.id = e.id ORDER BY t.id;",
    "SELECT id FROM t WHERE id IN (1, 3, 5) ORDER BY id;",
    "SELECT id FROM t WHERE id NOT IN (1, 3, 5) ORDER BY id;",
    "SELECT id FROM t WHERE score IN (10, 20) ORDER BY id;",
    "SELECT id FROM t WHERE score NOT IN (10, 20) ORDER BY id;",
    "SELECT id FROM t WHERE name IN ('a', NULL) ORDER BY id;",
    "SELECT id FROM t WHERE score BETWEEN 10 AND 30 ORDER BY id;",
    "SELECT id FROM t WHERE score NOT BETWEEN 10 AND 20 ORDER BY id;",
    "SELECT id FROM t WHERE score BETWEEN NULL AND 20 ORDER BY id;",
    "SELECT id FROM t WHERE score BETWEEN 21 AND NULL ORDER BY id;",
    "SELECT id FROM t WHERE name LIKE 'a%' ORDER BY id;",
    "SELECT id FROM t WHERE name LIKE 'A%' ORDER BY id;",
    "SELECT id FROM t WHERE name LIKE '_' ORDER BY id;",
    "SELECT id FROM t WHERE name NOT LIKE '%d%' ORDER BY id;",
    "SELECT id FROM t WHERE name LIKE '%' ORDER BY id;",
    "SELECT id FROM t WHERE id IN (1, 2) AND score BETWEEN 10 AND 20 ORDER BY id;",
    "SELECT id, CASE WHEN score IS NULL THEN -1 ELSE score END AS s FROM t ORDER BY id;",
    "SELECT id, CASE score WHEN 10 THEN 'ten' WHEN 20 THEN 'twenty' ELSE 'other' END AS label FROM t ORDER BY id;",
    "SELECT id, CASE WHEN flag THEN 'yes' WHEN flag IS NULL THEN 'unknown' ELSE 'no' END AS f FROM t ORDER BY id;",
    "SELECT id, CASE WHEN id = 1 THEN 'one' END AS c FROM t ORDER BY id;",
    "SELECT id, CASE WHEN flag THEN 1 WHEN NOT flag THEN 0 END AS f FROM t ORDER BY id;",
    "SELECT id FROM t WHERE CASE WHEN score IS NULL THEN FALSE ELSE score > 10 END ORDER BY id;",
    "SELECT CASE WHEN COUNT(*) > 1 THEN 'many' ELSE 'one' END AS k, COUNT(*) FROM t GROUP BY score ORDER BY score;",
    "SELECT CASE WHEN flag THEN COUNT(*) ELSE 0 END AS n FROM t GROUP BY flag ORDER BY flag;",
    "SELECT t.id, u.label FROM t JOIN u ON CASE WHEN t.id = 1 THEN u.dept = 1 ELSE t.id = u.dept END ORDER BY t.id;",
    "SELECT id, CASE WHEN id IN (1, 3) THEN CASE WHEN score IS NULL THEN 'null' ELSE 'set' END ELSE 'other' END AS c FROM t ORDER BY id;",
]



UPDATE_SETUP = """
CREATE TABLE w (id INT NOT NULL, score INT, name TEXT, flag BOOL);
INSERT INTO w VALUES
  (1, 10, 'a', TRUE),
  (2, NULL, NULL, NULL),
  (3, 20, 'c', FALSE),
  (4, 20, 'd', TRUE),
  (5, NULL, 'e', NULL);
"""

UPDATE_STEPS = [
    "UPDATE w SET score = 42 WHERE id = 1;",
    "UPDATE w SET name = 'nil', flag = NULL WHERE id = 2;",
    "UPDATE w SET score = id + score WHERE id >= 3;",
    "UPDATE w SET id = id + 100, score = id WHERE id = 4;",
    "UPDATE w SET score = 7 WHERE id = 2;",
    "UPDATE w SET name = 'none' WHERE id = 999;",
]

UPDATE_CASES = [
    "SELECT id, score, name, flag FROM w ORDER BY id;",
    "SELECT COUNT(*) FROM w WHERE score IS NULL;",
    "SELECT SUM(score), MIN(score), MAX(score) FROM w;",
    "SELECT name, COUNT(*) FROM w GROUP BY name ORDER BY name;",
    "SELECT id FROM w WHERE score > 20 ORDER BY id;",
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


def run_tritdb_message(binary: str, db: str, sql: str) -> str:
    result = subprocess.run(
        [binary, "sql", "--db", db, "--sql", sql],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"tritdb failed for {sql!r}: {result.stderr.strip()}")
    return result.stdout.strip()


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
    checks = 0
    with tempfile.TemporaryDirectory(prefix="tritdb-sql-oracle-") as temp_dir:
        db = str(Path(temp_dir) / "sql.db")
        run_tritdb(binary, db, SETUP)

        sqlite = sqlite3.connect(":memory:")
        sqlite.executescript(SETUP)

        for sql in CASES:
            checks += 1
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

        run_tritdb_message(binary, db, UPDATE_SETUP)
        sqlite.executescript(UPDATE_SETUP)

        for sql in UPDATE_STEPS:
            checks += 1
            try:
                message = run_tritdb_message(binary, db, sql)
                match = re.fullmatch(r"updated (\d+) row\(s\) in w", message)
                if match is None:
                    raise RuntimeError(f"unexpected tritdb output: {message!r}")
                actual = int(match.group(1))
                expected = sqlite.execute(sql).rowcount
            except Exception as error:  # noqa: BLE001 - report and continue
                failures += 1
                print(f"FAIL {sql}\n  error: {error}")
                continue
            if actual != expected:
                failures += 1
                print(f"FAIL {sql}\n  tritdb rows: {actual}\n  sqlite rows: {expected}")

        for sql in UPDATE_CASES:
            checks += 1
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

    total = checks
    if failures:
        print(f"sql oracle: {total - failures}/{total} passed, {failures} failed")
        return 1
    selects = len(CASES) + len(UPDATE_CASES)
    updates = len(UPDATE_STEPS)
    print(f"sql oracle: {total}/{total} passed ({selects} SELECT, {updates} UPDATE row counts)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
