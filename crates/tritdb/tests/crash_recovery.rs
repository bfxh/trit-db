use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use tritdb_txn::TxnStore;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tritdb-crash-test-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn wal(&self) -> PathBuf {
        self.0.join("crash.wal")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn parse_key(key: &[u8]) -> (u64, usize) {
    let text = std::str::from_utf8(key).expect("utf8 key");
    let (txn, index) = text.split_once('-').expect("key format k<txn>-<i>");
    let txn = txn
        .strip_prefix('k')
        .expect("key prefix")
        .parse()
        .expect("txn id");
    let index = index.parse().expect("key index");
    (txn, index)
}

#[test]
fn hard_kill_recovers_only_complete_committed_groups() {
    let exe = env!("CARGO_BIN_EXE_tritdb");
    let dir = TempDir::new();
    let wal = dir.wal();

    let mut child = Command::new(exe)
        .arg("txn")
        .arg("writer")
        .arg("--wal")
        .arg(&wal)
        .arg("--txns")
        .arg("2000")
        .arg("--keys-per-txn")
        .arg("4")
        .arg("--sleep-ms")
        .arg("1")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn writer");

    let stdout = child.stdout.take().expect("piped stdout");
    let mut lines = BufReader::new(stdout).lines();
    let first = lines
        .next()
        .expect("writer produced no output")
        .expect("read first line");
    assert!(
        first.starts_with("committed=1 "),
        "unexpected first line: {first}"
    );

    thread::sleep(Duration::from_millis(30));
    child.kill().expect("hard-kill writer");
    let status = child.wait().expect("wait for killed writer");
    assert!(!status.success(), "writer should have been killed");

    let recovery = {
        let store = TxnStore::open(&wal).expect("recover after hard kill");
        let recovery = store.recovery().clone();
        assert!(
            recovery.committed_txns >= 1,
            "no committed transaction survived"
        );
        assert!(
            recovery.discarded_txns <= 1,
            "more than one uncommitted transaction was discarded: {}",
            recovery.discarded_txns
        );

        let entries = store.iter().expect("read recovered entries");
        let mut groups: BTreeMap<u64, usize> = BTreeMap::new();
        for (key, value) in entries.iter() {
            let (txn, index) = parse_key(key);
            let expected = format!("v{txn}-{index}");
            assert_eq!(value, expected.as_bytes(), "value mismatch for key {key:?}");
            *groups.entry(txn).or_insert(0) += 1;
        }

        println!(
            "recovered committed={} discarded={} aborted={} torn_bytes={} keys={} groups={}",
            recovery.committed_txns,
            recovery.discarded_txns,
            recovery.aborted_txns,
            recovery.torn_tail_bytes,
            entries.len(),
            groups.len()
        );

        assert_eq!(
            groups.len(),
            recovery.committed_txns,
            "recovered key groups do not match committed transaction count"
        );
        for (txn, count) in groups {
            assert_eq!(
                count, 4,
                "transaction {txn} recovered {count} keys, expected 4"
            );
        }
        recovery
    };

    assert!(
        recovery.discarded_txns <= 1,
        "more than one uncommitted transaction was discarded: {}",
        recovery.discarded_txns
    );

    drop(dir);
    assert!(!Path::new(&wal).exists());
}
