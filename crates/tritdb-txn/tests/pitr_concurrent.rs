//! `recover_at` must tolerate a live writer appending to the WAL.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use tritdb_testkit::TempDir;
use tritdb_txn::TxnStore;

#[test]
fn recover_at_is_safe_while_a_writer_keeps_appending() {
    let dir = TempDir::new("txn-pitr-concurrent");
    let wal = dir.join("pitr.wal");
    let store = Arc::new(TxnStore::open(&wal).expect("open"));

    // Seed a few commits and archive them, so recovery also reads an archive.
    for i in 0..5 {
        let mut txn = store.begin().expect("begin");
        txn.put(format!("seed{i}").as_bytes(), b"v").expect("put");
        txn.commit().expect("commit");
    }
    store.checkpoint_archive().expect("archive seed");

    let writer = {
        let store = Arc::clone(&store);
        thread::spawn(move || {
            for i in 0..200 {
                let mut txn = store.begin().expect("begin");
                txn.put(format!("k{i}").as_bytes(), b"v").expect("put");
                txn.commit().expect("commit");
                if i % 10 == 0 {
                    thread::sleep(Duration::from_millis(1));
                }
            }
        })
    };

    let mut last = 0usize;
    for _ in 0..60 {
        let view = TxnStore::recover_at(&wal, u64::MAX).expect("recover under load");
        assert!(
            view.entries.len() >= last,
            "entries shrank: {last} -> {}",
            view.entries.len()
        );
        last = view.entries.len();
        thread::sleep(Duration::from_millis(2));
    }

    writer.join().expect("writer");
    let view = TxnStore::recover_at(&wal, u64::MAX).expect("final recover");
    assert_eq!(view.entries.len(), 205);
}
