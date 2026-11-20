//! Hard-kill timing matrix: kill the writer at several points between commit
//! groups and require exact recovery of every complete transaction.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use tritdb_testkit::{Rng, TempDir};
use tritdb_txn::TxnStore;

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
fn hard_kill_delay_matrix_recovers_exact_committed_groups() {
    let exe = env!("CARGO_BIN_EXE_tritdb");
    // 100 hard kills at deterministic pseudo-random delays. The delay is
    // capped so the matrix stays within a few seconds on CI.
    let mut rng = Rng::new(0xC0FF_EE00);
    for round in 0..100 {
        let delay_ms = rng.below(60) as u64;
        let dir = TempDir::new("crash-matrix");
        let wal = dir.join("matrix.wal");
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
        assert!(first.starts_with("committed=1 "), "first line: {first}");
        thread::sleep(Duration::from_millis(delay_ms));
        child.kill().expect("hard kill");
        let status = child.wait().expect("wait");
        assert!(
            !status.success(),
            "round {round} delay {delay_ms}: writer survived"
        );

        let store = TxnStore::open(&wal).expect("recover");
        let recovery = store.recovery().clone();
        let entries = store.iter().expect("iter");
        let mut groups: BTreeMap<u64, usize> = BTreeMap::new();
        for (key, value) in entries.iter() {
            let (txn, index) = parse_key(key);
            assert_eq!(
                value,
                format!("v{txn}-{index}").as_bytes(),
                "round {round} delay {delay_ms}: corrupted value for {key:?}"
            );
            *groups.entry(txn).or_insert(0) += 1;
        }
        assert!(
            recovery.committed_txns >= 1,
            "round {round} delay {delay_ms}: no committed transaction"
        );
        assert!(
            recovery.discarded_txns <= 1,
            "round {round} delay {delay_ms}: discarded={}",
            recovery.discarded_txns
        );
        assert_eq!(
            groups.len(),
            recovery.committed_txns,
            "round {round} delay {delay_ms}: groups={} committed={}",
            groups.len(),
            recovery.committed_txns
        );
        for (txn, count) in groups {
            assert_eq!(
                count, 4,
                "round {round} delay {delay_ms}: txn {txn} has {count} keys"
            );
        }
        println!(
            "round={round} delay={delay_ms} committed={} discarded={} torn={} keys={}",
            recovery.committed_txns,
            recovery.discarded_txns,
            recovery.torn_tail_bytes,
            entries.len()
        );
    }
}
