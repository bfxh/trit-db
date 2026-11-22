#![forbid(unsafe_code)]
//! M0 micro-benchmarks: radix pack/unpack and 8 KiB page build/parse.
//!
//! Deterministic input, 6 rounds by default, min/median/max. This is a
//! skeleton harness, not the final `tritdb-bench`; it intentionally has no
//! external dependencies.

use std::fmt::Write as _;
use std::fs;
use std::hint::black_box;
use std::io::{Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::Instant;

use tritdb_exec::graph::{
    annotated_knn, AnnotatedGraph, Point, MAX_DIMENSIONS, MAX_EDGES, MAX_NODES, MAX_POINTS,
};
use tritdb_exec::TruthValue;
use tritdb_exec::Value;
use tritdb_format::Block;
use tritdb_page::{Page, PageKind, MAX_PAYLOAD, PAGE_SIZE};
use tritdb_radix::{pack_into, unpack_into, Radix, Trit};
use tritdb_sql::{parse_script, JoinStrategy, QueryResult, SqlEngine};
use tritdb_store::{PageId, PageStore};
use tritdb_txn::TxnStore;
use tritdb_wal::{RecordKind, WalReader, WalWriter};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str);
    let print_memory = !matches!(command, Some("record" | "check" | "help" | "--help"));
    if print_memory {
        print_rss("rss_before_kb");
    }
    match command {
        Some("radix") | None => run_radix(&args),
        Some("baseline") => run_baseline(&args),
        Some("block") => run_block(&args),
        Some("page") => run_page(&args),
        Some("wal") => run_wal(&args),
        Some("pstore") => run_pstore(&args),
        Some("pstore-checkpoint") => run_pstore_checkpoint(&args),
        Some("txn-checkpoint") => run_txn_checkpoint(&args),
        Some("txn-group") => run_txn_group(&args),
        Some("sql-join") => run_sql_join(&args),
        Some("sql-plan") => run_sql_plan(&args),
        Some("sql-exec") => run_sql_exec(&args),
        Some("sql-scan") => run_sql_scan(&args),
        Some("sql-point") => run_sql_point(&args),
        Some("sql-group") => run_sql_group(&args),
        Some("sql-sort") => run_sql_sort(&args),
        Some("sql-insert") => run_sql_insert(&args),
        Some("sql-update") => run_sql_update(&args),
        Some("sql-delete") => run_sql_delete(&args),
        Some("sql-subquery") => run_sql_subquery(&args),
        Some("sql-window") => run_sql_window(&args),
        Some("pstore-scan") => run_pstore_scan(&args),
        Some("txn-put") => run_txn_put(&args),
        Some("pgwire") => run_pgwire_roundtrip(&args),
        Some("ann-knn") => run_ann_knn(&args),
        Some("graph-reach") => run_graph_reach(&args),
        Some("record") => run_record(&args),
        Some("check") => run_check(&args),
        Some("help") | Some("--help") => print_help(),
        Some(other) => {
            eprintln!("unknown subcommand: {other}");
            print_help();
            std::process::exit(2);
        }
    }
    // `record`/`check` are orchestration commands; their child benchmarks
    // print their own RSS line.  Keep their own output clean.
    if print_memory {
        print_rss("rss_kb");
    }
}

/// Current process RSS at the end of a case.  This is a comparable footprint
/// number across OSes; `sysinfo` keeps the crate free of unsafe FFI.
fn print_rss(field: &str) {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let pid = sysinfo::Pid::from_u32(std::process::id());
    if let Some(process) = system.process(pid) {
        let rss_kb = process.memory() / 1024;
        if rss_kb > 0 {
            println!("{{\"{field}\":{rss_kb}}}");
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
    println!("  sql-join --rows 1024 --rounds 3 [--strategy auto|hash|nested]");
    println!("  sql-plan --rows 1024 --rounds 3 [--query join|filter|aggregate|all]");
    println!("  sql-exec --rows 1024 --rounds 3 [--query filter|aggregate|all]");
    println!("  --profile small|medium|large   presets for --rows (4096 / 65536 / 262144)");
    println!("  sql-scan --rows 4096 --rounds 5 [--selectivity 10]");
    println!("  sql-point --rows 4096 --lookups 256");
    println!("  sql-group --rows 4096 --groups 64 --rounds 5");
    println!("  sql-sort --rows 4096 --limit 64 --rounds 5");
    println!("  sql-insert --rows 512 --rounds 5 [--reset-each-round]");
    println!("  sql-update --rows 4096 --cutoff 512 --rounds 3");
    println!("  sql-delete --rows 4096 --cutoff 512 --rounds 3");
    println!("  sql-subquery --rows 2048 --rounds 3 [--query in|exists|scalar|all]");
    println!("  sql-window --rows 4096 --rounds 3 [--query rank|sum|all]");
    println!("  pstore-scan --pages 256 --payload 4096 --rounds 5 [--cold]");
    println!("  txn-put --txns 512 --keys-per-txn 4 --rounds 3");
    println!("  pgwire --rows 256 --queries 200 --rounds 3");
    println!("  ann-knn --points 4096 --dims 8 --k 10 --rounds 5");
    println!("  graph-reach --nodes 1024 --fanout 4 --hops 8 --rounds 5");
    println!("  record --out <json> [--markdown <md>] [--suite ci] [--rounds N]");
    println!(
        "  check --baseline <json> [--tolerance 0.5] [--min-ms 0.05] [--retries 1] [--rounds N]"
    );
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

fn run_sql_join(args: &[String]) {
    let rows = rows_or_profile(args, 1024);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if rows == 0 {
        eprintln!("--rows must be greater than 0");
        std::process::exit(2);
    }
    let strategy = match arg_value(args, "--strategy").as_deref().unwrap_or("auto") {
        "auto" => JoinStrategy::Auto,
        "hash" => JoinStrategy::Hash,
        "nested" => JoinStrategy::NestedLoop,
        other => {
            eprintln!("unknown strategy: {other}; expected auto, hash, or nested");
            std::process::exit(2);
        }
    };

    let dir = TempDir::new();
    let db = dir.0.join("sql-join.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_join_tables(&mut engine, rows);
    engine.set_join_strategy(strategy);

    let query = parse_script("SELECT COUNT(*) FROM a JOIN b ON a.id = b.id;").expect("parse query");
    let statement = &query[0];
    let mut samples = Vec::with_capacity(rounds);
    let mut matches = 0usize;
    for _ in 0..rounds {
        let start = Instant::now();
        let result = engine.execute_statement(statement).expect("execute join");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        let QueryResult::Rows { rows, .. } = &result else {
            panic!("join query did not return rows");
        };
        let Value::Int(count) = rows[0][0] else {
            panic!("COUNT did not return INT");
        };
        matches = usize::try_from(count).expect("match count");
    }

    let stats = Stats::new(samples);
    let name = match strategy {
        JoinStrategy::Auto => "auto",
        JoinStrategy::Hash => "hash",
        JoinStrategy::NestedLoop => "nested",
    };
    println!("rows\tstrategy\trounds\tjoin_ms(min/med/max)\tmatches");
    println!(
        "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
        rows, name, rounds, stats.min, stats.median, stats.max, matches
    );
    println!(
        "{{\"bench_id\":\"sql_join\",\"rows\":{},\"strategy\":\"{}\",\"rounds\":{},\"join_ms\":[{:.3},{:.3},{:.3}],\"matches\":{},\"samples\":[{}]}}",
        rows, name, rounds, stats.min, stats.median, stats.max, matches, stats.samples_json()
    );
}

fn seed_join_tables(engine: &mut SqlEngine, rows: usize) {
    engine
        .execute_script(
            "CREATE TABLE a (id INT NOT NULL, payload INT); \
             CREATE TABLE b (id INT NOT NULL, payload INT);",
        )
        .expect("create join tables");
    for table in ["a", "b"] {
        let mut offset = 0;
        while offset < rows {
            let end = (offset + SEED_BATCH).min(rows);
            let mut sql = format!("INSERT INTO {table} VALUES ");
            for row in offset..end {
                write!(&mut sql, "({row}, {row})").expect("write seed");
                if row + 1 < end {
                    sql.push(',');
                }
            }
            sql.push(';');
            engine.execute_script(&sql).expect("seed join rows");
            offset = end;
        }
    }
}

struct PlanBench {
    name: &'static str,
    text: Stats,
    sexpr: Stats,
    text_chars: usize,
    sexpr_chars: usize,
}

fn run_sql_plan(args: &[String]) {
    let rows = rows_or_profile(args, 1024);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if rows == 0 {
        eprintln!("--rows must be greater than 0");
        std::process::exit(2);
    }
    let selected = arg_value(args, "--query").unwrap_or_else(|| "all".to_string());
    let all_queries: &[(&str, &str)] = &[
        ("join", "SELECT a.id FROM a JOIN b ON a.id = b.id"),
        (
            "filter",
            "SELECT a.id FROM a JOIN b ON a.id = b.id \
             WHERE a.payload < 512 AND b.payload < 512",
        ),
        (
            "aggregate",
            "SELECT a.id, SUM(b.payload) AS total FROM a JOIN b ON a.id = b.id \
             WHERE a.payload < 512 GROUP BY a.id ORDER BY total DESC LIMIT 10",
        ),
    ];
    let queries: Vec<_> = all_queries
        .iter()
        .copied()
        .filter(|(name, _)| selected == "all" || selected == *name)
        .collect();
    if queries.is_empty() {
        eprintln!("unknown --query: {selected}; expected join, filter, aggregate, or all");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-plan.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_join_tables(&mut engine, rows);

    let mut results = Vec::with_capacity(queries.len());
    for (name, sql) in queries {
        let text_plan = parse_script(&format!("EXPLAIN {sql};")).expect("parse text explain");
        let sexpr_plan =
            parse_script(&format!("EXPLAIN SEXPR {sql};")).expect("parse sexpr explain");
        let mut text_samples = Vec::with_capacity(rounds);
        let mut sexpr_samples = Vec::with_capacity(rounds);
        let mut text_chars = 0;
        let mut sexpr_chars = 0;
        engine
            .execute_statement(&text_plan[0])
            .expect("warm text explain");
        engine
            .execute_statement(&sexpr_plan[0])
            .expect("warm sexpr explain");
        for round in 0..rounds {
            if round % 2 == 0 {
                let start = Instant::now();
                let result = engine
                    .execute_statement(&text_plan[0])
                    .expect("execute text explain");
                text_samples.push(start.elapsed().as_secs_f64() * 1000.0);
                let (_, chars) = plan_output_chars(&result);
                text_chars = chars;

                let start = Instant::now();
                let result = engine
                    .execute_statement(&sexpr_plan[0])
                    .expect("execute sexpr explain");
                sexpr_samples.push(start.elapsed().as_secs_f64() * 1000.0);
                let (_, chars) = plan_output_chars(&result);
                sexpr_chars = chars;
            } else {
                let start = Instant::now();
                let result = engine
                    .execute_statement(&sexpr_plan[0])
                    .expect("execute sexpr explain");
                sexpr_samples.push(start.elapsed().as_secs_f64() * 1000.0);
                let (_, chars) = plan_output_chars(&result);
                sexpr_chars = chars;

                let start = Instant::now();
                let result = engine
                    .execute_statement(&text_plan[0])
                    .expect("execute text explain");
                text_samples.push(start.elapsed().as_secs_f64() * 1000.0);
                let (_, chars) = plan_output_chars(&result);
                text_chars = chars;
            }
        }
        results.push(PlanBench {
            name,
            text: Stats::new(text_samples),
            sexpr: Stats::new(sexpr_samples),
            text_chars,
            sexpr_chars,
        });
    }

    println!(
        "rows\tquery\trounds\ttext_ms(min/med/max)\tsexpr_ms(min/med/max)\ttext_chars\tsexpr_chars"
    );
    for bench in &results {
        println!(
            "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.3}/{:.3}/{:.3}\t{}\t{}",
            rows,
            bench.name,
            rounds,
            bench.text.min,
            bench.text.median,
            bench.text.max,
            bench.sexpr.min,
            bench.sexpr.median,
            bench.sexpr.max,
            bench.text_chars,
            bench.sexpr_chars
        );
        println!(
            "{{\"bench_id\":\"sql_plan\",\"rows\":{},\"query\":\"{}\",\"rounds\":{},\"text_ms\":[{:.3},{:.3},{:.3}],\"sexpr_ms\":[{:.3},{:.3},{:.3}],\"text_chars\":{},\"sexpr_chars\":{}}}",
            rows,
            bench.name,
            rounds,
            bench.text.min,
            bench.text.median,
            bench.text.max,
            bench.sexpr.min,
            bench.sexpr.median,
            bench.sexpr.max,
            bench.text_chars,
            bench.sexpr_chars
        );
    }
}

fn query_result_rows(result: &QueryResult) -> usize {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("query did not return rows");
    };
    rows.len()
}

fn run_sql_exec(args: &[String]) {
    let rows = rows_or_profile(args, 1024);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if rows == 0 {
        eprintln!("--rows must be greater than 0");
        std::process::exit(2);
    }
    let selected = arg_value(args, "--query").unwrap_or_else(|| "all".to_string());
    let all_queries: &[(&str, &str)] = &[
        (
            "filter",
            "SELECT COUNT(*) FROM a JOIN b ON a.id = b.id \
             WHERE a.payload < 512 AND b.payload < 512",
        ),
        (
            "aggregate",
            "SELECT a.id, SUM(b.payload) AS total FROM a JOIN b ON a.id = b.id \
             WHERE a.payload < 512 GROUP BY a.id ORDER BY a.id",
        ),
    ];
    let queries: Vec<_> = all_queries
        .iter()
        .copied()
        .filter(|(name, _)| selected == "all" || selected == *name)
        .collect();
    if queries.is_empty() {
        eprintln!("unknown --query: {selected}; expected filter, aggregate, or all");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-exec.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_join_tables(&mut engine, rows);
    engine.set_join_strategy(JoinStrategy::Auto);

    let mut results = Vec::with_capacity(queries.len());
    for (name, sql) in queries {
        let statement = parse_script(sql).expect("parse exec query");
        engine
            .execute_statement(&statement[0])
            .expect("warm exec query");
        let mut samples = Vec::with_capacity(rounds);
        let mut result_rows = 0;
        for _ in 0..rounds {
            let start = Instant::now();
            let result = engine
                .execute_statement(&statement[0])
                .expect("execute query");
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            result_rows = query_result_rows(&result);
        }
        assert!(result_rows > 0, "{name} returned no rows");
        results.push((name, Stats::new(samples), result_rows));
    }

    println!("rows\tquery\trounds\texec_ms(min/med/max)\tresult_rows");
    for (name, stats, result_rows) in &results {
        println!(
            "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
            rows, name, rounds, stats.min, stats.median, stats.max, result_rows
        );
        println!(
            "{{\"bench_id\":\"sql_exec\",\"rows\":{},\"query\":\"{}\",\"rounds\":{},\"exec_ms\":[{:.3},{:.3},{:.3}],\"result_rows\":{}}}",
            rows, name, rounds, stats.min, stats.median, stats.max, result_rows
        );
    }
}

fn plan_output_chars(result: &QueryResult) -> (usize, usize) {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("EXPLAIN did not return rows");
    };
    let mut chars = 0;
    for row in rows {
        for value in row {
            if let Value::Text(text) = value {
                chars += text.len();
            }
        }
    }
    (rows.len(), chars)
}

/// Seeds `t (id INT NOT NULL, payload INT, grp INT)` with `rows` rows; `payload`
/// cycles through `0..10_000` and `grp` through `0..groups`.
/// Rows per `INSERT` statement while seeding. Large profiles (hundreds of
/// thousands of rows) would otherwise build one multi-megabyte statement.
const SEED_BATCH: usize = 2_048;

/// Resolves `--profile small|medium|large` into a row count.
fn profile_rows(args: &[String]) -> Option<usize> {
    match arg_value(args, "--profile").as_deref() {
        Some("small") => Some(4_096),
        Some("medium") => Some(65_536),
        Some("large") => Some(262_144),
        Some(other) => {
            eprintln!("unknown --profile {other}; expected small, medium, or large");
            std::process::exit(2);
        }
        None => None,
    }
}

/// `--rows`, falling back to the selected profile, then to `default`.
fn rows_or_profile(args: &[String], default: usize) -> usize {
    arg_usize(args, "--rows", profile_rows(args).unwrap_or(default))
}

fn insert_scan_rows(engine: &mut SqlEngine, start: usize, rows: usize, groups: usize) {
    let groups = groups.max(1);
    let mut offset = start;
    while offset < start + rows {
        let end = (offset + SEED_BATCH).min(start + rows);
        let mut sql = String::from("INSERT INTO t VALUES ");
        for row in offset..end {
            let payload = row.wrapping_mul(37) % 10_000;
            let grp = row % groups;
            write!(&mut sql, "({row}, {payload}, {grp})").expect("write seed");
            if row + 1 < end {
                sql.push(',');
            }
        }
        sql.push(';');
        engine.execute_script(&sql).expect("seed scan rows");
        offset = end;
    }
}

fn seed_scan_table(engine: &mut SqlEngine, rows: usize, groups: usize) {
    engine
        .execute_script("CREATE TABLE t (id INT NOT NULL, payload INT, grp INT);")
        .expect("create scan table");
    insert_scan_rows(engine, 0, rows, groups);
}

fn run_sql_scan(args: &[String]) {
    let rows = rows_or_profile(args, 4096);
    let rounds = arg_usize(args, "--rounds", 5).max(1);
    let selectivity = arg_usize(args, "--selectivity", 10).clamp(1, 100);
    if rows == 0 {
        eprintln!("--rows must be greater than 0");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-scan.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_scan_table(&mut engine, rows, 16);

    let cutoff = (10_000 * selectivity / 100) as i64;
    let sql = format!("SELECT COUNT(*) FROM t WHERE payload < {cutoff};");
    let statements = parse_script(&sql).expect("parse scan query");
    let statement = &statements[0];

    let expected: i64 = (0..rows)
        .map(|row| (row.wrapping_mul(37) % 10_000) as i64)
        .filter(|payload| *payload < cutoff)
        .count() as i64;
    let warm = engine
        .execute_statement(statement)
        .expect("warm scan query");
    let QueryResult::Rows {
        rows: warm_rows, ..
    } = &warm
    else {
        panic!("scan query did not return rows");
    };
    let Value::Int(count) = warm_rows[0][0] else {
        panic!("COUNT did not return INT");
    };
    assert_eq!(count, expected, "scan count mismatch");

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        let result = engine
            .execute_statement(statement)
            .expect("execute scan query");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        black_box(&result);
    }

    let stats = Stats::new(samples);
    println!("rows\tselectivity\trounds\tscan_ms(min/med/max)\tmatches");
    println!(
        "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
        rows, selectivity, rounds, stats.min, stats.median, stats.max, expected
    );
    println!(
        "{{\"bench_id\":\"sql_scan\",\"rows\":{},\"selectivity\":{},\"rounds\":{},\"scan_ms\":[{:.3},{:.3},{:.3}],\"matches\":{},\"samples\":[{}]}}",
        rows, selectivity, rounds, stats.min, stats.median, stats.max, expected, stats.samples_json()
    );
}

fn run_sql_point(args: &[String]) {
    let rows = rows_or_profile(args, 4096);
    let lookups = arg_usize(args, "--lookups", 256);
    if rows == 0 || lookups == 0 {
        eprintln!("--rows and --lookups must be greater than 0");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-point.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_scan_table(&mut engine, rows, 16);

    // Parse once per key so the measurement covers execution only.
    let mut rng = Rng::new(0x5eed_1234);
    let mut parsed = Vec::with_capacity(lookups);
    for _ in 0..lookups {
        let key = (rng.next_u64() as usize) % rows;
        let sql = format!("SELECT id, payload FROM t WHERE id = {key};");
        parsed.push((key, parse_script(&sql).expect("parse point query")));
    }
    let (_, warm) = &parsed[0];
    engine
        .execute_statement(&warm[0])
        .expect("warm point query");

    let mut samples = Vec::with_capacity(lookups);
    for (key, statements) in &parsed {
        let start = Instant::now();
        let result = engine
            .execute_statement(&statements[0])
            .expect("execute point query");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        let QueryResult::Rows {
            rows: result_rows, ..
        } = &result
        else {
            panic!("point query did not return rows");
        };
        assert_eq!(result_rows.len(), 1, "point lookup for id {key}");
    }

    let stats = Stats::new(samples);
    println!("rows\tlookups\tpoint_ms(min/med/max)");
    println!(
        "{}\t{}\t{:.3}/{:.3}/{:.3}",
        rows, lookups, stats.min, stats.median, stats.max
    );
    println!(
        "{{\"bench_id\":\"sql_point\",\"rows\":{},\"lookups\":{},\"point_ms\":[{:.3},{:.3},{:.3}],\"samples\":[{}]}}",
        rows, lookups, stats.min, stats.median, stats.max, stats.samples_json()
    );
}

fn run_sql_group(args: &[String]) {
    let rows = rows_or_profile(args, 4096);
    let groups = arg_usize(args, "--groups", 64);
    let rounds = arg_usize(args, "--rounds", 5).max(1);
    if rows == 0 || groups == 0 {
        eprintln!("--rows and --groups must be greater than 0");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-group.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_scan_table(&mut engine, rows, groups);

    let sql = "SELECT grp, COUNT(*), SUM(payload) FROM t GROUP BY grp ORDER BY grp;";
    let statements = parse_script(sql).expect("parse group query");
    let statement = &statements[0];
    let expected = groups.min(rows);
    let warm = engine
        .execute_statement(statement)
        .expect("warm group query");
    let QueryResult::Rows {
        rows: warm_rows, ..
    } = &warm
    else {
        panic!("group query did not return rows");
    };
    assert_eq!(warm_rows.len(), expected, "group count mismatch");

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        let result = engine
            .execute_statement(statement)
            .expect("execute group query");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        black_box(&result);
    }

    let stats = Stats::new(samples);
    println!("rows\tgroups\trounds\tgroup_ms(min/med/max)\tresult_rows");
    println!(
        "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
        rows, groups, rounds, stats.min, stats.median, stats.max, expected
    );
    println!(
        "{{\"bench_id\":\"sql_group\",\"rows\":{},\"groups\":{},\"rounds\":{},\"group_ms\":[{:.3},{:.3},{:.3}],\"result_rows\":{},\"samples\":[{}]}}",
        rows, groups, rounds, stats.min, stats.median, stats.max, expected, stats.samples_json()
    );
}

fn run_sql_sort(args: &[String]) {
    let rows = rows_or_profile(args, 4096);
    let limit = arg_usize(args, "--limit", 64).max(1);
    let rounds = arg_usize(args, "--rounds", 5).max(1);
    if rows == 0 {
        eprintln!("--rows must be greater than 0");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-sort.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_scan_table(&mut engine, rows, 16);

    let sql = format!("SELECT id, payload FROM t ORDER BY payload DESC, id ASC LIMIT {limit};");
    let statements = parse_script(&sql).expect("parse sort query");
    let statement = &statements[0];
    let expected = limit.min(rows);
    let warm = engine
        .execute_statement(statement)
        .expect("warm sort query");
    let QueryResult::Rows {
        rows: warm_rows, ..
    } = &warm
    else {
        panic!("sort query did not return rows");
    };
    assert_eq!(warm_rows.len(), expected, "sort row count mismatch");

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        let result = engine
            .execute_statement(statement)
            .expect("execute sort query");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        black_box(&result);
    }

    let stats = Stats::new(samples);
    println!("rows\tlimit\trounds\tsort_ms(min/med/max)\tresult_rows");
    println!(
        "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
        rows, limit, rounds, stats.min, stats.median, stats.max, expected
    );
    println!(
        "{{\"bench_id\":\"sql_sort\",\"rows\":{},\"limit\":{},\"rounds\":{},\"sort_ms\":[{:.3},{:.3},{:.3}],\"result_rows\":{},\"samples\":[{}]}}",
        rows, limit, rounds, stats.min, stats.median, stats.max, expected, stats.samples_json()
    );
}

fn create_scan_table(engine: &mut SqlEngine) {
    engine
        .execute_script("CREATE TABLE t (id INT NOT NULL, payload INT, grp INT);")
        .expect("create scan table");
}

/// Empties `t` and refills it with `rows` deterministic rows.
fn reset_scan_table(engine: &mut SqlEngine, rows: usize, groups: usize) {
    engine
        .execute_script("DELETE FROM t;")
        .expect("clear scan table");
    insert_scan_rows(engine, 0, rows, groups);
}

fn run_sql_insert(args: &[String]) {
    let batch = arg_usize(args, "--rows", 512);
    let rounds = arg_usize(args, "--rounds", 5).max(1);
    let reset_each_round = args.iter().any(|arg| arg == "--reset-each-round");
    if batch == 0 {
        eprintln!("--rows must be greater than 0");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-insert.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    create_scan_table(&mut engine);

    let mut samples = Vec::with_capacity(rounds);
    let mut inserted = 0usize;
    for round in 0..rounds {
        // By default rows accumulate across rounds, so the workload grows.
        // The opt-in reset mode opens a fresh database and times the same
        // batch against an empty table every round, making rounds comparable.
        if reset_each_round && round > 0 {
            drop(engine);
            let round_db = dir.0.join(format!("sql-insert-{round}.db"));
            engine = SqlEngine::open(&round_db).expect("open fresh SQL engine");
            create_scan_table(&mut engine);
        }
        let base = if reset_each_round { 0 } else { inserted };
        let mut sql = String::from("INSERT INTO t (id, payload, grp) VALUES ");
        for offset in 0..batch {
            let id = base + offset;
            write!(
                &mut sql,
                "({id}, {}, {})",
                id.wrapping_mul(37) % 10_000,
                id % 16
            )
            .expect("write insert");
            if offset + 1 < batch {
                sql.push(',');
            }
        }
        sql.push(';');
        let statements = parse_script(&sql).expect("parse insert batch");
        let start = Instant::now();
        engine
            .execute_statement(&statements[0])
            .expect("insert batch");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        inserted += batch;
    }

    let result = engine
        .execute_statement(&parse_script("SELECT COUNT(*) FROM t;").expect("parse count")[0])
        .expect("count rows");
    let QueryResult::Rows { rows, .. } = &result else {
        panic!("count did not return rows");
    };
    let Value::Int(count) = rows[0][0] else {
        panic!("COUNT did not return INT");
    };
    let expected_final = if reset_each_round { batch } else { inserted };
    assert_eq!(
        count as usize, expected_final,
        "inserted row count mismatch"
    );

    let stats = Stats::new(samples);
    let rows_per_sec = batch as f64 / (stats.median.max(f64::MIN_POSITIVE) / 1000.0);
    println!("batch\trounds\tinsert_ms(min/med/max)\trows_per_sec\tinserted\tfinal_rows\treset");
    println!(
        "{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.0}\t{}\t{}\t{}",
        batch,
        rounds,
        stats.min,
        stats.median,
        stats.max,
        rows_per_sec,
        inserted,
        expected_final,
        reset_each_round
    );
    println!(
        "{{\"bench_id\":\"sql_insert\",\"batch\":{},\"rounds\":{},\"insert_ms\":[{:.3},{:.3},{:.3}],\"rows_per_sec\":{:.0},\"inserted\":{},\"final_rows\":{},\"reset\":{},\"samples\":[{}]}}",
        batch,
        rounds,
        stats.min,
        stats.median,
        stats.max,
        rows_per_sec,
        inserted,
        expected_final,
        reset_each_round,
        stats.samples_json()
    );
}

fn run_sql_update(args: &[String]) {
    let rows = rows_or_profile(args, 4096);
    let cutoff = arg_usize(args, "--cutoff", 512);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if rows == 0 || cutoff == 0 {
        eprintln!("--rows and --cutoff must be greater than 0");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-update.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    create_scan_table(&mut engine);

    let sql = format!("UPDATE t SET payload = payload + 1 WHERE id < {cutoff};");
    let statements = parse_script(&sql).expect("parse update");
    let statement = &statements[0];
    let expected = cutoff.min(rows);

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        reset_scan_table(&mut engine, rows, 16);
        let start = Instant::now();
        let result = engine.execute_statement(statement).expect("execute update");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        let QueryResult::Message(message) = &result else {
            panic!("UPDATE did not return a message");
        };
        assert!(
            message.contains(&expected.to_string()),
            "unexpected UPDATE status: {message}"
        );
    }

    let stats = Stats::new(samples);
    println!("rows\tcutoff\trounds\tupdate_ms(min/med/max)\tupdated");
    println!(
        "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
        rows, cutoff, rounds, stats.min, stats.median, stats.max, expected
    );
    println!(
        "{{\"bench_id\":\"sql_update\",\"rows\":{},\"cutoff\":{},\"rounds\":{},\"update_ms\":[{:.3},{:.3},{:.3}],\"updated\":{},\"samples\":[{}]}}",
        rows, cutoff, rounds, stats.min, stats.median, stats.max, expected, stats.samples_json()
    );
}

fn run_sql_delete(args: &[String]) {
    let rows = rows_or_profile(args, 4096);
    let cutoff = arg_usize(args, "--cutoff", 512);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if rows == 0 || cutoff == 0 {
        eprintln!("--rows and --cutoff must be greater than 0");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-delete.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    create_scan_table(&mut engine);

    let sql = format!("DELETE FROM t WHERE id < {cutoff};");
    let statements = parse_script(&sql).expect("parse delete");
    let statement = &statements[0];
    let expected = cutoff.min(rows);

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        reset_scan_table(&mut engine, rows, 16);
        let start = Instant::now();
        let result = engine.execute_statement(statement).expect("execute delete");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        let QueryResult::Message(message) = &result else {
            panic!("DELETE did not return a message");
        };
        assert!(
            message.contains(&expected.to_string()),
            "unexpected DELETE status: {message}"
        );
    }

    let stats = Stats::new(samples);
    println!("rows\tcutoff\trounds\tdelete_ms(min/med/max)\tdeleted");
    println!(
        "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
        rows, cutoff, rounds, stats.min, stats.median, stats.max, expected
    );
    println!(
        "{{\"bench_id\":\"sql_delete\",\"rows\":{},\"cutoff\":{},\"rounds\":{},\"delete_ms\":[{:.3},{:.3},{:.3}],\"deleted\":{},\"samples\":[{}]}}",
        rows, cutoff, rounds, stats.min, stats.median, stats.max, expected, stats.samples_json()
    );
}

fn run_sql_subquery(args: &[String]) {
    let rows = rows_or_profile(args, 2048);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if rows == 0 {
        eprintln!("--rows must be greater than 0");
        std::process::exit(2);
    }
    let selected = arg_value(args, "--query").unwrap_or_else(|| "all".to_string());
    let all_queries: &[(&str, &str)] = &[
        (
            "in",
            "SELECT COUNT(*) FROM a WHERE id IN (SELECT id FROM b WHERE payload < 512)",
        ),
        (
            "exists",
            "SELECT COUNT(*) FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.id = a.id)",
        ),
        (
            "scalar",
            "SELECT COUNT(*) FROM a WHERE payload < (SELECT MAX(payload) FROM b)",
        ),
    ];
    let queries: Vec<_> = all_queries
        .iter()
        .copied()
        .filter(|(name, _)| selected == "all" || selected == *name)
        .collect();
    if queries.is_empty() {
        eprintln!("unknown --query: {selected}; expected in, exists, scalar, or all");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-subquery.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_join_tables(&mut engine, rows);

    let mut results = Vec::with_capacity(queries.len());
    for (name, sql) in queries {
        let statements = parse_script(sql).expect("parse subquery");
        let statement = &statements[0];
        let warm = engine.execute_statement(statement).expect("warm subquery");
        let QueryResult::Rows {
            rows: warm_rows, ..
        } = &warm
        else {
            panic!("subquery did not return rows");
        };
        let Value::Int(matches) = warm_rows[0][0] else {
            panic!("COUNT did not return INT");
        };
        let mut samples = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let start = Instant::now();
            let result = engine
                .execute_statement(statement)
                .expect("execute subquery");
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            black_box(&result);
        }
        results.push((name, Stats::new(samples), matches));
    }

    println!("rows\tquery\trounds\tsubquery_ms(min/med/max)\tmatches");
    for (name, stats, matches) in &results {
        println!(
            "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
            rows, name, rounds, stats.min, stats.median, stats.max, matches
        );
        println!(
            "{{\"bench_id\":\"sql_subquery\",\"rows\":{},\"query\":\"{}\",\"rounds\":{},\"subquery_ms\":[{:.3},{:.3},{:.3}],\"matches\":{},\"samples\":[{}]}}",
            rows, name, rounds, stats.min, stats.median, stats.max, matches, stats.samples_json()
        );
    }
}

fn run_sql_window(args: &[String]) {
    let rows = rows_or_profile(args, 4096);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if rows == 0 {
        eprintln!("--rows must be greater than 0");
        std::process::exit(2);
    }
    let selected = arg_value(args, "--query").unwrap_or_else(|| "all".to_string());
    let all_queries: &[(&str, &str)] = &[
        (
            "rank",
            "SELECT id, ROW_NUMBER() OVER (ORDER BY payload) AS rn FROM t ORDER BY id LIMIT 64",
        ),
        (
            "sum",
            "SELECT id, SUM(payload) OVER (ORDER BY id) AS running FROM t ORDER BY id LIMIT 64",
        ),
    ];
    let queries: Vec<_> = all_queries
        .iter()
        .copied()
        .filter(|(name, _)| selected == "all" || selected == *name)
        .collect();
    if queries.is_empty() {
        eprintln!("unknown --query: {selected}; expected rank, sum, or all");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("sql-window.db");
    let mut engine = SqlEngine::open(&db).expect("open SQL engine");
    seed_scan_table(&mut engine, rows, 16);

    let mut results = Vec::with_capacity(queries.len());
    for (name, sql) in queries {
        let statements = parse_script(sql).expect("parse window query");
        let statement = &statements[0];
        let warm = engine
            .execute_statement(statement)
            .expect("warm window query");
        let QueryResult::Rows {
            rows: warm_rows, ..
        } = &warm
        else {
            panic!("window query did not return rows");
        };
        let result_rows = warm_rows.len();
        let mut samples = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let start = Instant::now();
            let result = engine
                .execute_statement(statement)
                .expect("execute window query");
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
            black_box(&result);
        }
        results.push((name, Stats::new(samples), result_rows));
    }

    println!("rows\tquery\trounds\twindow_ms(min/med/max)\tresult_rows");
    for (name, stats, result_rows) in &results {
        println!(
            "{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{}",
            rows, name, rounds, stats.min, stats.median, stats.max, result_rows
        );
        println!(
            "{{\"bench_id\":\"sql_window\",\"rows\":{},\"query\":\"{}\",\"rounds\":{},\"window_ms\":[{:.3},{:.3},{:.3}],\"result_rows\":{},\"samples\":[{}]}}",
            rows, name, rounds, stats.min, stats.median, stats.max, result_rows, stats.samples_json()
        );
    }
}

fn run_pstore_scan(args: &[String]) {
    let pages = arg_usize(args, "--pages", 256);
    let payload_len = arg_usize(args, "--payload", 4096);
    let rounds = arg_usize(args, "--rounds", 5).max(1);
    let cold = args.iter().any(|arg| arg == "--cold");
    if pages == 0 {
        eprintln!("--pages must be greater than 0");
        std::process::exit(2);
    }
    if payload_len > MAX_PAYLOAD {
        eprintln!("--payload max is {MAX_PAYLOAD}");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let data = dir.data(0);
    let wal = dir.file(0);
    let payload = vec![0x5a_u8; payload_len];
    let page = Page::new(PageKind::Data, &payload).expect("build page");
    {
        let store = PageStore::open(&data, &wal).expect("open pstore");
        let mut txn = store.begin().expect("begin");
        for index in 0..pages {
            txn.write_page(PageId::new(index as u64), &page)
                .expect("write page");
        }
        txn.commit().expect("commit pages");
    }

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        // A cold round reopens the store so every read misses the page cache.
        let store = PageStore::open(&data, &wal).expect("open pstore");
        let start = Instant::now();
        let mut bytes = 0usize;
        for index in 0..pages {
            let page = store
                .read_page(PageId::new(index as u64))
                .expect("read page")
                .expect("page exists");
            bytes += page.as_bytes().len();
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        black_box(bytes);
    }

    let stats = Stats::new(samples);
    let pages_per_sec = pages as f64 / (stats.median.max(f64::MIN_POSITIVE) / 1000.0);
    println!("pages\tpayload\tmode\trounds\tscan_ms(min/med/max)\tpages_per_sec");
    println!(
        "{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.0}",
        pages,
        payload_len,
        if cold { "cold" } else { "warm" },
        rounds,
        stats.min,
        stats.median,
        stats.max,
        pages_per_sec
    );
    println!(
        "{{\"bench_id\":\"pstore_scan\",\"pages\":{},\"payload\":{},\"mode\":\"{}\",\"rounds\":{},\"scan_ms\":[{:.3},{:.3},{:.3}],\"pages_per_sec\":{:.0},\"samples\":[{}]}}",
        pages, payload_len, if cold { "cold" } else { "warm" }, rounds,
        stats.min, stats.median, stats.max, pages_per_sec, stats.samples_json()
    );
}

fn run_txn_put(args: &[String]) {
    let txns = arg_usize(args, "--txns", 512);
    let keys_per_txn = arg_usize(args, "--keys-per-txn", 4);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if txns == 0 || keys_per_txn == 0 {
        eprintln!("--txns and --keys-per-txn must be greater than 0");
        std::process::exit(2);
    }
    let keys = txns.checked_mul(keys_per_txn).unwrap_or_else(|| {
        eprintln!("--txns * --keys-per-txn overflows");
        std::process::exit(2);
    });

    let dir = TempDir::new();
    let mut samples = Vec::with_capacity(rounds);
    let mut syncs = Vec::with_capacity(rounds);
    let mut wal_bytes = 0u64;

    for round in 0..rounds {
        let wal = dir.file(round);
        let store = TxnStore::open(&wal).expect("open txn store");
        let start = Instant::now();
        for txn_index in 0..txns {
            let mut txn = store.begin().expect("begin");
            for key_index in 0..keys_per_txn {
                let key = format!("k{txn_index}-{key_index}");
                txn.put(key.as_bytes(), b"v").expect("put");
            }
            txn.commit().expect("commit");
        }
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(store.len().expect("len"), keys);
        syncs.push(store.sync_count());
        wal_bytes = fs::metadata(&wal).expect("wal metadata").len();
        drop(store);
    }

    let stats = Stats::new(samples);
    let txns_per_sec = txns as f64 / (stats.median.max(f64::MIN_POSITIVE) / 1000.0);
    let syncs_min = *syncs.iter().min().expect("syncs min");
    let syncs_max = *syncs.iter().max().expect("syncs max");
    println!("txns\tkeys_per_txn\tkeys\twal_bytes\trounds\tput_ms(min/med/max)\ttxns_per_sec\tsyncs(min/max)");
    println!(
        "{}\t{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.0}\t{}/{}",
        txns,
        keys_per_txn,
        keys,
        wal_bytes,
        rounds,
        stats.min,
        stats.median,
        stats.max,
        txns_per_sec,
        syncs_min,
        syncs_max
    );
    println!(
        "{{\"bench_id\":\"txn_put\",\"txns\":{},\"keys_per_txn\":{},\"keys\":{},\"wal_bytes\":{},\"rounds\":{},\"put_ms\":[{:.3},{:.3},{:.3}],\"txns_per_sec\":{:.0},\"syncs\":[{},{}],\"samples\":[{}]}}",
        txns, keys_per_txn, keys, wal_bytes, rounds,
        stats.min, stats.median, stats.max, txns_per_sec, syncs_min, syncs_max, stats.samples_json()
    );
}

fn pg_write_startup(stream: &mut TcpStream) {
    let mut body = Vec::new();
    body.extend_from_slice(&196_608i32.to_be_bytes());
    body.push(0);
    let mut message = Vec::new();
    message.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&body);
    stream.write_all(&message).expect("pgwire startup");
}

fn pg_write_query(stream: &mut TcpStream, sql: &str) {
    let mut payload = sql.as_bytes().to_vec();
    payload.push(0);
    let mut message = vec![b'Q'];
    message.extend_from_slice(&((payload.len() + 4) as i32).to_be_bytes());
    message.extend_from_slice(&payload);
    stream.write_all(&message).expect("pgwire query");
}

fn pg_read_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut kind = [0u8; 1];
    stream.read_exact(&mut kind).expect("pgwire message type");
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .expect("pgwire message length");
    let length = i32::from_be_bytes(length) as usize - 4;
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).expect("pgwire payload");
    (kind[0], payload)
}

/// Reads messages until `ReadyForQuery`, returning the message kinds seen.
fn pg_read_until_ready(stream: &mut TcpStream) -> Vec<u8> {
    let mut kinds = Vec::new();
    loop {
        let (kind, _) = pg_read_message(stream);
        kinds.push(kind);
        if kind == b'Z' {
            return kinds;
        }
    }
}

fn run_pgwire_roundtrip(args: &[String]) {
    let rows = rows_or_profile(args, 256);
    let queries = arg_usize(args, "--queries", 200);
    let rounds = arg_usize(args, "--rounds", 3).max(1);
    if rows == 0 || queries == 0 {
        eprintln!("--rows and --queries must be greater than 0");
        std::process::exit(2);
    }

    let dir = TempDir::new();
    let db = dir.0.join("pgwire-bench.db");
    {
        let mut engine = SqlEngine::open(&db).expect("open SQL engine");
        seed_scan_table(&mut engine, rows, 16);
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind pgwire listener");
    let addr = listener.local_addr().expect("listener addr");
    let db_path = db.clone();
    thread::spawn(move || {
        let _ = tritdb_pgwire::serve(db_path, listener);
    });

    let mut stream = TcpStream::connect(addr).expect("connect pgwire");
    stream
        .set_nodelay(true)
        .expect("disable Nagle on the bench client");
    pg_write_startup(&mut stream);
    let startup_kinds = pg_read_until_ready(&mut stream);
    assert!(
        startup_kinds.contains(&b'Z'),
        "startup did not reach ReadyForQuery"
    );

    let sql = "SELECT COUNT(*) FROM t WHERE payload < 1000;";
    pg_write_query(&mut stream, sql);
    let warm = pg_read_until_ready(&mut stream);
    assert!(warm.contains(&b'D'), "query returned no DataRow");

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        for _ in 0..queries {
            pg_write_query(&mut stream, sql);
            let kinds = pg_read_until_ready(&mut stream);
            assert!(kinds.contains(&b'D'), "query returned no DataRow");
        }
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        samples.push(elapsed_ms / queries as f64);
    }

    let _ = stream.write_all(b"X\x00\x00\x00\x04");
    let stats = Stats::new(samples);
    let qps = 1000.0 / stats.median.max(f64::MIN_POSITIVE);
    println!("rows\tqueries\trounds\troundtrip_ms(min/med/max)\tqps");
    println!(
        "{}\t{}\t{}\t{:.4}/{:.4}/{:.4}\t{:.0}",
        rows, queries, rounds, stats.min, stats.median, stats.max, qps
    );
    println!(
        "{{\"bench_id\":\"pgwire_roundtrip\",\"rows\":{},\"queries\":{},\"rounds\":{},\"roundtrip_ms\":[{:.4},{:.4},{:.4}],\"qps\":{:.0},\"samples\":[{}]}}",
        rows, queries, rounds, stats.min, stats.median, stats.max, qps, stats.samples_json()
    );
}

fn run_ann_knn(args: &[String]) {
    let count = arg_usize(args, "--points", 4096);
    let dims = arg_usize(args, "--dims", 8).clamp(1, MAX_DIMENSIONS);
    let k = arg_usize(args, "--k", 10).max(1);
    let rounds = arg_usize(args, "--rounds", 5).max(1);
    if count == 0 {
        eprintln!("--points must be greater than 0");
        std::process::exit(2);
    }
    if count > MAX_POINTS {
        eprintln!("--points must be at most {MAX_POINTS}");
        std::process::exit(2);
    }

    let mut rng = Rng::new(0xabcd_0001);
    let mut points = Vec::with_capacity(count);
    for id in 0..count {
        let mut vector = Vec::with_capacity(dims);
        for _ in 0..dims {
            vector.push((rng.next_u64() % 1000) as i64);
        }
        let label = match id % 3 {
            0 => TruthValue::True,
            1 => TruthValue::Unknown,
            _ => TruthValue::False,
        };
        points.push(Point {
            id: id as u32,
            vector,
            label,
        });
    }
    let query: Vec<i64> = (0..dims).map(|index| (index as i64 * 7) % 1000).collect();

    let warm = annotated_knn(&points, &query, k).expect("warm knn");
    assert!(
        warm.certain.len() <= k,
        "knn returned more than k neighbours"
    );

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        let answer = annotated_knn(&points, &query, k).expect("execute knn");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        black_box(&answer);
    }

    let stats = Stats::new(samples);
    let points_per_sec = count as f64 / (stats.median.max(f64::MIN_POSITIVE) / 1000.0);
    println!("points\tdims\tk\trounds\tknn_ms(min/med/max)\tpoints_per_sec\tcertain");
    println!(
        "{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:.0}\t{}",
        count,
        dims,
        k,
        rounds,
        stats.min,
        stats.median,
        stats.max,
        points_per_sec,
        warm.certain.len()
    );
    println!(
        "{{\"bench_id\":\"ann_knn\",\"points\":{},\"dims\":{},\"k\":{},\"rounds\":{},\"knn_ms\":[{:.3},{:.3},{:.3}],\"points_per_sec\":{:.0},\"certain\":{},\"samples\":[{}]}}",
        count, dims, k, rounds, stats.min, stats.median, stats.max, points_per_sec,
        warm.certain.len(), stats.samples_json()
    );
}

fn run_graph_reach(args: &[String]) {
    let nodes = arg_usize(args, "--nodes", 1024);
    let fanout = arg_usize(args, "--fanout", 4);
    let hops = arg_usize(args, "--hops", 8);
    let rounds = arg_usize(args, "--rounds", 5).max(1);
    if nodes < 2 || fanout == 0 || hops == 0 {
        eprintln!("--nodes must be at least 2; --fanout and --hops must be greater than 0");
        std::process::exit(2);
    }
    if fanout >= nodes {
        eprintln!("--fanout must be smaller than --nodes");
        std::process::exit(2);
    }
    if nodes > MAX_NODES {
        eprintln!("--nodes must be at most {MAX_NODES}");
        std::process::exit(2);
    }
    if nodes.saturating_mul(fanout) > MAX_EDGES {
        eprintln!("--nodes * --fanout must be at most {MAX_EDGES}");
        std::process::exit(2);
    }

    let mut edges = Vec::with_capacity(nodes * fanout);
    for from in 0..nodes {
        for step in 1..=fanout {
            let to = (from + step) % nodes;
            let label = match (from + step) % 3 {
                0 => TruthValue::True,
                1 => TruthValue::Unknown,
                _ => TruthValue::False,
            };
            edges.push((from as u32, to as u32, label));
        }
    }
    let graph = AnnotatedGraph::from_edges(nodes, &edges).expect("build annotated graph");
    let target = (nodes - 1) as u32;
    let warm = graph.reach(0, target, hops).expect("warm reach");
    black_box(&warm);

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        let reach = graph.reach(0, target, hops).expect("execute reach");
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
        black_box(&reach);
    }

    let stats = Stats::new(samples);
    println!("nodes\tedges\tfanout\thops\trounds\treach_ms(min/med/max)\tanswer");
    println!(
        "{}\t{}\t{}\t{}\t{}\t{:.3}/{:.3}/{:.3}\t{:?}",
        nodes,
        graph.edge_count(),
        fanout,
        hops,
        rounds,
        stats.min,
        stats.median,
        stats.max,
        warm.answer
    );
    println!(
        "{{\"bench_id\":\"graph_reach\",\"nodes\":{},\"edges\":{},\"fanout\":{},\"hops\":{},\"rounds\":{},\"reach_ms\":[{:.3},{:.3},{:.3}],\"answer\":\"{:?}\",\"samples\":[{}]}}",
        nodes, graph.edge_count(), fanout, hops, rounds,
        stats.min, stats.median, stats.max, warm.answer, stats.samples_json()
    );
}

/// One case of the baseline / regression suite.
struct CaseSpec {
    bench_id: &'static str,
    args: &'static [&'static str],
}

/// Small, seconds-level suite shared by `record` and `check`. Keep it in sync
/// with the `bench-smoke` CI job.
const CI_CASES: &[CaseSpec] = &[
    CaseSpec {
        bench_id: "sql_scan",
        args: &["sql-scan", "--rows", "512", "--rounds", "3"],
    },
    CaseSpec {
        bench_id: "sql_point",
        args: &["sql-point", "--rows", "512", "--lookups", "64"],
    },
    CaseSpec {
        bench_id: "sql_group",
        args: &[
            "sql-group",
            "--rows",
            "512",
            "--groups",
            "16",
            "--rounds",
            "3",
        ],
    },
    CaseSpec {
        bench_id: "sql_sort",
        args: &[
            "sql-sort", "--rows", "512", "--limit", "16", "--rounds", "3",
        ],
    },
    CaseSpec {
        bench_id: "sql_insert",
        args: &[
            "sql-insert",
            "--rows",
            "128",
            "--rounds",
            "10",
            "--reset-each-round",
        ],
    },
    CaseSpec {
        bench_id: "sql_update",
        args: &[
            "sql-update",
            "--rows",
            "512",
            "--cutoff",
            "64",
            "--rounds",
            "2",
        ],
    },
    CaseSpec {
        bench_id: "sql_delete",
        args: &[
            "sql-delete",
            "--rows",
            "512",
            "--cutoff",
            "64",
            "--rounds",
            "2",
        ],
    },
    CaseSpec {
        bench_id: "sql_subquery",
        args: &[
            "sql-subquery",
            "--rows",
            "256",
            "--rounds",
            "2",
            "--query",
            "in",
        ],
    },
    CaseSpec {
        bench_id: "sql_window",
        args: &[
            "sql-window",
            "--rows",
            "512",
            "--rounds",
            "2",
            "--query",
            "rank",
        ],
    },
    CaseSpec {
        bench_id: "sql_join",
        args: &[
            "sql-join",
            "--rows",
            "512",
            "--rounds",
            "2",
            "--strategy",
            "auto",
        ],
    },
    CaseSpec {
        bench_id: "pstore_scan",
        args: &[
            "pstore-scan",
            "--pages",
            "64",
            "--payload",
            "1024",
            "--rounds",
            "3",
        ],
    },
    CaseSpec {
        bench_id: "txn_put",
        args: &[
            "txn-put",
            "--txns",
            "64",
            "--keys-per-txn",
            "4",
            "--rounds",
            "1",
        ],
    },
    CaseSpec {
        bench_id: "pgwire_roundtrip",
        args: &[
            "pgwire",
            "--rows",
            "128",
            "--queries",
            "32",
            "--rounds",
            "2",
        ],
    },
    CaseSpec {
        bench_id: "ann_knn",
        args: &[
            "ann-knn", "--points", "1024", "--dims", "8", "--k", "10", "--rounds", "3",
        ],
    },
    CaseSpec {
        bench_id: "graph_reach",
        args: &[
            "graph-reach",
            "--nodes",
            "256",
            "--fanout",
            "4",
            "--hops",
            "6",
            "--rounds",
            "3",
        ],
    },
];

/// Extracts `(min, median, max)` from a benchmark JSON line such as
/// `{"bench_id":"sql_scan","scan_ms":[0.1,0.12,0.15],...}`.
fn find_ms_triplet(line: &str) -> Option<(f64, f64, f64)> {
    let marker = "_ms\":[";
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find(']')?;
    let mut parts = rest[..end].split(',');
    let min = parts.next()?.trim().parse::<f64>().ok()?;
    let median = parts.next()?.trim().parse::<f64>().ok()?;
    let max = parts.next()?.trim().parse::<f64>().ok()?;
    Some((min, median, max))
}

/// Optional `--rounds` override applied to every case that has the flag.
fn rounds_override(args: &[String]) -> Option<usize> {
    arg_value(args, "--rounds").map(|value| {
        value.parse::<usize>().unwrap_or_else(|_| {
            eprintln!("bad --rounds: {value}");
            std::process::exit(2);
        })
    })
}

fn case_args(spec: &CaseSpec, rounds: Option<usize>) -> Vec<String> {
    let mut args: Vec<String> = spec.args.iter().map(|arg| arg.to_string()).collect();
    // The default sql_insert workload appends rows across rounds, so raising
    // --rounds changes the workload shape instead of reducing noise.  A case
    // that opts into --reset-each-round is safe to scale.
    let scalable = spec.bench_id != "sql_insert" || spec.args.contains(&"--reset-each-round");
    if let Some(rounds) = rounds.filter(|_| scalable) {
        if let Some(index) = args.iter().position(|arg| arg == "--rounds") {
            if index + 1 < args.len() {
                args[index + 1] = rounds.to_string();
            } else {
                args.push(rounds.to_string());
            }
        }
    }
    args
}

/// One benchmark run's aggregated numbers plus the raw per-round samples.
struct CaseResult {
    min: f64,
    median: f64,
    max: f64,
    samples: Vec<f64>,
    rss_before_kb: Option<u64>,
    rss_kb: Option<u64>,
}

/// Extracts raw `samples` from a benchmark JSON line.  Older binaries did not
/// emit this field, so an empty vector is a valid fallback.
fn find_samples(line: &str) -> Vec<f64> {
    let marker = "\"samples\":[";
    let Some(start) = line.find(marker).map(|index| index + marker.len()) else {
        return Vec::new();
    };
    let rest = &line[start..];
    let Some(end) = rest.find(']') else {
        return Vec::new();
    };
    rest[..end]
        .split(',')
        .filter_map(|value| value.trim().parse::<f64>().ok())
        .collect()
}

fn format_samples(samples: &[f64]) -> String {
    samples
        .iter()
        .map(|sample| format!("{sample:.6}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Reads an optional `{"key":123}` line emitted around a benchmark case.
fn find_u64_field(stdout: &str, key: &str) -> Option<u64> {
    let marker = format!("\"{key}\":");
    stdout.lines().find_map(|line| {
        let start = line.find(&marker)? + marker.len();
        line[start..]
            .trim()
            .trim_end_matches('}')
            .parse::<u64>()
            .ok()
    })
}

/// Runs one benchmark case as a child process and parses its measurement.
fn run_case(exe: &Path, spec: &CaseSpec, rounds: Option<usize>) -> Result<CaseResult, String> {
    let args = case_args(spec, rounds);
    let output = std::process::Command::new(exe)
        .args(&args)
        .output()
        .map_err(|error| format!("{}: spawn failed: {error}", spec.bench_id))?;
    if !output.status.success() {
        return Err(format!("{}: exited with {}", spec.bench_id, output.status));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("\"bench_id\"") {
            if let Some((min, median, max)) = find_ms_triplet(line) {
                return Ok(CaseResult {
                    min,
                    median,
                    max,
                    samples: find_samples(line),
                    rss_before_kb: find_u64_field(&stdout, "rss_before_kb"),
                    rss_kb: find_u64_field(&stdout, "rss_kb"),
                });
            }
        }
    }
    Err(format!("{}: no *_ms triplet in output", spec.bench_id))
}

fn platform_tag() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

fn run_record(args: &[String]) {
    let Some(out) = arg_value(args, "--out") else {
        eprintln!("record requires --out <path>");
        std::process::exit(2);
    };
    let markdown = arg_value(args, "--markdown");
    let suite = arg_value(args, "--suite").unwrap_or_else(|| "ci".to_string());
    if suite != "ci" {
        eprintln!("unknown --suite {suite}; only \"ci\" is implemented");
        std::process::exit(2);
    }
    let rounds = rounds_override(args);

    let exe = std::env::current_exe().expect("current exe");
    let platform = platform_tag();
    let mut recorded = Vec::with_capacity(CI_CASES.len());
    for spec in CI_CASES {
        eprintln!("recording {}", spec.bench_id);
        let case_args = case_args(spec, rounds);
        match run_case(&exe, spec, rounds) {
            Ok(result) => recorded.push((spec, case_args, result)),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }

    let mut json = String::new();
    json.push_str("{\n  \"format\": 1,\n  \"suite\": \"ci\",\n");
    writeln!(&mut json, "  \"platform\": \"{platform}\",").expect("write baseline");
    json.push_str("  \"cases\": [\n");
    for (index, (spec, case_args, result)) in recorded.iter().enumerate() {
        let rss_before = result
            .rss_before_kb
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string());
        let rss_after = result
            .rss_kb
            .map(|value| value.to_string())
            .unwrap_or_else(|| "null".to_string());
        let rss_delta = match (result.rss_before_kb, result.rss_kb) {
            (Some(before), Some(after)) => (after as i64 - before as i64).to_string(),
            _ => "null".to_string(),
        };
        writeln!(
            &mut json,
            "    {{\"bench_id\":\"{}\",\"args\":\"{}\",\"min_ms\":{:.6},\"median_ms\":{:.6},\"max_ms\":{:.6},\"rss_before_kb\":{},\"rss_kb\":{},\"rss_delta_kb\":{},\"samples\":[{}]}}{}",
            spec.bench_id,
            case_args.join(" "),
            result.min,
            result.median,
            result.max,
            rss_before,
            rss_after,
            rss_delta,
            format_samples(&result.samples),
            if index + 1 < recorded.len() { "," } else { "" }
        )
        .expect("write baseline");
    }
    json.push_str("  ]\n}\n");
    fs::write(&out, json).expect("write baseline json");
    println!("wrote {out}");

    if let Some(markdown_path) = markdown {
        let mut md = String::new();
        md.push_str("# trit-db 基准基线\n\n");
        md.push_str("由 `tritdb-bench record` 生成；CI 用 `tritdb-bench check` 与它比较。\n\n");
        writeln!(&mut md, "- 平台：`{platform}`").expect("write markdown");
        md.push_str("- 套件：`ci`（小规模、秒级；不代表性能上限，只用于捕获数量级回归）\n");
        md.push_str("- 指标：各基准 `*_ms` 的 min / median / max\n\n");
        md.push_str(
            "| case | 参数 | min (ms) | median (ms) | max (ms) |\n|---|---|---:|---:|---:|\n",
        );
        for (spec, case_args, result) in &recorded {
            writeln!(
                &mut md,
                "| `{}` | `{}` | {:.4} | {:.4} | {:.4} |",
                spec.bench_id,
                case_args.join(" "),
                result.min,
                result.median,
                result.max
            )
            .expect("write markdown");
        }
        md.push_str("\n## 如何更新基线\n\n```bash\ncargo run -p tritdb-bench --release -- record \\\n  --out docs/results/bench-baseline.json \\\n  --markdown docs/results/BENCH.md\n```\n\n");
        md.push_str("## CI 门限\n\n```bash\ncargo run -p tritdb-bench --release -- check \\\n  --baseline docs/results/bench-baseline.json \\\n  --tolerance 2.0 --min-ms 0.05\n```\n\n共享 runner 噪声大，CI 只拦“超过基线 3 倍”的数量级回归；本地可用更紧的 `--tolerance 0.5`。\n");
        fs::write(&markdown_path, md).expect("write baseline markdown");
        println!("wrote {markdown_path}");
    }
}

/// Parses `{"bench_id":"...","median_ms":...}` lines from the baseline JSON.
fn parse_baseline(text: &str) -> Vec<(String, f64)> {
    let mut cases = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = text[cursor..].find("\"bench_id\"") {
        let start = cursor + offset;
        let end = text[start..]
            .find('}')
            .map(|index| start + index + 1)
            .unwrap_or(text.len());
        let object = &text[start..end];

        let bench_id = object
            .find(':')
            .and_then(|colon| object[colon + 1..].trim_start().strip_prefix('"'))
            .and_then(|quoted| quoted.find('"').map(|quote| &quoted[..quote]));
        let median = object
            .find("\"median_ms\"")
            .and_then(|key| {
                let after_key = key + "\"median_ms\"".len();
                object[after_key..]
                    .find(':')
                    .map(|colon| &object[after_key + colon + 1..])
            })
            .map(str::trim_start)
            .and_then(|value| {
                let end = value
                    .find(|ch: char| ch == ',' || ch == '}' || ch.is_whitespace())
                    .unwrap_or(value.len());
                value[..end].parse::<f64>().ok()
            });
        if let (Some(bench_id), Some(median)) = (bench_id, median) {
            cases.push((bench_id.to_string(), median));
        }
        cursor = end;
    }
    cases
}

fn run_check(args: &[String]) {
    let Some(baseline_path) = arg_value(args, "--baseline") else {
        eprintln!("check requires --baseline <path>");
        std::process::exit(2);
    };
    let tolerance = arg_value(args, "--tolerance")
        .map(|value| {
            value.parse::<f64>().unwrap_or_else(|_| {
                eprintln!("bad --tolerance: {value}");
                std::process::exit(2);
            })
        })
        .unwrap_or(0.5);
    let min_ms = arg_value(args, "--min-ms")
        .map(|value| {
            value.parse::<f64>().unwrap_or_else(|_| {
                eprintln!("bad --min-ms: {value}");
                std::process::exit(2);
            })
        })
        .unwrap_or(0.05);
    // Shared CI disks occasionally produce a single slow sample (fsync
    // jitter); re-run a regressed case and keep the median of attempts.  Using
    // the fastest sample would bias every gate toward "no regression".
    let retries = arg_value(args, "--retries")
        .map(|value| {
            value.parse::<usize>().unwrap_or_else(|_| {
                eprintln!("bad --retries: {value}");
                std::process::exit(2);
            })
        })
        .unwrap_or(1);
    let rounds = rounds_override(args);
    let text = fs::read_to_string(&baseline_path).expect("read baseline");
    let baseline = parse_baseline(&text);
    if baseline.is_empty() {
        eprintln!("baseline {baseline_path} contains no cases");
        std::process::exit(1);
    }

    let exe = std::env::current_exe().expect("current exe");
    println!("case\tbaseline_ms\tcurrent_ms\tratio\tstatus");
    let mut regressions = 0;
    for (bench_id, baseline_median) in &baseline {
        let Some(spec) = CI_CASES.iter().find(|spec| spec.bench_id == *bench_id) else {
            eprintln!("baseline case {bench_id} is not in the ci suite");
            std::process::exit(1);
        };
        if *baseline_median < min_ms {
            println!("{bench_id}\t{baseline_median:.4}\t-\t-\tskip (baseline below --min-ms)");
            continue;
        }
        let mut current_samples = Vec::new();
        match run_case(&exe, spec, rounds) {
            Ok(result) => current_samples.push(result.median),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        let mut attempts = 0;
        loop {
            let current_median = Stats::new(current_samples.clone()).median;
            if current_median / baseline_median.max(f64::MIN_POSITIVE) <= 1.0 + tolerance
                || attempts >= retries
            {
                break;
            }
            attempts += 1;
            eprintln!("{bench_id}: over tolerance, re-running (attempt {attempts}/{retries})");
            match run_case(&exe, spec, rounds) {
                Ok(result) => current_samples.push(result.median),
                Err(error) => {
                    eprintln!("{error}");
                    std::process::exit(1);
                }
            }
        }
        let current_median = Stats::new(current_samples).median;
        let ratio = current_median / baseline_median.max(f64::MIN_POSITIVE);
        let regressed = ratio > 1.0 + tolerance;
        if regressed {
            regressions += 1;
        }
        println!(
            "{bench_id}\t{baseline_median:.4}\t{current_median:.4}\t{ratio:.2}\t{}",
            if regressed { "REGRESSION" } else { "ok" }
        );
    }
    if regressions > 0 {
        eprintln!(
            "{regressions} case(s) exceeded the {:.0}% tolerance",
            tolerance * 100.0
        );
        std::process::exit(1);
    }
    println!("all cases within tolerance ({:.0}%)", tolerance * 100.0);
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
}

fn arg_usize(args: &[String], flag: &str, default: usize) -> usize {
    let value = arg_value(args, flag)
        .and_then(|value| value.parse().ok())
        .unwrap_or(default);
    // A bench flag like `--rows 1000000000` must fail fast instead of trying
    // to allocate the whole range.
    const MAX_BENCH_ARG: usize = 1 << 24;
    if value > MAX_BENCH_ARG {
        eprintln!("{flag} must be at most {MAX_BENCH_ARG}");
        std::process::exit(2);
    }
    value
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
    /// Sorted individual rounds.  Kept so `record` can persist the distribution
    /// instead of only min/median/max.
    samples: Vec<f64>,
}

impl Stats {
    fn new(mut samples: Vec<f64>) -> Self {
        assert!(!samples.is_empty());
        let raw_samples = samples.clone();
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
            samples: raw_samples,
        }
    }

    /// JSON fragment for the raw per-round numbers, e.g. `1.2,1.3,1.4`.
    fn samples_json(&self) -> String {
        format_samples(&self.samples)
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

#[cfg(test)]
mod tests {
    use super::{case_args, find_samples, parse_baseline, CaseSpec};

    #[test]
    fn parses_pretty_and_compact_baseline_cases() {
        let text = r#"{
          "cases": [
            {"bench_id": "sql_scan", "args": "sql-scan --rows 512", "median_ms": 0.092},
            {"bench_id":"txn_put","median_ms":195.774}
          ]
        }"#;
        let cases = parse_baseline(text);
        assert_eq!(
            cases,
            vec![
                ("sql_scan".to_string(), 0.092),
                ("txn_put".to_string(), 195.774)
            ]
        );
    }

    #[test]
    fn rounds_override_replaces_and_skips_cumulative_insert() {
        let update = CaseSpec {
            bench_id: "sql_update",
            args: &[
                "sql-update",
                "--rows",
                "512",
                "--cutoff",
                "64",
                "--rounds",
                "2",
            ],
        };
        assert_eq!(
            case_args(&update, Some(10)),
            vec![
                "sql-update",
                "--rows",
                "512",
                "--cutoff",
                "64",
                "--rounds",
                "10"
            ]
        );

        let insert = CaseSpec {
            bench_id: "sql_insert",
            args: &["sql-insert", "--rows", "128", "--rounds", "2"],
        };
        assert_eq!(
            case_args(&insert, Some(10)),
            vec!["sql-insert", "--rows", "128", "--rounds", "2"]
        );

        let reset_insert = CaseSpec {
            bench_id: "sql_insert",
            args: &[
                "sql-insert",
                "--rows",
                "128",
                "--rounds",
                "2",
                "--reset-each-round",
            ],
        };
        assert_eq!(
            case_args(&reset_insert, Some(10)),
            vec![
                "sql-insert",
                "--rows",
                "128",
                "--rounds",
                "10",
                "--reset-each-round"
            ]
        );
    }

    #[test]
    fn parses_raw_samples_from_bench_json() {
        let line = r#"{"bench_id":"sql_scan","scan_ms":[0.1,0.12,0.15],"samples":[0.100000,0.120000,0.150000]}"#;
        assert_eq!(find_samples(line), vec![0.1, 0.12, 0.15]);
        assert!(find_samples(r#"{"bench_id":"old","scan_ms":[0.1,0.12,0.15]}"#).is_empty());
    }
}
