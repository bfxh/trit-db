//! Ignored page-store stress tests for the nightly pipeline.

use std::sync::Arc;
use std::thread;

use tritdb_page::{Page, PageKind};
use tritdb_store::{PageId, PageStore};
use tritdb_testkit::TempDir;

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "nightly stress: many readers during page commits"]
fn many_readers_during_commits() {
    let dir = TempDir::new("store-stress-readers");
    let data = dir.join("stress.pages");
    let wal = dir.join("stress.wal");
    let store = Arc::new(PageStore::open(&data, &wal).expect("open"));
    let pages = env_usize("TRITDB_STRESS_PAGES", 512);
    let readers = env_usize("TRITDB_STRESS_READERS", 8);
    let mut handles = Vec::new();

    for _ in 0..readers {
        let store = Arc::clone(&store);
        handles.push(thread::spawn(move || {
            let mut checked = 0usize;
            for _ in 0..pages * 4 {
                let count = store.page_count();
                if count == 0 {
                    continue;
                }
                let id = (checked as u64) % count;
                if let Some(page) = store.read_page(PageId::new(id)).expect("read") {
                    let view = Page::parse(page.as_bytes()).expect("parse");
                    assert!(!view.payload().is_empty());
                    checked += 1;
                }
            }
        }));
    }

    for index in 0..pages {
        let mut txn = store.begin().expect("begin");
        txn.allocate(PageKind::Data, format!("page-{index}").as_bytes())
            .expect("allocate");
        txn.commit().expect("commit");
    }
    for handle in handles {
        handle.join().expect("reader");
    }
    assert_eq!(store.page_count(), pages as u64);
    store.checkpoint().expect("checkpoint");
    drop(store);
    let reopened = PageStore::open(&data, &wal).expect("reopen");
    assert_eq!(reopened.page_count(), pages as u64);
}

#[test]
#[ignore = "nightly stress: repeated checkpoints"]
fn repeated_checkpoints() {
    let dir = TempDir::new("store-stress-checkpoint");
    let data = dir.join("stress.pages");
    let wal = dir.join("stress.wal");
    let store = PageStore::open(&data, &wal).expect("open");
    let rounds = env_usize("TRITDB_STRESS_ROUNDS", 100);
    for index in 0..rounds {
        let mut txn = store.begin().expect("begin");
        txn.allocate(PageKind::Index, format!("p{index}").as_bytes())
            .expect("allocate");
        txn.commit().expect("commit");
        store.checkpoint().expect("checkpoint");
    }
    drop(store);
    let reopened = PageStore::open(&data, &wal).expect("reopen");
    assert_eq!(reopened.page_count(), rounds as u64);
}
