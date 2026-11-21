//! Page-store matrix: allocation order, overwrite, abort, checkpoint and
//! reopen.

use tritdb_page::{Page, PageKind};
use tritdb_store::{PageId, PageStore};
use tritdb_testkit::TempDir;

fn open(name: &str) -> (TempDir, PageStore) {
    let dir = TempDir::new(name);
    let data = dir.join("pstore.pages");
    let wal = dir.join("pstore.wal");
    let store = PageStore::open(&data, &wal).expect("open page store");
    (dir, store)
}

fn payload(len: usize, byte: u8) -> Vec<u8> {
    vec![byte; len]
}

fn read_view(store: &PageStore, id: PageId) -> (PageKind, Vec<u8>) {
    let page = store
        .read_page(id)
        .expect("read page")
        .unwrap_or_else(|| panic!("missing page {}", id.get()));
    let view = Page::parse(page.as_bytes()).expect("parse page");
    (view.kind(), view.payload().to_vec())
}

#[test]
fn allocate_write_read_matrix() {
    let (_dir, store) = open("pstore-matrix-alloc");
    let first;
    let second;
    let third;
    {
        let mut txn = store.begin().expect("begin");
        first = txn
            .allocate(PageKind::Data, &payload(16, 0x11))
            .expect("allocate data");
        second = txn
            .allocate(PageKind::Index, &payload(32, 0x22))
            .expect("allocate index");
        third = txn
            .allocate(PageKind::Manifest, &payload(8, 0x33))
            .expect("allocate manifest");
        txn.commit().expect("commit");
    }
    assert_eq!(store.page_count(), 3);
    assert_eq!(
        read_view(&store, first),
        (PageKind::Data, payload(16, 0x11))
    );
    assert_eq!(
        read_view(&store, second),
        (PageKind::Index, payload(32, 0x22))
    );
    assert_eq!(
        read_view(&store, third),
        (PageKind::Manifest, payload(8, 0x33))
    );
}

#[test]
fn overwrite_keeps_latest_content_and_page_count() {
    let (_dir, store) = open("pstore-matrix-overwrite");
    let id;
    {
        let mut txn = store.begin().expect("begin");
        id = txn
            .allocate(PageKind::Data, &payload(16, 0x01))
            .expect("allocate");
        txn.commit().expect("commit");
    }
    {
        let page = Page::new(PageKind::Index, &payload(24, 0x02)).expect("build page");
        let mut txn = store.begin().expect("begin");
        txn.write_page(id, &page).expect("write page");
        txn.commit().expect("commit");
    }
    assert_eq!(store.page_count(), 1);
    assert_eq!(read_view(&store, id), (PageKind::Index, payload(24, 0x02)));
}

#[test]
fn abort_discards_allocations() {
    let (_dir, store) = open("pstore-matrix-abort");
    {
        let mut txn = store.begin().expect("begin");
        txn.allocate(PageKind::Data, &payload(16, 0x01))
            .expect("allocate");
        txn.abort().expect("abort");
    }
    assert_eq!(store.page_count(), 0);
    assert!(store.read_page(PageId::new(0)).expect("read").is_none());
}

#[test]
fn checkpoint_then_reopen_preserves_pages() {
    let dir = TempDir::new("pstore-matrix-ckpt");
    let data = dir.join("pstore.pages");
    let wal = dir.join("pstore.wal");
    let expected: Vec<(PageKind, Vec<u8>)> = (0..4usize)
        .map(|step| (PageKind::Data, payload(16 + step, step as u8)))
        .collect();
    {
        let store = PageStore::open(&data, &wal).expect("open");
        let mut txn = store.begin().expect("begin");
        for (step, (kind, bytes)) in expected.iter().enumerate() {
            txn.write_page(
                PageId::new(step as u64),
                &Page::new(*kind, bytes).expect("page"),
            )
            .expect("write page");
        }
        txn.commit().expect("commit");
        store.checkpoint().expect("checkpoint");
        assert_eq!(store.page_count(), expected.len() as u64);
    }

    let store = PageStore::open(&data, &wal).expect("reopen");
    assert_eq!(store.page_count(), expected.len() as u64);
    for (step, want) in expected.iter().enumerate() {
        assert_eq!(read_view(&store, PageId::new(step as u64)), *want);
    }
}

#[test]
fn reading_unwritten_page_returns_none() {
    let (_dir, store) = open("pstore-matrix-missing");
    assert_eq!(store.page_count(), 0);
    assert!(store.read_page(PageId::new(0)).expect("read").is_none());
    assert!(store.read_page(PageId::new(42)).expect("read").is_none());
}

#[test]
fn multiple_commits_accumulate_pages() {
    let (_dir, store) = open("pstore-matrix-multi");
    for step in 0..3usize {
        let mut txn = store.begin().expect("begin");
        txn.allocate(PageKind::Data, &payload(8 + step, step as u8))
            .expect("allocate");
        txn.commit().expect("commit");
        assert_eq!(store.page_count(), step as u64 + 1);
    }
    for step in 0..3u64 {
        assert_eq!(
            read_view(&store, PageId::new(step)),
            (PageKind::Data, payload(8 + step as usize, step as u8))
        );
    }
}
