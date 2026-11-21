//! Decoder fuzz: every binary decoder must return an error, never panic.
//!
//! Random bytes cover the "hostile input" path; mutated valid payloads cover
//! the "almost valid" path that usually finds off-by-one and length bugs.

use tritdb_exec::ann::B4;
use tritdb_exec::{Column, DataType, Schema, Table, Value};
use tritdb_testkit::{Rng, TempDir};

fn random_bytes(rng: &mut Rng, len: usize) -> Vec<u8> {
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

fn sample_table() -> Table {
    Table {
        name: "fuzz".to_string(),
        schema: Schema::new(vec![
            Column::new("id", DataType::Int),
            Column::new("flag", DataType::Logic),
            Column::new("p", DataType::Lukasiewicz),
            Column::new("m", DataType::Belnap),
        ]),
        rows: vec![
            vec![
                Value::Int(1),
                Value::Bool(true),
                Value::Lukasiewicz(500),
                Value::Logic(B4::Both),
            ],
            vec![Value::Int(2), Value::Null, Value::Null, Value::Null],
        ],
    }
}

#[test]
fn random_bytes_never_panic_any_decoder() {
    let mut rng = Rng::new(0xF022_0001);
    for _ in 0..2000 {
        let len = (rng.below(300) + 1) as usize;
        let bytes = random_bytes(&mut rng, len);
        let _ = tritdb_sql::decode_table("fuzz", &bytes);
        let _ = tritdb_cas::Manifest::decode(&bytes);
        let _ = tritdb_cas::Anchor::decode(&bytes);
        let _ = tritdb_format::Block::decode(&bytes);
        let count = rng.below(64) as usize;
        for radix in [
            tritdb_radix::Radix::R3,
            tritdb_radix::Radix::R4,
            tritdb_radix::Radix::R16,
        ] {
            let _ = tritdb_radix::unpack(radix, &bytes, count);
        }
    }
}

#[test]
fn mutated_valid_payloads_never_panic() {
    let bytes = tritdb_sql::encode_table(&sample_table()).expect("encode");
    let mut rng = Rng::new(0xF022_0002);
    for _ in 0..2000 {
        let mut mutated = bytes.clone();
        let flips = (rng.below(3) + 1) as usize;
        for _ in 0..flips {
            let index = rng.below(mutated.len() as u32) as usize;
            mutated[index] ^= 1 << (rng.below(8) as u8);
        }
        let _ = tritdb_sql::decode_table("fuzz", &mutated);
    }

    // Truncations of a valid payload.
    for cut in 0..bytes.len() {
        let _ = tritdb_sql::decode_table("fuzz", &bytes[..cut]);
    }
}

#[test]
fn mutated_cas_and_format_payloads_never_panic() {
    let mut rng = Rng::new(0xF022_0003);
    for _ in 0..2000 {
        let len = (rng.below(128) + 1) as usize;
        let mut bytes = random_bytes(&mut rng, len);
        if len >= 5 {
            // Make the magic match so the mutation reaches the body parsers.
            bytes[0..4].copy_from_slice(b"TRMF");
            bytes[4] = 0;
            let _ = tritdb_cas::Manifest::decode(&bytes);
            bytes[0..4].copy_from_slice(b"TRBL");
            bytes[4] = 1;
            let _ = tritdb_format::Block::decode(&bytes);
        }
    }
}

#[test]
fn page_view_parse_never_panics() {
    let mut rng = Rng::new(0xF022_0004);
    let mut block = [0u8; tritdb_page::PAGE_SIZE];
    for _ in 0..2000 {
        for byte in block.iter_mut() {
            *byte = rng.next_u64() as u8;
        }
        let _ = tritdb_page::Page::parse(&block);
        let len = rng.below(tritdb_page::PAGE_SIZE as u32 + 16) as usize;
        let _ = tritdb_page::Page::parse_slice(&block[..len.min(block.len())]);
    }
}

#[test]
fn random_txn_snapshot_never_panics() {
    let dir = TempDir::new("fuzz-txn-snapshot");
    let mut rng = Rng::new(0xF022_0005);
    for round in 0..200 {
        let wal = dir.join(&format!("fuzz-{round}.wal"));
        let snapshot = dir.join(&format!("fuzz-{round}.wal.snap"));
        let len = (rng.below(2048) + 8) as usize;
        let mut bytes = random_bytes(&mut rng, len);
        // Force the magic+version so the fuzz reaches the entry parser instead
        // of stopping at the header check.
        bytes[0..4].copy_from_slice(&tritdb_txn::SNAPSHOT_MAGIC);
        bytes[4] = tritdb_txn::SNAPSHOT_VERSION;
        bytes[5..8].fill(0);
        std::fs::write(&snapshot, &bytes).expect("write snapshot");
        let _ = tritdb_txn::TxnStore::open(&wal);
    }
}

#[test]
fn random_page_store_files_never_panic() {
    let dir = TempDir::new("fuzz-pstore");
    let mut rng = Rng::new(0xF022_0006);
    for round in 0..200 {
        let data = dir.join(&format!("data-{round}.pages"));
        let wal = dir.join(&format!("data-{round}.wal"));
        let pages = rng.below(3) as usize;
        let data_len = pages * tritdb_page::PAGE_SIZE;
        std::fs::write(&data, random_bytes(&mut rng, data_len.max(1))).expect("write data");
        let wal_len = (rng.below(2048) + 1) as usize;
        std::fs::write(&wal, random_bytes(&mut rng, wal_len)).expect("write wal");
        let _ = tritdb_store::PageStore::open(&data, &wal);
    }
}
