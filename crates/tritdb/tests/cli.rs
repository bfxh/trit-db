//! CLI integration tests. These exercise the same binary users run, not the
//! library API, and cover paths with spaces plus failure exit codes.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tritdb_format::Block;
use tritdb_page::{Page, PageKind, PAGE_SIZE};
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

fn assert_success(output: &Output) -> String {
    assert!(
        output.status.success(),
        "CLI failed: status={:?}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout(output),
        stderr(output)
    );
    stdout(output)
}

#[test]
fn help_radix_and_error_exit_codes() {
    let output = run(["help"]);
    assert!(output.status.success());
    let text = stdout(&output);
    assert!(text.contains("tritdb M0"));
    assert!(text.contains("pstore checkpoint"));

    let output = run(["radix", "--trits", "4096"]);
    let text = assert_success(&output);
    assert_eq!(text.lines().count(), 3, "one line per radix: {text}");
    assert!(text.contains("r3"));
    assert!(text.contains("r4"));
    assert!(text.contains("r16"));
    assert_eq!(text.matches("roundtrip=ok").count(), 3);
    assert!(text.contains("bytes=820"), "r3 byte count: {text}");

    let output = run(["radix", "--trits", "0"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("must be greater than 0"));

    let output = run(["definitely-not-a-command"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unknown subcommand"));
}

#[test]
fn block_and_page_cli_roundtrip_with_spaces_in_paths() {
    let dir = TempDir::new("cli-files");
    let spaced = dir.join("with space");
    fs::create_dir(&spaced).expect("create spaced dir");
    let block_path = spaced.join("block.trbl");
    let page_path = spaced.join("page.bin");

    let output = run([
        "block",
        "make",
        "--out",
        block_path.to_str().expect("path"),
        "--radix",
        "r4",
        "--trits",
        "4096",
    ]);
    let text = assert_success(&output);
    assert!(text.contains("radix=r4"));
    let raw = fs::read(&block_path).expect("read block");
    let decoded = Block::decode(&raw).expect("decode block");
    assert_eq!(decoded.trit_count(), 4096);

    let output = run(["block", "check", "--in", block_path.to_str().expect("path")]);
    let text = assert_success(&output);
    assert!(text.contains("crc=ok"));
    assert!(text.contains("trits_roundtrip=ok"));

    let output = run([
        "page",
        "make",
        "--out",
        page_path.to_str().expect("path"),
        "--kind",
        "manifest",
        "--text",
        "hello world",
    ]);
    assert_success(&output);
    let raw = fs::read(&page_path).expect("read page");
    let page = Page::parse_slice(&raw).expect("parse page");
    assert_eq!(page.kind(), PageKind::Manifest);
    assert_eq!(page.payload(), b"hello world");
    assert_eq!(raw.len(), PAGE_SIZE);

    let output = run(["page", "check", "--in", page_path.to_str().expect("path")]);
    let text = assert_success(&output);
    assert!(text.contains("kind=manifest"));
    assert!(text.contains("checksum=ok"));

    let output = run([
        "page",
        "check",
        "--in",
        Path::new("missing").to_str().unwrap(),
    ]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("read missing"));
}

#[test]
fn txn_cli_put_get_dump_inspect_and_checkpoint() {
    let dir = TempDir::new("cli-txn");
    let wal = dir.join("cli.wal");
    let wal_text = wal.to_str().expect("wal path");

    let output = run([
        "txn", "put", "--wal", wal_text, "--key", "alpha", "--value", "one",
    ]);
    let text = assert_success(&output);
    assert!(text.contains("ops=1"), "{text}");

    let output = run(["txn", "get", "--wal", wal_text, "--key", "alpha"]);
    let text = assert_success(&output);
    assert!(text.contains("value=\"one\""), "{text}");

    let output = run(["txn", "dump", "--wal", wal_text]);
    let text = assert_success(&output);
    assert!(text.contains("committed_txns=1"), "{text}");
    assert!(text.contains("keys=1"), "{text}");

    let output = run(["wal", "inspect", "--in", wal_text]);
    let text = assert_success(&output);
    assert!(text.contains("records=3"), "{text}");
    assert!(text.contains("tail=clean"), "{text}");

    let output = run(["txn", "checkpoint", "--wal", wal_text]);
    let text = assert_success(&output);
    assert!(text.contains("entries=1"), "{text}");
    assert_eq!(fs::metadata(&wal).expect("wal metadata").len(), 32);
    assert!(dir.join("cli.wal.snap").exists());

    let output = run(["txn", "get", "--wal", wal_text, "--key", "alpha"]);
    let text = assert_success(&output);
    assert!(
        text.contains("value=\"one\""),
        "value after checkpoint: {text}"
    );

    let output = run(["txn", "get", "--wal", wal_text, "--key", "missing"]);
    let text = assert_success(&output);
    assert!(text.contains("not_found"), "{text}");
}

#[test]
fn pstore_cli_alloc_get_put_dump_checkpoint() {
    let dir = TempDir::new("cli-pstore");
    let data = dir.join("cli.pages");
    let wal = dir.join("cli.wal");
    let data_text = data.to_str().expect("data path");
    let wal_text = wal.to_str().expect("wal path");

    let output = run([
        "pstore", "alloc", "--data", data_text, "--wal", wal_text, "--kind", "data", "--text",
        "hello",
    ]);
    let text = assert_success(&output);
    assert!(text.contains("page=0"), "{text}");
    assert_eq!(
        fs::metadata(&data).expect("data metadata").len(),
        PAGE_SIZE as u64
    );

    let output = run([
        "pstore", "get", "--data", data_text, "--wal", wal_text, "--id", "0",
    ]);
    let text = assert_success(&output);
    assert!(text.contains("payload=\"hello\""), "{text}");

    let output = run(["pstore", "dump", "--data", data_text, "--wal", wal_text]);
    let text = assert_success(&output);
    assert!(text.contains("page_count=1"), "{text}");

    let output = run([
        "pstore", "put", "--data", data_text, "--wal", wal_text, "--id", "0", "--text", "updated",
    ]);
    let text = assert_success(&output);
    assert!(text.contains("pages=1"), "{text}");

    let output = run([
        "pstore", "get", "--data", data_text, "--wal", wal_text, "--id", "0",
    ]);
    let text = assert_success(&output);
    assert!(text.contains("payload=\"updated\""), "{text}");

    let output = run([
        "pstore",
        "checkpoint",
        "--data",
        data_text,
        "--wal",
        wal_text,
    ]);
    let text = assert_success(&output);
    assert!(text.contains("page_count=1"), "{text}");
    assert_eq!(fs::metadata(&wal).expect("wal metadata").len(), 32);
    assert!(dir.join("cli.pages.ckpt").exists());

    let output = run([
        "pstore", "get", "--data", data_text, "--wal", wal_text, "--id", "99",
    ]);
    let text = assert_success(&output);
    assert!(text.contains("not_found"), "{text}");

    let output = run([
        "pstore", "put", "--data", data_text, "--wal", wal_text, "--id", "99", "--text", "missing",
    ]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("does not exist"));
}

#[test]
fn serve_rejects_incomplete_auth_and_tls_flags() {
    let dir = TempDir::new("cli-serve-flags");
    let db = dir.join("sql.db");
    let db = db.to_str().expect("path");

    // `--auth password` needs a password source.
    let output = run(["serve", "--db", db, "--auth", "password"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("requires --password"),
        "stderr: {}",
        stderr(&output)
    );

    // Unknown auth mode.
    let output = run(["serve", "--db", db, "--auth", "md5"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("unknown --auth"),
        "stderr: {}",
        stderr(&output)
    );

    // TLS needs both halves of the key pair.
    let output = run(["serve", "--db", db, "--tls-cert", "cert.pem"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("must be given together"),
        "stderr: {}",
        stderr(&output)
    );

    // `--require-tls` without a certificate is rejected.
    let output = run(["serve", "--db", db, "--require-tls"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--require-tls needs"),
        "stderr: {}",
        stderr(&output)
    );
}
