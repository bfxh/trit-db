//! CLI smoke tests for content-addressed storage and manifest chains.

use std::ffi::OsStr;
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

fn field<'a>(text: &'a str, name: &str) -> &'a str {
    let needle = format!("{name}=");
    text.split_whitespace()
        .find_map(|token| token.strip_prefix(&needle))
        .unwrap_or_else(|| panic!("missing {needle} in {text}"))
}

#[test]
fn cas_cli_puts_gets_and_verifies_a_chain() {
    let dir = TempDir::new("cas-cli");
    let store = dir.join("store");
    let store_text = store.to_str().expect("store path");
    let blob = dir.join("blob.bin");
    std::fs::write(&blob, b"hello").expect("write blob");
    let blob_text = blob.to_str().expect("blob path");

    let output = run(["cas", "put", "--store", store_text, "--file", blob_text]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let hash = field(&stdout(&output), "hash").to_string();

    let output = run([
        "cas",
        "get",
        "--store",
        store_text,
        "--hash",
        &hash,
        "--out",
        dir.join("copy.bin").to_str().expect("out path"),
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert_eq!(std::fs::read(dir.join("copy.bin")).expect("copy"), b"hello");

    let entry = format!("blob={hash}");
    let output = run([
        "manifest", "commit", "--store", store_text, "--entry", &entry,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let manifest = field(&stdout(&output), "manifest").to_string();

    let output = run([
        "manifest", "verify", "--store", store_text, "--head", &manifest,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert!(stdout(&output).contains("length=1"));
}

#[test]
fn manifest_verify_rejects_a_tampered_manifest() {
    let dir = TempDir::new("cas-tamper");
    let store = dir.join("store");
    let store_text = store.to_str().expect("store path");
    let output = run([
        "manifest",
        "commit",
        "--store",
        store_text,
        "--entry",
        &format!("empty={}", "0".repeat(64)),
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let manifest = field(&stdout(&output), "manifest").to_string();

    let path = store
        .join("chunks")
        .join(&manifest[..2])
        .join(&manifest[2..]);
    std::fs::write(&path, b"tampered").expect("tamper manifest");

    let output = run([
        "manifest", "verify", "--store", store_text, "--head", &manifest,
    ]);
    assert!(!output.status.success(), "tampered manifest must fail");
}

#[test]
fn cas_gc_removes_orphans_and_replicate_copies_the_chain() {
    let dir = TempDir::new("cas-gc-cli");
    let store = dir.join("store");
    let store_text = store.to_str().expect("store path");

    let blob = dir.join("blob.bin");
    std::fs::write(&blob, b"payload").expect("write blob");
    let output = run([
        "cas",
        "put",
        "--store",
        store_text,
        "--file",
        blob.to_str().expect("blob path"),
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let blob_hash = field(&stdout(&output), "hash").to_string();

    let orphan = dir.join("orphan.bin");
    std::fs::write(&orphan, b"orphan").expect("write orphan");
    let output = run([
        "cas",
        "put",
        "--store",
        store_text,
        "--file",
        orphan.to_str().expect("orphan path"),
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let orphan_hash = field(&stdout(&output), "hash").to_string();

    let entry = format!("blob={blob_hash}");
    let output = run([
        "manifest", "commit", "--store", store_text, "--entry", &entry,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let manifest = field(&stdout(&output), "manifest").to_string();

    let output = run(["cas", "gc", "--store", store_text, "--root", &manifest]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert!(stdout(&output).contains("removed=1"));

    let output = run(["cas", "has", "--store", store_text, "--hash", &orphan_hash]);
    assert!(stdout(&output).contains("present=false"));
    let output = run(["cas", "has", "--store", store_text, "--hash", &blob_hash]);
    assert!(stdout(&output).contains("present=true"));

    let replica = dir.join("replica");
    let output = run([
        "manifest",
        "replicate",
        "--from",
        store_text,
        "--to",
        replica.to_str().expect("replica path"),
        "--head",
        &manifest,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert!(stdout(&output).contains("manifests=1"));

    let output = run([
        "manifest",
        "verify",
        "--store",
        replica.to_str().expect("replica path"),
        "--head",
        &manifest,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
}

#[test]
fn manifest_snapshot_and_restore_roundtrip() {
    let dir = TempDir::new("cas-snapshot");
    let db = dir.join("sql.db");
    let db_text = db.to_str().expect("db path");
    let store = dir.join("cas");
    let store_text = store.to_str().expect("store path");

    let output = run([
        "sql",
        "--db",
        db_text,
        "--sql",
        "CREATE TABLE t (id INT NOT NULL, name TEXT);          INSERT INTO t VALUES (1,'a'),(2,'b');",
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);

    let output = run([
        "manifest", "snapshot", "--store", store_text, "--db", db_text,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let head = field(&stdout(&output), "manifest").to_string();

    // A second snapshot links to the first.
    let output = run([
        "manifest", "snapshot", "--store", store_text, "--db", db_text, "--parent", &head,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let head2 = field(&stdout(&output), "manifest").to_string();
    let output = run([
        "manifest", "verify", "--store", store_text, "--head", &head2,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    assert!(stdout(&output).contains("length=2"));

    std::fs::remove_file(&db).expect("remove original db");
    let restored = dir.join("restored.db");
    let restored_text = restored.to_str().expect("restored path");
    let output = run([
        "manifest",
        "restore",
        "--store",
        store_text,
        "--head",
        &head,
        "--db",
        restored_text,
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);

    let output = run([
        "sql",
        "--db",
        restored_text,
        "--sql",
        "SELECT id, name FROM t ORDER BY id;",
    ]);
    assert!(output.status.success(), "stderr: {:?}", output);
    let text = stdout(&output);
    assert!(text.contains("1	a"), "out: {text}");
    assert!(text.contains("2	b"), "out: {text}");
}
