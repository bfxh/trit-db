use std::ffi::OsStr;
use std::fs;
use std::process::{Command, Output};

use tritdb_testkit::TempDir;

fn run<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_tritdb"))
        .args(args)
        .output()
        .expect("run tritdb CLI")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf8 stdout")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf8 stderr")
}

#[test]
fn sql_cli_creates_rows_and_reopens() {
    let dir = TempDir::new("sql-cli");
    let db = dir.join("sql.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, flag BOOL); \
         INSERT INTO t VALUES (1, TRUE), (2, NULL), (3, FALSE);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("created table t"));
    assert!(stdout(&output).contains("inserted 3 row(s)"));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "ANALYZE; EXPLAIN SELECT id FROM t WHERE flag IS NULL;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("analyzed 1 table(s)"), "stdout: {text}");
    assert!(text.contains("Scan t est rows=3"), "stdout: {text}");
    assert!(text.contains("Filter est rows=1"), "stdout: {text}");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT id, flag FROM t WHERE flag IS NULL ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "id\tflag\n2\tNULL\n");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT id FROM t WHERE flag ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "id\n1\n");
}

#[test]
fn sql_cli_rejects_deeply_nested_expression() {
    let dir = TempDir::new("sql-cli-depth");
    let db = dir.join("sql.db");
    let db = db.to_str().expect("path");
    let sql = format!("SELECT {}1{};", "(".repeat(512), ")".repeat(512));
    let output = run(["sql", "--db", db, "--sql", &sql]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("nesting too deep"),
        "stderr: {}",
        stderr(&output)
    );
}

#[test]
fn sql_cli_reads_file_and_reports_errors() {
    let dir = TempDir::new("sql-cli-file");
    let db = dir.join("sql.db");
    let script = dir.join("script.sql");
    fs::write(
        &script,
        "CREATE TABLE t (id INT);\nINSERT INTO t VALUES (1), (2);\n",
    )
    .expect("write script");
    let db = db.to_str().expect("path");
    let script = script.to_str().expect("path");
    let output = run(["sql", "--db", db, "--file", script]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run(["sql", "--db", db, "--sql", "SELECT * FROM missing;"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unknown table"));
}

#[test]
fn sql_count_models_returns_exact_model_count() {
    let dir = TempDir::new("sql-count-models");
    let db = dir.join("ann.db");
    let db = db.to_str().expect("path");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT count_models('a AND (NOT b OR c)');",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("models"), "out: {text}");
    assert!(text.contains('3'), "out: {text}");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT count_models('a OR b', 3);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains('6'));
}

#[test]
fn under_repair_returns_only_certain_rows() {
    let dir = TempDir::new("sql-under-repair");
    let db = dir.join("ur.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, x INT);          INSERT INTO t VALUES (1,10),(1,20),(2,30),(2,30);          SELECT * FROM t UNDER REPAIR ON (id);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("2\t30"), "out: {text}");
    assert!(!text.contains("1\t10"), "out: {text}");
    assert!(!text.contains("1\t20"), "out: {text}");
}

#[test]
fn under_repair_keeps_a_consistent_table() {
    let dir = TempDir::new("sql-under-repair-ok");
    let db = dir.join("ur.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, x INT);          INSERT INTO t VALUES (1,10),(2,20);          SELECT * FROM t UNDER REPAIR ON (id);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1\t10"), "out: {text}");
    assert!(text.contains("2\t20"), "out: {text}");
}

#[test]
fn under_repair_unknown_key_is_an_error() {
    let dir = TempDir::new("sql-under-repair-bad-key");
    let db = dir.join("ur.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, x INT);          INSERT INTO t VALUES (1,10);          SELECT * FROM t UNDER REPAIR ON (missing);",
    ]);
    assert!(!output.status.success(), "unknown key must fail");
}

#[test]
fn logic_column_and_truth_projection() {
    let dir = TempDir::new("sql-logic-column");
    let db = dir.join("logic.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, status LOGIC K3); \
         INSERT INTO t VALUES (1, TRUE), (2, FALSE), (3, NULL); \
         SELECT id, truth(status) AS s FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1\tTRUE"), "out: {text}");
    assert!(text.contains("2\tFALSE"), "out: {text}");
    assert!(text.contains("3\tUNKNOWN"), "out: {text}");
}

#[test]
fn logic_column_is_unknown_filterable() {
    let dir = TempDir::new("sql-logic-unknown");
    let db = dir.join("logic.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, status LOGIC K3); \
         INSERT INTO t VALUES (1, TRUE), (2, FALSE), (3, NULL); \
         SELECT id FROM t WHERE status IS UNKNOWN;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("3"), "out: {text}");
    assert!(!text.contains("\n1"), "out: {text}");
    assert!(!text.contains("\n2"), "out: {text}");
}

#[test]
fn belnap_column_roundtrips_all_four_values() {
    let dir = TempDir::new("sql-belnap");
    let db = dir.join("b.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, status LOGIC BELNAP); \
         INSERT INTO t VALUES (1, TRUE), (2, FALSE), (3, BOTH), (4, NEITHER); \
         SELECT id, status FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    for expected in ["1\tTRUE", "2\tFALSE", "3\tBOTH", "4\tNEITHER"] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }

    // Reopen: the TRSQ decoder must accept the Belnap value tag.
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT id, status FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("3\tBOTH"), "out: {text}");
    assert!(text.contains("4\tNEITHER"), "out: {text}");
}

#[test]
fn belnap_both_and_neither_project_to_unknown() {
    let dir = TempDir::new("sql-belnap-unknown");
    let db = dir.join("b.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, status LOGIC BELNAP); \
         INSERT INTO t VALUES (1, TRUE), (2, BOTH), (3, NEITHER), (4, NULL); \
         SELECT id FROM t WHERE status IS UNKNOWN ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    for expected in ["\n2", "\n3", "\n4"] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }
    assert!(!text.contains("\n1"), "TRUE must not be UNKNOWN: {text}");
}

#[test]
fn lukasiewicz_and_nat_columns_roundtrip() {
    let dir = TempDir::new("sql-luk-nat");
    let db = dir.join("ln.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, conf LOGIC LUKASIEWICZ, n LOGIC NAT);          INSERT INTO t VALUES (1, 500, 7), (2, 1000, 0);          SELECT id, conf, n FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1	0.500	7"), "out: {text}");
    assert!(text.contains("2	1.000	0"), "out: {text}");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT id, conf, n FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1	0.500	7"), "reopen: {text}");
}

#[test]
fn unique_column_rejects_duplicates_and_survives_reopen() {
    let dir = TempDir::new("sql-unique");
    let db = dir.join("u.db");
    let db = db.to_str().expect("path");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL UNIQUE, name TEXT);          INSERT INTO t VALUES (1, 'a');",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run(["sql", "--db", db, "--sql", "INSERT INTO t VALUES (1, 'b');"]);
    assert!(!output.status.success(), "duplicate must fail");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "INSERT INTO t VALUES (2, 'c'); SELECT * FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1	a"), "out: {text}");
    assert!(text.contains("2	c"), "out: {text}");
}

#[test]
fn under_repair_accepts_multiple_key_columns() {
    let dir = TempDir::new("sql-under-repair-multi");
    let db = dir.join("ur.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (tenant INT NOT NULL, id INT NOT NULL, x INT);          INSERT INTO t VALUES (1,1,10),(1,1,20),(1,2,30),(2,1,40);          SELECT * FROM t UNDER REPAIR ON (tenant, id);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1	2	30"), "out: {text}");
    assert!(text.contains("2	1	40"), "out: {text}");
    assert!(!text.contains("1	1	10"), "out: {text}");
    assert!(!text.contains("1	1	20"), "out: {text}");
}

#[test]
fn update_checks_unique_columns() {
    let dir = TempDir::new("sql-update-unique");
    let db = dir.join("u.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL UNIQUE, name TEXT);          INSERT INTO t VALUES (1,'a'),(2,'b');",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "UPDATE t SET id = 1 WHERE name = 'b';",
    ]);
    assert!(!output.status.success(), "duplicate update must fail");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "UPDATE t SET id = 3 WHERE name = 'b'; SELECT * FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1	a"), "out: {text}");
    assert!(text.contains("3	b"), "out: {text}");
}

#[test]
fn table_level_unique_group_survives_reopen() {
    let dir = TempDir::new("sql-table-unique");
    let db = dir.join("u.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (a INT, b INT, x INT, UNIQUE (a, b));          INSERT INTO t VALUES (1,1,10),(1,2,20);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run(["sql", "--db", db, "--sql", "INSERT INTO t VALUES (1,1,30);"]);
    assert!(!output.status.success(), "duplicate tuple must fail");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "UPDATE t SET b = 2 WHERE a = 1 AND b = 1;",
    ]);
    assert!(!output.status.success(), "duplicate update must fail");

    // Reopen: the v2 TRSQ schema must still know the unique group.
    let output = run(["sql", "--db", db, "--sql", "INSERT INTO t VALUES (1,1,40);"]);
    assert!(
        !output.status.success(),
        "duplicate tuple must fail after reopen"
    );
}

#[test]
fn check_constraints_reject_false_and_accept_unknown() {
    let dir = TempDir::new("sql-check");
    let db = dir.join("c.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (x INT CHECK (x > 0), y INT, CHECK (y < 100));          INSERT INTO t VALUES (1, 10);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    for insert in [
        "INSERT INTO t VALUES (-1, 10);",
        "INSERT INTO t VALUES (1, 200);",
    ] {
        let output = run(["sql", "--db", db, "--sql", insert]);
        assert!(!output.status.success(), "must fail: {insert}");
    }

    // UNKNOWN passes SQL CHECK semantics.
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "INSERT INTO t VALUES (2, NULL);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "UPDATE t SET x = -2 WHERE x = 1;",
    ]);
    assert!(!output.status.success(), "update must fail");

    let output = run(["sql", "--db", db, "--sql", "SELECT * FROM t ORDER BY x;"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1	10"), "out: {text}");
    assert!(text.contains("2	NULL"), "out: {text}");
}

#[test]
fn annotation_columns_support_sum_avg_min_max() {
    let dir = TempDir::new("sql-ann-agg");
    let db = dir.join("a.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, conf LOGIC LUKASIEWICZ, n LOGIC NAT); \
         INSERT INTO t VALUES (1,200,3),(2,300,4),(3,NULL,5); \
         SELECT SUM(conf) AS s, AVG(conf) AS a, MIN(conf) AS lo, MAX(conf) AS hi FROM t; \
         SELECT SUM(n) AS s, AVG(n) AS a, MIN(n) AS lo, MAX(n) AS hi FROM t;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    for expected in ["0.500", "0.250", "0.200", "0.300"] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }
    assert!(text.contains("\n12\t4\t3\t5"), "out: {text}");
}

#[test]
fn prob_sum_multiplies_survival_probabilities() {
    let dir = TempDir::new("sql-prob-sum");
    let db = dir.join("p.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, conf LOGIC LUKASIEWICZ);          INSERT INTO t VALUES (1,200),(2,300),(3,NULL);          SELECT PROB_SUM(conf) AS p FROM t;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).contains("0.440"),
        "out: {}",
        stdout(&output)
    );
}

#[test]
fn with_recursive_reaches_a_fixpoint() {
    let dir = TempDir::new("sql-recursive");
    let db = dir.join("rec.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE edges (src INT NOT NULL, dst INT NOT NULL); \
         INSERT INTO edges VALUES (0,1),(1,2),(2,3);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH RECURSIVE reach(node) AS (\
           SELECT src FROM edges WHERE src = 0 \
           UNION SELECT e.dst FROM edges e JOIN reach r ON e.src = r.node\
         ) SELECT node FROM reach ORDER BY node;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    for expected in ["0", "1", "2", "3"] {
        assert!(
            text.lines().any(|line| line == expected),
            "missing {expected}: {text}"
        );
    }

    // UNION ALL walks a tree without revisiting nodes.
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE tree (parent INT NOT NULL, child INT NOT NULL); \
         INSERT INTO tree VALUES (0,1),(0,2),(1,3);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH RECURSIVE walk(node) AS (\
           SELECT child FROM tree WHERE parent = 0 \
           UNION ALL SELECT t.child FROM tree t JOIN walk w ON t.parent = w.node\
         ) SELECT node FROM walk ORDER BY node;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert_eq!(
        text.lines().filter(|line| *line == "1").count(),
        1,
        "UNION ALL must not duplicate the tree node: {text}"
    );
    assert!(text.lines().any(|line| line == "3"), "out: {text}");
}

#[test]
fn with_recursive_reports_column_and_fixpoint_errors() {
    let dir = TempDir::new("sql-recursive-errors");
    let db = dir.join("rec.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH RECURSIVE w(a, b) AS (SELECT id FROM t UNION SELECT id FROM t) SELECT a FROM w;",
    ]);
    assert!(!output.status.success(), "column mismatch must fail");
    assert!(stderr(&output).contains("declares 2 column(s)"));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH RECURSIVE w(n) AS (SELECT id FROM t UNION ALL SELECT n + 1 FROM w) SELECT n FROM w;",
    ]);
    assert!(!output.status.success(), "no fixpoint must fail");
    assert!(stderr(&output).contains("did not reach a fixed point"));
}

#[test]
fn with_recursive_supports_no_from_seed_and_nested_with() {
    let dir = TempDir::new("sql-recursive-gaps");
    let db = dir.join("gaps.db");
    let db = db.to_str().expect("path");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM nums WHERE n < 3)          SELECT n FROM nums ORDER BY n;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    for expected in ["1", "2", "3"] {
        assert!(
            text.lines().any(|line| line == expected),
            "missing {expected}: {text}"
        );
    }

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1),(2);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH a AS (WITH b AS (SELECT id FROM t) SELECT id FROM b)          SELECT id FROM a ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.lines().any(|line| line == "1"), "out: {text}");
    assert!(text.lines().any(|line| line == "2"), "out: {text}");
}

#[test]
fn with_dml_bodies_and_insert_select() {
    let dir = TempDir::new("sql-recursive-dml");
    let db = dir.join("dml.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1),(2);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE u (id INT NOT NULL);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH c AS (SELECT id FROM t WHERE id > 1) INSERT INTO u SELECT id FROM c;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("inserted 1 row(s) into u"));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH c AS (SELECT 1 AS x) UPDATE t SET id = id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("updated 2 row(s)"));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH c AS (SELECT 1 AS x) DELETE FROM t WHERE id = 1;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("deleted 1 row(s)"));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH c AS (SELECT 1 AS x) CREATE TABLE z (id INT);",
    ]);
    assert!(!output.status.success(), "WITH + DDL must fail");
    assert!(stderr(&output).contains("WITH must be followed by"));
}

#[test]
fn subqueries_in_where_and_projection() {
    let dir = TempDir::new("sql-subquery");
    let db = dir.join("subq.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1),(2),(3); \
         CREATE TABLE u (id INT NOT NULL); INSERT INTO u VALUES (2),(3);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT id FROM t WHERE id IN (SELECT id FROM u) ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.lines().any(|line| line == "2"), "out: {text}");
    assert!(text.lines().any(|line| line == "3"), "out: {text}");
    assert!(!text.lines().any(|line| line == "1"), "out: {text}");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT id FROM t WHERE id NOT IN (SELECT id FROM u) ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.lines().any(|line| line == "1"), "out: {text}");
    assert!(!text.lines().any(|line| line == "2"), "out: {text}");

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT id, (SELECT MAX(id) FROM u) AS m FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("1\t3"), "out: {text}");
    assert!(text.contains("2\t3"), "out: {text}");
}

#[test]
fn subqueries_connect_ctes_to_update_and_delete() {
    let dir = TempDir::new("sql-subquery");
    let db = dir.join("subq.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1),(2),(3); \
         CREATE TABLE u (id INT NOT NULL); INSERT INTO u VALUES (2),(3);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH c AS (SELECT id FROM u) UPDATE t SET id = id WHERE id IN (SELECT id FROM c);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("updated 2 row(s)"));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "WITH c AS (SELECT id FROM u WHERE id = 2) DELETE FROM t WHERE id IN (SELECT id FROM c);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("deleted 1 row(s)"));

    let output = run(["sql", "--db", db, "--sql", "SELECT id FROM t ORDER BY id;"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = stdout(&output);
    assert!(text.lines().any(|line| line == "1"), "out: {text}");
    assert!(text.lines().any(|line| line == "3"), "out: {text}");
    assert!(!text.lines().any(|line| line == "2"), "out: {text}");
}

#[test]
fn subquery_errors_are_reported() {
    let dir = TempDir::new("sql-subquery");
    let db = dir.join("subq.db");
    let db = db.to_str().expect("path");
    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL); INSERT INTO t VALUES (1),(2),(3); \
         CREATE TABLE u (id INT NOT NULL); INSERT INTO u VALUES (2),(3);",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT (SELECT id FROM u) AS m FROM t;",
    ]);
    assert!(!output.status.success(), "scalar multi-row must fail");
    assert!(stderr(&output).contains("more than one row"));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT id FROM t WHERE id IN (SELECT id, id FROM u);",
    ]);
    assert!(!output.status.success(), "IN multi-column must fail");
    assert!(stderr(&output).contains("exactly one column"));

    let output = run([
        "sql",
        "--db",
        db,
        "--sql",
        "SELECT (SELECT id FROM u WHERE id = 99) AS m;",
    ]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("NULL"));
}
