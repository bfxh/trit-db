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
