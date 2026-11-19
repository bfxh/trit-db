//! Page-store model tests, cache/reader stress, and checkpoint corruption tests.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::thread;

use tritdb_page::{Page, PageKind, MAX_PAYLOAD, PAGE_SIZE};
use tritdb_store::{PageId, PageStore, StoreError};
use tritdb_testkit::{Rng, TempDir};

fn payload(step: usize) -> Vec<u8> {
    format!("page-{step}-{}", "x".repeat(step % 17)).into_bytes()
}

fn verify_model(store: &PageStore, model: &BTreeMap<u64, (PageKind, Vec<u8>)>, context: &str) {
    assert_eq!(
        store.page_count(),
        model.len() as u64,
        "page_count at {context}"
    );
    for (id, (kind, expected)) in model {
        let page = store
            .read_page(PageId::new(*id))
            .unwrap_or_else(|error| panic!("read page {id} at {context}: {error}"));
        let page = page.unwrap_or_else(|| panic!("missing page {id} at {context}"));
        let view = Page::parse(page.as_bytes()).expect("parse page");
        assert_eq!(view.kind(), *kind, "kind at {context} page {id}");
        assert_eq!(
            view.payload(),
            expected.as_slice(),
            "payload at {context} page {id}"
        );
    }
}

#[test]
fn model_based_page_alloc_write_abort_checkpoint_reopen() {
    let dir = TempDir::new("store-model");
    let data = dir.join("model.pages");
    let wal = dir.join("model.wal");
    let mut store = PageStore::open(&data, &wal).expect("open");
    let mut model: BTreeMap<u64, (PageKind, Vec<u8>)> = BTreeMap::new();
    let mut next = 0u64;
    let mut rng = Rng::new(0x50E0_0000);

    for step in 0..160usize {
        match rng.below(7) {
            0..=2 => {
                let kind = match rng.below(3) {
                    0 => PageKind::Data,
                    1 => PageKind::Index,
                    _ => PageKind::Manifest,
                };
                let bytes = payload(step);
                let mut txn = store.begin().expect("begin");
                let id = txn.allocate(kind, &bytes).expect("allocate");
                assert_eq!(id.get(), next, "allocated id at step {step}");
                txn.commit().expect("commit");
                model.insert(id.get(), (kind, bytes));
                next += 1;
            }
            3..=4 if !model.is_empty() => {
                let mut ids: Vec<u64> = model.keys().copied().collect();
                let id = ids.swap_remove(rng.below(ids.len() as u32) as usize);
                let kind = match rng.below(3) {
                    0 => PageKind::Data,
                    1 => PageKind::Index,
                    _ => PageKind::Manifest,
                };
                let bytes = payload(step + 1000);
                let page = Page::new(kind, &bytes).expect("page");
                let mut txn = store.begin().expect("begin");
                txn.write_page(PageId::new(id), &page).expect("write");
                txn.commit().expect("commit");
                model.insert(id, (kind, bytes));
            }
            5 => {
                let kind = PageKind::Index;
                let bytes = payload(step + 2000);
                let mut txn = store.begin().expect("begin");
                let id = txn.allocate(kind, &bytes).expect("allocate");
                txn.abort().expect("abort");
                assert_eq!(id.get(), next, "aborted allocation must reuse id");
                assert_eq!(store.page_count(), next, "abort must not publish page");
            }
            _ => {
                store.checkpoint().expect("checkpoint");
                drop(store);
                store = PageStore::open(&data, &wal).expect("reopen");
            }
        }
        verify_model(&store, &model, &format!("step {step}"));
    }

    store.checkpoint().expect("final checkpoint");
    drop(store);
    let reopened = PageStore::open(&data, &wal).expect("final reopen");
    verify_model(&reopened, &model, "final");
}

#[test]
fn aborted_write_keeps_previous_page_content() {
    let dir = TempDir::new("store-abort-write");
    let data = dir.join("abort.pages");
    let wal = dir.join("abort.wal");
    let store = PageStore::open(&data, &wal).expect("open");
    let mut txn = store.begin().expect("begin");
    let id = txn.allocate(PageKind::Data, b"old").expect("allocate");
    txn.commit().expect("commit");

    let mut txn = store.begin().expect("begin");
    let page = Page::new(PageKind::Index, b"new").expect("page");
    txn.write_page(id, &page).expect("write");
    txn.abort().expect("abort");
    let page = store.read_page(id).expect("read").expect("page");
    let view = Page::parse(page.as_bytes()).expect("parse");
    assert_eq!(view.kind(), PageKind::Data);
    assert_eq!(view.payload(), b"old");
}

#[test]
fn page_payload_boundaries_roundtrip() {
    let dir = TempDir::new("store-boundaries");
    let data = dir.join("bounds.pages");
    let wal = dir.join("bounds.wal");
    let store = PageStore::open(&data, &wal).expect("open");
    let mut txn = store.begin().expect("begin");
    let zero = txn.allocate(PageKind::Data, b"").expect("empty");
    let max = txn
        .allocate(PageKind::Manifest, &vec![0x5a; MAX_PAYLOAD])
        .expect("max");
    txn.commit().expect("commit");
    for (id, expected) in [(zero, 0usize), (max, MAX_PAYLOAD)] {
        let page = store.read_page(id).expect("read").expect("page");
        let view = Page::parse(page.as_bytes()).expect("parse");
        assert_eq!(view.payload_len(), expected);
    }
}

#[test]
fn checkpoint_sidecar_truncation_is_rejected() {
    let dir = TempDir::new("store-ckpt-trunc");
    let data = dir.join("ckpt.pages");
    let wal = dir.join("ckpt.wal");
    let store = PageStore::open(&data, &wal).expect("open");
    let mut txn = store.begin().expect("begin");
    txn.allocate(PageKind::Data, b"one").expect("allocate");
    txn.commit().expect("commit");
    store.checkpoint().expect("checkpoint");
    let sidecar = store.checkpoint_path().to_path_buf();
    let raw = fs::read(&sidecar).expect("read sidecar");
    drop(store);

    for cut in 0..raw.len() {
        fs::write(&sidecar, &raw[..cut]).expect("write truncated sidecar");
        let error = PageStore::open(&data, &wal)
            .err()
            .expect("truncated sidecar");
        assert!(
            matches!(error, StoreError::BadCheckpoint { .. }),
            "cut {cut}: {error:?}"
        );
    }
    fs::write(&sidecar, &raw).expect("restore sidecar");
    PageStore::open(&data, &wal).expect("valid sidecar must open");
}

#[test]
fn checkpoint_sidecar_crc_corruption_is_rejected() {
    let dir = TempDir::new("store-ckpt-crc");
    let data = dir.join("ckpt.pages");
    let wal = dir.join("ckpt.wal");
    let store = PageStore::open(&data, &wal).expect("open");
    let mut txn = store.begin().expect("begin");
    txn.allocate(PageKind::Data, b"one").expect("allocate");
    txn.commit().expect("commit");
    store.checkpoint().expect("checkpoint");
    let sidecar = store.checkpoint_path().to_path_buf();
    let raw = fs::read(&sidecar).expect("read sidecar");
    drop(store);

    for index in 0..raw.len() {
        let mut mutated = raw.clone();
        mutated[index] ^= 0xff;
        fs::write(&sidecar, &mutated).expect("write mutated sidecar");
        let error = PageStore::open(&data, &wal)
            .err()
            .expect("corrupted sidecar");
        assert!(
            matches!(error, StoreError::BadCheckpoint { .. }),
            "byte {index}: {error:?}"
        );
    }
    fs::write(&sidecar, &raw).expect("restore sidecar");
    PageStore::open(&data, &wal).expect("valid sidecar must open");
}

#[test]
fn data_tail_truncation_never_loses_committed_pages() {
    let dir = TempDir::new("store-tail");
    let data = dir.join("tail.pages");
    let wal = dir.join("tail.wal");
    let store = PageStore::open(&data, &wal).expect("open");
    let mut txn = store.begin().expect("begin");
    for index in 0..3u64 {
        txn.allocate(PageKind::Data, format!("page-{index}").as_bytes())
            .expect("allocate");
    }
    txn.commit().expect("commit");
    drop(store);

    let raw = fs::read(&data).expect("read data");
    assert_eq!(raw.len(), 3 * PAGE_SIZE);
    for cut in [
        PAGE_SIZE - 1,
        PAGE_SIZE,
        PAGE_SIZE + 1,
        PAGE_SIZE + 123,
        2 * PAGE_SIZE - 1,
        2 * PAGE_SIZE,
        2 * PAGE_SIZE + 7,
        3 * PAGE_SIZE - 1,
    ] {
        fs::write(&data, &raw[..cut]).expect("write truncated data");
        let reopened = PageStore::open(&data, &wal).expect("recover from WAL");
        assert_eq!(reopened.page_count(), 3, "cut {cut}");
        assert!(reopened.recovery().data_tail_bytes > 0 || cut % PAGE_SIZE == 0);
        for id in 0..3 {
            let page = reopened
                .read_page(PageId::new(id))
                .expect("read")
                .expect("page");
            assert_eq!(
                Page::parse(page.as_bytes()).expect("parse").payload(),
                format!("page-{id}").as_bytes()
            );
        }
        drop(reopened);
        fs::write(&data, &raw).expect("restore data");
    }
}

#[test]
fn readers_see_monotonic_page_count_and_valid_pages() {
    let dir = TempDir::new("store-readers");
    let data = dir.join("readers.pages");
    let wal = dir.join("readers.wal");
    let store = Arc::new(PageStore::open(&data, &wal).expect("open"));
    let reader_count = 4usize;
    let reads = 500usize;
    let mut readers = Vec::new();
    for _ in 0..reader_count {
        let store = Arc::clone(&store);
        readers.push(thread::spawn(move || {
            let mut last = 0u64;
            for _ in 0..reads {
                let count = store.page_count();
                assert!(
                    count >= last,
                    "page_count went backwards: {last} -> {count}"
                );
                last = count;
                if count > 0 {
                    let id = count - 1;
                    if let Some(page) = store.read_page(PageId::new(id)).expect("read") {
                        let view = Page::parse(page.as_bytes()).expect("parse");
                        assert!(!view.payload().is_empty());
                    }
                }
            }
        }));
    }

    let writer_store = Arc::clone(&store);
    let writer = thread::spawn(move || {
        for index in 0..200u64 {
            let mut txn = writer_store.begin().expect("begin");
            txn.allocate(PageKind::Data, format!("payload-{index}").as_bytes())
                .expect("allocate");
            txn.commit().expect("commit");
        }
    });
    writer.join().expect("writer");
    for reader in readers {
        reader.join().expect("reader");
    }
    assert_eq!(store.page_count(), 200);
    for id in 0..200 {
        let page = store
            .read_page(PageId::new(id))
            .expect("read")
            .expect("page");
        let view = Page::parse(page.as_bytes()).expect("parse");
        assert_eq!(view.payload(), format!("payload-{id}").as_bytes());
    }
}

#[test]
fn empty_store_checkpoint_and_reopen() {
    let dir = TempDir::new("store-empty");
    let data = dir.join("empty.pages");
    let wal = dir.join("empty.wal");
    let store = PageStore::open(&data, &wal).expect("open");
    let info = store.checkpoint().expect("checkpoint");
    assert_eq!(info.page_count, 0);
    drop(store);
    let reopened = PageStore::open(&data, &wal).expect("reopen");
    assert_eq!(reopened.page_count(), 0);
    assert_eq!(reopened.recovery().data_pages, 0);
}

#[test]
fn checkpoint_rejects_page_count_larger_than_data() {
    let dir = TempDir::new("store-page-count");
    let data = dir.join("count.pages");
    let wal = dir.join("count.wal");
    let store = PageStore::open(&data, &wal).expect("open");
    let mut txn = store.begin().expect("begin");
    txn.allocate(PageKind::Data, b"one").expect("allocate");
    txn.commit().expect("commit");
    store.checkpoint().expect("checkpoint");
    drop(store);

    let raw = fs::read(&data).expect("read data");
    fs::write(&data, &raw[..PAGE_SIZE / 2]).expect("truncate data");
    let error = PageStore::open(&data, &wal)
        .err()
        .expect("checkpoint page count mismatch");
    assert!(matches!(
        error,
        StoreError::CheckpointPageCount {
            checkpoint: 1,
            data: 0
        }
    ));
}
