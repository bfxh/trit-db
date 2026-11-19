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
