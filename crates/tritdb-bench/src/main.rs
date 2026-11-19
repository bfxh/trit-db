#![forbid(unsafe_code)]
//! M0 micro-benchmarks: radix pack/unpack and 8 KiB page build/parse.
//!
//! Deterministic input, 6 rounds by default, min/median/max. This is a
//! skeleton harness, not the final `tritdb-bench`; it intentionally has no
//! external dependencies.

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Instant;

use tritdb_format::Block;
use tritdb_page::{Page, PageKind, MAX_PAYLOAD, PAGE_SIZE};
use tritdb_radix::{pack_into, unpack_into, Radix, Trit};
use tritdb_store::{PageId, PageStore};
use tritdb_txn::TxnStore;
use tritdb_wal::{RecordKind, WalReader, WalWriter};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("radix") | None => run_radix(&args),
        Some("baseline") => run_baseline(&args),
        Some("block") => run_block(&args),
        Some("page") => run_page(&args),
        Some("wal") => run_wal(&args),
        Some("pstore") => run_pstore(&args),
        Some("pstore-checkpoint") => run_pstore_checkpoint(&args),
        Some("txn-checkpoint") => run_txn_checkpoint(&args),
        Some("txn-group") => run_txn_group(&args),
        Some("help") | Some("--help") => print_help(),
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!("tritdb-bench");
    println!("  radix    --trits 1000000 --rounds 6 [--radix r3|r4|r16]");
    println!("  baseline --trits 1000000 --rounds 6");
    println!("  block    --trits 1000000 --rounds 6 [--radix r3|r4|r16]");
    println!("  page     --payload 4096 --rounds 6");
    println!("  wal      --records 1000 --payload 64 --rounds 6 [--fsync]");
    println!("  pstore   --pages 1 --payload 4096 --rounds 6");
    println!("  pstore-checkpoint --txns 32 --payload 4096 --rounds 6");
    println!("  txn-checkpoint --txns 64 --keys-per-txn 4 --rounds 3");
    println!("  txn-group --txns 256 --threads 16 --keys-per-txn 4 --rounds 3");
}

fn run_radix(args: &[String]) {
    let trits = arg_usize(args, "--trits", 1_000_000);
    let rounds = arg_usize(args, "--rounds", 6).max(1);
    if trits == 0 {
        eprintln!("--trits must be greater than 0");
        std::process::exit(2);
    }

    let radices: Vec<Radix> = match arg_value(args, "--radix") {
        Some(name) => match Radix::from_name(&name) {
            Some(radix) => vec![radix],
            None => {
                eprintln!("unknown radix: {name}; expected r3, r4, or r16");
                std::process::exit(2);
            }
        },
        None => Radix::ALL.to_vec(),
    };

    let mut rng = Rng::new(0x5eed_1234_abcd_ef01);
    let input: Vec<Trit> = (0..trits).map(|_| rng.trit()).collect();

    println!(
        "radix\ttrits\tbytes\tB/trit\tpack_ms(min/med/max)\tunpack_ms(min/med/max)\troundtrip"
    );

    let mut json_results = Vec::with_capacity(radices.len());
    for radix in radices {
        let expected_bytes = radix.packed_len(trits);
        let mut packed = Vec::with_capacity(expected_bytes);
        let mut decoded = Vec::with_capacity(trits);

        // Warmup and correctness check.
        pack_into(radix, &input, &mut packed).expect("pack");
        unpack_into(radix, &packed, trits, &mut decoded).expect("unpack");
        assert_eq!(decoded, input, "roundtrip failed for {radix:?}");
        assert_eq!(packed.len(), expected_bytes);

        let mut pack_ms = Vec::with_capacity(rounds);
        let mut unpack_ms = Vec::with_capacity(rounds);

        for _ in 0..rounds {
            let start = Instant::now();
            pack_into(radix, &input, &mut packed).expect("pack");
            black_box(&packed);
            pack_ms.push(start.elapsed().as_secs_f64() * 1000.0);

            let start = Instant::now();
            unpack_into(radix, &packed, trits, &mut decoded).expect("unpack");
            black_box(&decoded);
            unpack_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }

        let pack = Stats::new(pack_ms);
        let unpack = Stats::new(unpack_ms);
        let bytes_per_trit = expected_bytes as f64 / trits as f64;

        println!(
            "{}\t{}\t{}\t{:.3}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}\ttrue",
            radix.name(),
            trits,
            expected_bytes,
            bytes_per_trit,
            pack.min,
            pack.median,
            pack.max,
            unpack.min,
            unpack.median,
            unpack.max
        );

        json_results.push(format!(
            "{{\"radix\":\"{}\",\"bytes\":{},\"bytes_per_trit\":{:.6},\"pack_ms\":[{:.3},{:.3},{:.3}],\"unpack_ms\":[{:.3},{:.3},{:.3}],\"roundtrip\":true}}",
            radix.name(),
            expected_bytes,
            bytes_per_trit,
            pack.min, pack.median, pack.max,
            unpack.min, unpack.median, unpack.max
        ));
    }

    println!(
        "{{\"bench_id\":\"radix_pack\",\"trits\":{},\"rounds\":{},\"results\":[{}]}}",
        trits,
        rounds,
        json_results.join(",")
    );
}

fn run_block(args: &[String]) {
    let trits = arg_usize(args, "--trits", 1_000_000);
    let rounds = arg_usize(args, "--rounds", 6).max(1);
    if trits == 0 {
        eprintln!("--trits must be greater than 0");
        std::process::exit(2);
    }

    let radix = match arg_value(args, "--radix") {
        Some(name) => match Radix::from_name(&name) {
            Some(radix) => radix,
            None => {
                eprintln!("unknown radix: {name}; expected r3, r4, or r16");
                std::process::exit(2);
            }
        },
        None => Radix::R3,
    };

    let mut rng = Rng::new(0x5eed_1234_abcd_ef01);
    let input: Vec<Trit> = (0..trits).map(|_| rng.trit()).collect();

    let block = Block::encode(radix, &input).expect("block encode");
    let view = Block::decode(block.as_bytes()).expect("block decode");
    assert_eq!(view.trit_count() as usize, trits);
    let payload_bytes = view.payload_len();
    let block_bytes = block.len();

    let mut decoded = Vec::with_capacity(trits);
    view.trits_into(&mut decoded).expect("block trits");
    assert_eq!(decoded, input, "TRBL roundtrip failed");

    let mut encode_ms = Vec::with_capacity(rounds);
    let mut parse_ms = Vec::with_capacity(rounds);
    let mut trits_ms = Vec::with_capacity(rounds);

    for _ in 0..rounds {
        let start = Instant::now();
        let next = Block::encode(radix, &input).expect("block encode");
        black_box(next.as_bytes());
        encode_ms.push(start.elapsed().as_secs_f64() * 1000.0);

        let start = Instant::now();
        let view = Block::decode(next.as_bytes()).expect("block decode");
        black_box(view.payload());
        parse_ms.push(start.elapsed().as_secs_f64() * 1000.0);

        let start = Instant::now();
        view.trits_into(&mut decoded).expect("block trits");
        black_box(&decoded);
        trits_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let encode = Stats::new(encode_ms);
    let parse = Stats::new(parse_ms);
    let trits_stats = Stats::new(trits_ms);

    println!(
        "radix\ttrits\tblock_bytes\tpayload_bytes\tencode_ms(min/med/max)\tparse_ms(min/med/max)\ttrits_ms(min/med/max)"
    );
    println!(
        "{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}",
        radix.name(),
        trits,
        block_bytes,
        payload_bytes,
        encode.min,
        encode.median,
        encode.max,
        parse.min,
        parse.median,
        parse.max,
        trits_stats.min,
        trits_stats.median,
        trits_stats.max
    );
    println!(
        "{{\"bench_id\":\"trbl_block\",\"radix\":\"{}\",\"trits\":{},\"block_bytes\":{},\"payload_bytes\":{},\"rounds\":{},\"encode_ms\":[{:.3},{:.3},{:.3}],\"parse_ms\":[{:.3},{:.3},{:.3}],\"trits_ms\":[{:.3},{:.3},{:.3}]}}",
        radix.name(),
        trits,
        block_bytes,
        payload_bytes,
        rounds,
        encode.min,
        encode.median,
        encode.max,
        parse.min,
        parse.median,
        parse.max,
        trits_stats.min,
        trits_stats.median,
        trits_stats.max
    );
}

fn run_baseline(args: &[String]) {
    let trits = arg_usize(args, "--trits", 1_000_000);
    let rounds = arg_usize(args, "--rounds", 6).max(1);
    if trits == 0 {
        eprintln!("--trits must be greater than 0");
        std::process::exit(2);
    }

    let mut rng = Rng::new(0x5eed_1234_abcd_ef01);
    let input: Vec<Trit> = (0..trits).map(|_| rng.trit()).collect();

    println!(
        "codec\ttrits\tbytes\tB/trit\tpack_ms(min/med/max)\tunpack_ms(min/med/max)\troundtrip"
    );

    let mut json_results = Vec::with_capacity(Baseline::ALL.len());
    for codec in Baseline::ALL {
        let expected_bytes = codec.packed_len(trits);
        let mut packed = Vec::with_capacity(expected_bytes);
        let mut decoded = Vec::with_capacity(trits);

        codec.pack_into(&input, &mut packed);
        codec
            .unpack_into(&packed, trits, &mut decoded)
            .expect("baseline roundtrip");
        assert_eq!(decoded, input, "baseline roundtrip failed for {codec:?}");
        assert_eq!(packed.len(), expected_bytes);

        let mut pack_ms = Vec::with_capacity(rounds);
        let mut unpack_ms = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let start = Instant::now();
            codec.pack_into(&input, &mut packed);
            black_box(&packed);
            pack_ms.push(start.elapsed().as_secs_f64() * 1000.0);

            let start = Instant::now();
            codec
                .unpack_into(&packed, trits, &mut decoded)
                .expect("baseline unpack");
            black_box(&decoded);
            unpack_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        }

        let pack = Stats::new(pack_ms);
        let unpack = Stats::new(unpack_ms);
        let bytes_per_trit = expected_bytes as f64 / trits as f64;

        println!(
            "{}\t{}\t{}\t{:.3}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}\ttrue",
            codec.name(),
            trits,
            expected_bytes,
            bytes_per_trit,
            pack.min,
            pack.median,
            pack.max,
            unpack.min,
            unpack.median,
            unpack.max
        );

        json_results.push(format!(
            "{{\"codec\":\"{}\",\"bytes\":{},\"bytes_per_trit\":{:.6},\"pack_ms\":[{:.3},{:.3},{:.3}],\"unpack_ms\":[{:.3},{:.3},{:.3}],\"roundtrip\":true}}",
            codec.name(),
            expected_bytes,
            bytes_per_trit,
            pack.min,
            pack.median,
            pack.max,
            unpack.min,
            unpack.median,
            unpack.max
        ));
    }

    println!(
        "{{\"bench_id\":\"binary_baseline\",\"trits\":{},\"rounds\":{},\"results\":[{}]}}",
        trits,
        rounds,
        json_results.join(",")
    );
}

fn run_wal(args: &[String]) {
    let records = arg_usize(args, "--records", 1000).max(1);
    let payload_len = arg_usize(args, "--payload", 64);
    let rounds = arg_usize(args, "--rounds", 6).max(1);
    let fsync = args.iter().any(|arg| arg == "--fsync");

    let dir = TempDir::new();
    let payload = vec![0x5a_u8; payload_len];
    let mut append_ms = Vec::with_capacity(rounds);
    let mut replay_ms = Vec::with_capacity(rounds);
    let mut file_bytes = 0u64;

    for round in 0..rounds {
        let path = dir.file(round);
        let (mut writer, _) = WalWriter::open(&path).expect("create wal");

        let start = Instant::now();
        for index in 0..records {
            writer
                .append(RecordKind::Put, index as u64 + 1, &payload, fsync)
                .expect("append wal record");
        }
        append_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        file_bytes = fs::metadata(&path).expect("wal metadata").len();
        drop(writer);

        let start = Instant::now();
        let scan = WalReader::open(&path).expect("scan wal");
        assert_eq!(scan.records.len(), records);
        black_box(&scan.records);
        replay_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let append = Stats::new(append_ms);
    let replay = Stats::new(replay_ms);
    let mode = if fsync { "fsync" } else { "append" };

    println!("mode\trecords\tpayload\tfile_bytes\tappend_ms(min/med/max)\treplay_ms(min/med/max)");
    println!(
        "{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}",
        mode,
        records,
        payload_len,
        file_bytes,
        append.min,
        append.median,
        append.max,
        replay.min,
        replay.median,
        replay.max
    );
    println!(
        "{{\"bench_id\":\"wal_append\",\"fsync\":{},\"records\":{},\"payload\":{},\"file_bytes\":{},\"rounds\":{},\"append_ms\":[{:.3},{:.3},{:.3}],\"replay_ms\":[{:.3},{:.3},{:.3}]}}",
        fsync,
        records,
        payload_len,
        file_bytes,
        rounds,
        append.min,
        append.median,
        append.max,
        replay.min,
        replay.median,
        replay.max
    );
}

fn run_page(args: &[String]) {
    let payload_len = arg_usize(args, "--payload", 4096);
    let rounds = arg_usize(args, "--rounds", 6).max(1);
    if payload_len > MAX_PAYLOAD {
        eprintln!("--payload max is {MAX_PAYLOAD}");
        std::process::exit(2);
    }

    let payload = vec![0x5a_u8; payload_len];
    let mut page = Page::new(PageKind::Data, &payload).expect("build page");

    // Warmup and correctness check.
    let view = Page::parse(page.as_bytes()).expect("parse page");
    assert_eq!(view.payload(), payload.as_slice());
    assert_eq!(view.payload_len(), payload_len);

    let mut build_ms = Vec::with_capacity(rounds);
    let mut parse_ms = Vec::with_capacity(rounds);

    for _ in 0..rounds {
        let start = Instant::now();
        page = Page::new(PageKind::Data, &payload).expect("build page");
        black_box(page.as_bytes());
        build_ms.push(start.elapsed().as_secs_f64() * 1000.0);

        let start = Instant::now();
        let view = Page::parse(page.as_bytes()).expect("parse page");
        black_box(view.payload());
        parse_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let build = Stats::new(build_ms);
    let parse = Stats::new(parse_ms);

    println!("payload\tbytes\tbuild_ms(min/med/max)\tparse_ms(min/med/max)");
    println!(
        "{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}",
        payload_len,
        PAGE_SIZE,
        build.min,
        build.median,
        build.max,
        parse.min,
        parse.median,
        parse.max
    );
    println!(
        "{{\"bench_id\":\"page_io\",\"payload\":{},\"page_size\":{},\"rounds\":{},\"build_ms\":[{:.3},{:.3},{:.3}],\"parse_ms\":[{:.3},{:.3},{:.3}]}}",
        payload_len,
        PAGE_SIZE,
        rounds,
        build.min,
        build.median,
        build.max,
        parse.min,
        parse.median,
        parse.max
    );
}

fn run_pstore(args: &[String]) {
    let pages = arg_usize(args, "--pages", 1);
    let payload_len = arg_usize(args, "--payload", 4096);
    let rounds = arg_usize(args, "--rounds", 6).max(1);
    if pages == 0 {
        eprintln!("--pages must be greater than 0");
        std::process::exit(2);
    }
    if pages > 64 {
        eprintln!("--pages max is 64");
        std::process::exit(2);
    }
    if payload_len > MAX_PAYLOAD {
        eprintln!("--payload max is {MAX_PAYLOAD}");
        std::process::exit(2);
    }

    let payload = vec![0x5a_u8; payload_len];
    let prepared: Vec<Page> = (0..pages)
        .map(|_| Page::new(PageKind::Data, &payload).expect("build page"))
        .collect();
    let dir = TempDir::new();
    let mut write_ms = Vec::with_capacity(rounds);
    let mut read_ms = Vec::with_capacity(rounds);
    let mut data_bytes = 0u64;
    let mut wal_bytes = 0u64;

    for round in 0..rounds {
        let data = dir.data(round);
        let wal = dir.file(round);
        let store = PageStore::open(&data, &wal).expect("open pstore");

        let start = Instant::now();
        let mut txn = store.begin().expect("begin");
        for (index, page) in prepared.iter().enumerate() {
            txn.write_page(PageId::new(index as u64), page)
                .expect("write page");
        }
        let info = txn.commit().expect("commit pages");
        write_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(info.pages, pages);

        let start = Instant::now();
        let page = store
            .read_page(PageId::new(0))
            .expect("read page")
            .expect("page exists");
        black_box(page.as_bytes());
        read_ms.push(start.elapsed().as_secs_f64() * 1000.0);

        data_bytes = fs::metadata(&data).expect("data metadata").len();
        wal_bytes = fs::metadata(&wal).expect("wal metadata").len();
    }

    let write = Stats::new(write_ms);
    let read = Stats::new(read_ms);

    println!("pages\tpayload\tdata_bytes\twal_bytes\twrite_commit_ms(min/med/max)\tread_hit_ms(min/med/max)");
    println!(
        "{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}",
        pages,
        payload_len,
        data_bytes,
        wal_bytes,
        write.min,
        write.median,
        write.max,
        read.min,
        read.median,
        read.max
    );
    println!(
        "{{\"bench_id\":\"pstore_commit\",\"pages\":{},\"payload\":{},\"data_bytes\":{},\"wal_bytes\":{},\"rounds\":{},\"write_commit_ms\":[{:.3},{:.3},{:.3}],\"read_hit_ms\":[{:.3},{:.3},{:.3}]}}",
        pages,
        payload_len,
        data_bytes,
        wal_bytes,
        rounds,
        write.min,
        write.median,
        write.max,
        read.min,
        read.median,
        read.max
    );
}

fn run_pstore_checkpoint(args: &[String]) {
    let txns = arg_usize(args, "--txns", 32);
    let payload_len = arg_usize(args, "--payload", 4096);
    let rounds = arg_usize(args, "--rounds", 6).max(1);
    if txns == 0 {
        eprintln!("--txns must be greater than 0");
        std::process::exit(2);
    }
    if payload_len > MAX_PAYLOAD {
        eprintln!("--payload max is {MAX_PAYLOAD}");
        std::process::exit(2);
    }

    let payload = vec![0x5a_u8; payload_len];
    let page = Page::new(PageKind::Data, &payload).expect("build page");
    let dir = TempDir::new();
    let mut write_total_ms = Vec::with_capacity(rounds);
    let mut checkpoint_ms = Vec::with_capacity(rounds);
    let mut reopen_before_ms = Vec::with_capacity(rounds);
    let mut reopen_after_ms = Vec::with_capacity(rounds);
    let mut data_bytes = 0u64;
    let mut wal_before = 0u64;
    let mut wal_after = 0u64;

    for round in 0..rounds {
        let data = dir.data(round);
        let wal = dir.file(round);
        let store = PageStore::open(&data, &wal).expect("open pstore");

        let start = Instant::now();
        for index in 0..txns {
            let mut txn = store.begin().expect("begin");
            txn.write_page(PageId::new(index as u64), &page)
                .expect("write page");
            txn.commit().expect("commit page");
        }
        write_total_ms.push(start.elapsed().as_secs_f64() * 1000.0);

        data_bytes = fs::metadata(&data).expect("data metadata").len();
        wal_before = fs::metadata(&wal).expect("wal metadata").len();
        drop(store);

        let start = Instant::now();
        let reopened = PageStore::open(&data, &wal).expect("reopen before checkpoint");
        assert_eq!(reopened.page_count(), txns as u64);
        reopen_before_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        drop(reopened);

        let store = PageStore::open(&data, &wal).expect("open for checkpoint");
        let start = Instant::now();
        let info = store.checkpoint().expect("checkpoint");
        checkpoint_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(info.page_count, txns as u64);
        wal_after = info.wal_bytes_after;
        drop(store);

        let start = Instant::now();
        let reopened = PageStore::open(&data, &wal).expect("reopen after checkpoint");
        assert_eq!(reopened.page_count(), txns as u64);
        reopen_after_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let write = Stats::new(write_total_ms);
    let checkpoint = Stats::new(checkpoint_ms);
    let before = Stats::new(reopen_before_ms);
    let after = Stats::new(reopen_after_ms);

    println!("txns\tpayload\tdata_bytes\twal_before\twal_after\twrite_total_ms(min/med/max)\tcheckpoint_ms(min/med/max)\treopen_before_ms(min/med/max)\treopen_after_ms(min/med/max)");
    println!(
        "{}\t{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}",
        txns,
        payload_len,
        data_bytes,
        wal_before,
        wal_after,
        write.min,
        write.median,
        write.max,
        checkpoint.min,
        checkpoint.median,
        checkpoint.max,
        before.min,
        before.median,
        before.max,
        after.min,
        after.median,
        after.max
    );
    println!(
        "{{\"bench_id\":\"pstore_checkpoint\",\"txns\":{},\"payload\":{},\"data_bytes\":{},\"wal_before\":{},\"wal_after\":{},\"rounds\":{},\"write_total_ms\":[{:.3},{:.3},{:.3}],\"checkpoint_ms\":[{:.3},{:.3},{:.3}],\"reopen_before_ms\":[{:.3},{:.3},{:.3}],\"reopen_after_ms\":[{:.3},{:.3},{:.3}]}}",
        txns,
        payload_len,
        data_bytes,
        wal_before,
        wal_after,
        rounds,
        write.min,
        write.median,
        write.max,
        checkpoint.min,
        checkpoint.median,
        checkpoint.max,
        before.min,
        before.median,
        before.max,
        after.min,
        after.median,
        after.max
    );
}

fn run_txn_checkpoint(args: &[String]) {
    let txns = arg_usize(args, "--txns", 64);
    let keys_per_txn = arg_usize(args, "--keys-per-txn", 4);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if txns == 0 {
        eprintln!("--txns must be greater than 0");
        std::process::exit(2);
    }
    if keys_per_txn == 0 {
        eprintln!("--keys-per-txn must be greater than 0");
        std::process::exit(2);
    }
    let keys = txns.checked_mul(keys_per_txn).unwrap_or_else(|| {
        eprintln!("--txns * --keys-per-txn overflows");
        std::process::exit(2);
    });

    let dir = TempDir::new();
    let mut write_total_ms = Vec::with_capacity(rounds);
    let mut checkpoint_ms = Vec::with_capacity(rounds);
    let mut reopen_before_ms = Vec::with_capacity(rounds);
    let mut reopen_after_ms = Vec::with_capacity(rounds);
    let mut wal_before = 0u64;
    let mut wal_after = 0u64;
    let mut snapshot_bytes = 0u64;

    for round in 0..rounds {
        let wal = dir.file(round);
        let store = TxnStore::open(&wal).expect("open txn store");

        let start = Instant::now();
        for txn_index in 0..txns {
            let mut txn = store.begin().expect("begin");
            for key_index in 0..keys_per_txn {
                let key = format!("k{txn_index}-{key_index}");
                let value = format!("v{txn_index}-{key_index}");
                txn.put(key.as_bytes(), value.as_bytes()).expect("put");
            }
            txn.commit().expect("commit");
        }
        write_total_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(store.len().expect("len"), keys);
        wal_before = fs::metadata(&wal).expect("wal metadata").len();
        drop(store);

        let start = Instant::now();
        let reopened = TxnStore::open(&wal).expect("reopen before checkpoint");
        assert_eq!(reopened.len().expect("len before"), keys);
        reopen_before_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        drop(reopened);

        let store = TxnStore::open(&wal).expect("open for checkpoint");
        let start = Instant::now();
        let info = store.checkpoint().expect("checkpoint");
        checkpoint_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(info.entries, keys as u64);
        wal_after = info.wal_bytes_after;
        snapshot_bytes = info.snapshot_bytes;
        drop(store);

        let start = Instant::now();
        let reopened = TxnStore::open(&wal).expect("reopen after checkpoint");
        assert_eq!(reopened.len().expect("len after"), keys);
        reopen_after_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }

    let write = Stats::new(write_total_ms);
    let checkpoint = Stats::new(checkpoint_ms);
    let before = Stats::new(reopen_before_ms);
    let after = Stats::new(reopen_after_ms);

    println!("txns\tkeys_per_txn\tkeys\twal_before\twal_after\tsnapshot_bytes\twrite_total_ms(min/med/max)\tcheckpoint_ms(min/med/max)\treopen_before_ms(min/med/max)\treopen_after_ms(min/med/max)");
    println!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}",
        txns,
        keys_per_txn,
        keys,
        wal_before,
        wal_after,
        snapshot_bytes,
        write.min,
        write.median,
        write.max,
        checkpoint.min,
        checkpoint.median,
        checkpoint.max,
        before.min,
        before.median,
        before.max,
        after.min,
        after.median,
        after.max
    );
    println!(
        "{{\"bench_id\":\"txn_checkpoint\",\"txns\":{},\"keys_per_txn\":{},\"keys\":{},\"wal_before\":{},\"wal_after\":{},\"snapshot_bytes\":{},\"rounds\":{},\"write_total_ms\":[{:.3},{:.3},{:.3}],\"checkpoint_ms\":[{:.3},{:.3},{:.3}],\"reopen_before_ms\":[{:.3},{:.3},{:.3}],\"reopen_after_ms\":[{:.3},{:.3},{:.3}]}}",
        txns,
        keys_per_txn,
        keys,
        wal_before,
        wal_after,
        snapshot_bytes,
        rounds,
        write.min,
        write.median,
        write.max,
        checkpoint.min,
        checkpoint.median,
        checkpoint.max,
        before.min,
        before.median,
        before.max,
        after.min,
        after.median,
        after.max
    );
}

fn run_txn_group(args: &[String]) {
    let txns = arg_usize(args, "--txns", 256);
    let threads = arg_usize(args, "--threads", 16);
    let keys_per_txn = arg_usize(args, "--keys-per-txn", 4);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if txns == 0 || threads == 0 || keys_per_txn == 0 {
        eprintln!("--txns, --threads, and --keys-per-txn must be greater than 0");
        std::process::exit(2);
    }
    if txns < threads {
        eprintln!("--txns must be at least --threads");
        std::process::exit(2);
    }
    let keys = txns.checked_mul(keys_per_txn).unwrap_or_else(|| {
        eprintln!("--txns * --keys-per-txn overflows");
        std::process::exit(2);
    });

    let dir = TempDir::new();
    let mut write_total_ms = Vec::with_capacity(rounds);
    let mut syncs = Vec::with_capacity(rounds);
    let mut wal_bytes = 0u64;
    let per_thread = txns / threads;
    let remainder = txns % threads;

    for round in 0..rounds {
        let wal = dir.file(round);
        let store = Arc::new(TxnStore::open(&wal).expect("open txn store"));
        let barrier = Arc::new(Barrier::new(threads));
        let mut handles = Vec::with_capacity(threads);

        let start = Instant::now();
        for thread_index in 0..threads {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let count = per_thread + usize::from(thread_index < remainder);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for txn_index in 0..count {
                    let mut txn = store.begin().expect("begin");
                    for key_index in 0..keys_per_txn {
                        let key = format!("t{thread_index}-x{txn_index}-k{key_index}");
                        let value = format!("v{thread_index}-{txn_index}-{key_index}");
                        txn.put(key.as_bytes(), value.as_bytes()).expect("put");
                    }
                    txn.commit().expect("commit");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("txn worker");
        }
        write_total_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(store.len().expect("len"), keys);
        assert_eq!(store.active_txns(), 0);
        syncs.push(store.sync_count());
        wal_bytes = fs::metadata(&wal).expect("wal metadata").len();
        drop(store);
    }

    let write = Stats::new(write_total_ms);
    let syncs_min = *syncs.iter().min().expect("syncs min");
    let syncs_max = *syncs.iter().max().expect("syncs max");
    let syncs_median = {
        let mut sorted = syncs.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    };
    let syncs_per_txn = syncs_median as f64 / txns as f64;

    println!("txns\tthreads\tkeys_per_txn\tkeys\twal_bytes\twrite_total_ms(min/med/max)\tsyncs(min/med/max)\tsyncs_per_txn");
    println!(
        "{}\t{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}/{}/{}\t{:.4}",
        txns,
        threads,
        keys_per_txn,
        keys,
        wal_bytes,
        write.min,
        write.median,
        write.max,
        syncs_min,
        syncs_median,
        syncs_max,
        syncs_per_txn
    );
    println!(
        "{{\"bench_id\":\"txn_group\",\"txns\":{},\"threads\":{},\"keys_per_txn\":{},\"keys\":{},\"wal_bytes\":{},\"rounds\":{},\"write_total_ms\":[{:.3},{:.3},{:.3}],\"syncs\":[{},{},{}],\"syncs_per_txn\":{:.6}}}",
        txns,
        threads,
        keys_per_txn,
        keys,
        wal_bytes,
        rounds,
        write.min,
        write.median,
        write.max,
        syncs_min,
        syncs_median,
        syncs_max,
        syncs_per_txn
    );
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

fn arg_usize(args: &[String], flag: &str, default: usize) -> usize {
    arg_value(args, flag)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[derive(Clone, Copy, Debug)]
enum Baseline {
    /// One byte per trit: the naive binary baseline.
    U8,
    /// Two bits per trit: the minimal binary baseline.
    U2,
}

impl Baseline {
    const ALL: [Self; 2] = [Self::U8, Self::U2];

    fn name(self) -> &'static str {
        match self {
            Self::U8 => "bin-u8",
            Self::U2 => "bin-u2",
        }
    }

    fn packed_len(self, trits: usize) -> usize {
        match self {
            Self::U8 => trits,
            Self::U2 => trits.div_ceil(4),
        }
    }

    fn pack_into(self, trits: &[Trit], out: &mut Vec<u8>) {
        match self {
            Self::U8 => {
                out.clear();
                out.reserve(trits.len());
                out.extend(trits.iter().map(|trit| trit.get()));
            }
            Self::U2 => {
                out.clear();
                out.reserve(trits.len().div_ceil(4));
                for chunk in trits.chunks(4) {
                    let mut byte = 0u8;
                    for (index, trit) in chunk.iter().enumerate() {
                        byte |= trit.get() << (2 * index);
                    }
                    out.push(byte);
                }
            }
        }
    }

    fn unpack_into(
        self,
        bytes: &[u8],
        trit_count: usize,
        out: &mut Vec<Trit>,
    ) -> Result<(), &'static str> {
        if bytes.len() != self.packed_len(trit_count) {
            return Err("baseline length mismatch");
        }
        out.clear();
        out.reserve(trit_count);

        match self {
            Self::U8 => {
                for &byte in bytes {
                    let trit = Trit::from_u8(byte).ok_or("u8 value outside 0..=2")?;
                    out.push(trit);
                }
            }
            Self::U2 => {
                let full = trit_count / 4;
                let rem = trit_count % 4;
                for &byte in &bytes[..full] {
                    for index in 0..4 {
                        push_u2_slot(byte, index, out)?;
                    }
                }
                if rem > 0 {
                    let byte = bytes[full];
                    for index in 0..rem {
                        push_u2_slot(byte, index, out)?;
                    }
                }
            }
        }

        Ok(())
    }
}

fn push_u2_slot(byte: u8, index: usize, out: &mut Vec<Trit>) -> Result<(), &'static str> {
    let value = (byte >> (2 * index)) & 0b11;
    if value > 2 {
        return Err("u2 slot outside 0..=2");
    }
    out.push(Trit::from_u8(value).expect("checked"));
    Ok(())
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tritdb-bench-wal-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create bench temp dir");
        Self(path)
    }

    fn file(&self, round: usize) -> PathBuf {
        self.0.join(format!("wal-{round}.wal"))
    }

    fn data(&self, round: usize) -> PathBuf {
        self.0.join(format!("data-{round}.pages"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Stats {
    min: f64,
    median: f64,
    max: f64,
}

impl Stats {
    fn new(mut samples: Vec<f64>) -> Self {
        assert!(!samples.is_empty());
        samples.sort_by(|a, b| a.partial_cmp(b).expect("finite sample"));
        let len = samples.len();
        let median = if len.is_multiple_of(2) {
            (samples[len / 2 - 1] + samples[len / 2]) / 2.0
        } else {
            samples[len / 2]
        };
        Self {
            min: samples[0],
            median,
            max: samples[len - 1],
        }
    }
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn trit(&mut self) -> Trit {
        Trit::from_u8((self.next_u64() % 3) as u8).expect("0..=2")
    }
}
