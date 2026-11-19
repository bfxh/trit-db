//! Cross-engine tests: the key/value and page stores consume the same
//! logical workload, plus TRBL payload composition with both engines.

use std::collections::BTreeMap;

use tritdb_format::Block;
use tritdb_page::{Page, PageKind};
use tritdb_radix::{Radix, Trit};
use tritdb_store::{PageId, PageStore};
use tritdb_testkit::{Rng, TempDir};
use tritdb_txn::TxnStore;

fn trits(values: &[u8]) -> Vec<Trit> {
    values
        .iter()
        .map(|value| Trit::from_u8(*value).expect("trit"))
        .collect()
}

#[test]
fn key_value_and_page_store_converge_on_the_same_workload() {
    let dir = TempDir::new("cross-engine");
    let txn_wal = dir.join("logical.wal");
    let data = dir.join("logical.pages");
    let page_wal = dir.join("logical-pages.wal");
    let mut txns = TxnStore::open(&txn_wal).expect("txn store");
    let mut pages = PageStore::open(&data, &page_wal).expect("page store");

    let mut mapping: BTreeMap<Vec<u8>, PageId> = BTreeMap::new();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = Rng::new(0xD00D_0000);

    for key_index in 0..16u64 {
        let key = format!("k{key_index}").into_bytes();
        let value = format!("initial-{key_index}").into_bytes();
        let mut txn = txns.begin().expect("txn begin");
        txn.put(&key, &value).expect("txn put");
        txn.commit().expect("txn commit");
        let mut page_txn = pages.begin().expect("page begin");
        let id = page_txn
            .allocate(PageKind::Data, &value)
            .expect("page allocate");
        page_txn.commit().expect("page commit");
        mapping.insert(key.clone(), id);
        model.insert(key, value);
    }

    for step in 0..80usize {
        let index = rng.below(16) as u64;
        let key = format!("k{index}").into_bytes();
        let value = format!("step-{step}-{}", rng.below(100)).into_bytes();
        let mut txn = txns.begin().expect("txn begin");
        txn.put(&key, &value).expect("txn put");
        txn.commit().expect("txn commit");
        model.insert(key.clone(), value.clone());

        let page = Page::new(PageKind::Data, &value).expect("page");
        let mut page_txn = pages.begin().expect("page begin");
        page_txn
            .write_page(*mapping.get(&key).expect("mapping"), &page)
            .expect("page write");
        page_txn.commit().expect("page commit");

        assert_eq!(
            txns.get(&key).expect("txn get"),
            Some(value.clone()),
            "txn value at step {step}"
        );
        let page = pages
            .read_page(*mapping.get(&key).expect("mapping"))
            .expect("page read")
            .expect("page exists");
        assert_eq!(
            Page::parse(page.as_bytes()).expect("parse").payload(),
            value.as_slice()
        );

        if step % 10 == 0 {
            txns.checkpoint().expect("txn checkpoint");
            drop(txns);
            txns = TxnStore::open(&txn_wal).expect("txn reopen");
            pages.checkpoint().expect("page checkpoint");
            drop(pages);
            pages = PageStore::open(&data, &page_wal).expect("page reopen");
            for (key, value) in &model {
                assert_eq!(txns.get(key).expect("txn get"), Some(value.clone()));
                let page = pages
                    .read_page(*mapping.get(key).expect("mapping"))
                    .expect("read")
                    .expect("page");
                assert_eq!(
                    Page::parse(page.as_bytes()).expect("parse").payload(),
                    value.as_slice()
                );
            }
        }
    }
}

#[test]
fn trbl_block_is_the_same_payload_in_both_engines() {
    let dir = TempDir::new("cross-engine-block");
    let txn_wal = dir.join("block.wal");
    let data = dir.join("block.pages");
    let page_wal = dir.join("block-pages.wal");
    let mut rng = Rng::new(0xB10C_1234);
    let input = trits(&(0..1024).map(|_| rng.trit()).collect::<Vec<_>>());
    let block = Block::from_trits(Radix::R3, &input).expect("block");
    let block_bytes = block.as_bytes().to_vec();

    let txns = TxnStore::open(&txn_wal).expect("txn store");
    let mut txn = txns.begin().expect("begin");
    txn.put(b"block-key", &block_bytes).expect("put block");
    txn.commit().expect("commit block");
    let embedded = txns.get(b"block-key").expect("get").expect("block value");
    assert_eq!(embedded, block_bytes);

    let pages = PageStore::open(&data, &page_wal).expect("page store");
    let mut txn = pages.begin().expect("begin");
    let id = txn
        .allocate(PageKind::Data, block.as_bytes())
        .expect("allocate block page");
    txn.commit().expect("commit page");
    let page = pages.read_page(id).expect("read").expect("page");
    let view = Page::parse(page.as_bytes()).expect("parse page");
    let decoded = Block::decode(view.payload()).expect("decode block");
    assert_eq!(decoded.trits().expect("trits"), input);

    pages.checkpoint().expect("page checkpoint");
    drop(pages);
    let reopened = PageStore::open(&data, &page_wal).expect("reopen");
    let page = reopened.read_page(id).expect("read").expect("page");
    let view = Page::parse(page.as_bytes()).expect("parse");
    assert_eq!(
        Block::decode(view.payload())
            .expect("decode")
            .trits()
            .expect("trits"),
        input
    );
}
