//! CLI smoke tests for archived checkpoints and point-in-time recovery.

use std::ffi::OsStr;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

fn field<'a>(text: &'a str, name: &str) -> &'a str {
    let needle = format!("{name}=");
    text.split_whitespace()
        .find_map(|token| token.strip_prefix(&needle))
        .unwrap_or_else(|| panic!("missing {needle} in {text}"))
}

#[test]
fn pitr_cli_archives_and_recovers_an_earlier_state() {
    let dir = TempDir::new("pitr-cli");
    let wal = dir.join("t.wal");
    let wal = wal.to_str().expect("path");

    for i in 0..3 {
        let output = run([
            "txn",
            "put",
            "--wal",
            wal,
            "--key",
            &format!("k{i}"),
            "--value",
            "v1",
        ]);
        assert!(output.status.success(), "put failed: {:?}", output);
    }

    let output = run(["txn", "checkpoint", "--wal", wal, "--archive"]);
    assert!(output.status.success(), "checkpoint failed: {:?}", output);
    let text = stdout(&output);
    let checkpoint_lsn: u64 = field(&text, "checkpoint_lsn").parse().expect("lsn");
    assert_ne!(field(&text, "archived_wal"), "-");

    for i in 0..3 {
        let output = run([
            "txn",
            "put",
            "--wal",
            wal,
            "--key",
            &format!("k{i}"),
            "--value",
            "v2",
        ]);
        assert!(output.status.success(), "put failed: {:?}", output);
    }

    let output = run(["txn", "history", "--wal", wal]);
    assert!(output.status.success(), "history failed: {:?}", output);
    let history = stdout(&output);
    assert!(
        history.contains(&format!("lsn={checkpoint_lsn} ")),
        "history: {history}"
    );

    let target = checkpoint_lsn.to_string();
    let output = run(["txn", "recover", "--wal", wal, "--to-lsn", &target]);
    assert!(output.status.success(), "recover failed: {:?}", output);
    let recovered = stdout(&output);
    assert_eq!(field(&recovered, "keys"), "3");
    assert!(
        recovered.contains(r#"key="k0" value="v1""#),
        "out: {recovered}"
    );
    assert!(
        recovered.contains(r#"key="k2" value="v1""#),
        "out: {recovered}"
    );

    let output = run(["txn", "recover", "--wal", wal, "--to-lsn", "0"]);
    assert!(!output.status.success(), "target 0 must be rejected");
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis() as u64
}

#[test]
fn pitr_cli_recovers_by_timestamp() {
    let dir = TempDir::new("pitr-cli-time");
    let wal = dir.join("t.wal");
    let wal = wal.to_str().expect("path");

    for i in 0..3 {
        let output = run([
            "txn",
            "put",
            "--wal",
            wal,
            "--key",
            &format!("k{i}"),
            "--value",
            "v1",
        ]);
        assert!(output.status.success(), "put failed: {:?}", output);
    }
    let timestamp = now_ms();
    thread::sleep(Duration::from_millis(10));

    for i in 0..3 {
        let output = run([
            "txn",
            "put",
            "--wal",
            wal,
            "--key",
            &format!("k{i}"),
            "--value",
            "v2",
        ]);
        assert!(output.status.success(), "put failed: {:?}", output);
    }

    let target = timestamp.to_string();
    let output = run(["txn", "recover", "--wal", wal, "--to-time-ms", &target]);
    assert!(output.status.success(), "recover failed: {:?}", output);
    let recovered = stdout(&output);
    assert_eq!(field(&recovered, "keys"), "3");
    assert!(
        recovered.contains(r#"key="k0" value="v1""#),
        "out: {recovered}"
    );
}
