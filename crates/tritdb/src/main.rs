#![forbid(unsafe_code)]
//! `tritdb` M0 CLI.
//!
//! This is a diagnostics tool, not a database shell:
//!
//! ```text
//! tritdb radix --radix r3 --trits 1000000
//! tritdb page make --out page.bin --kind data --text "hello"
//! tritdb page check --in page.bin
//! ```

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::exit;
use std::thread;
use std::time::Duration;

use tritdb_exec::TruthValue;
use tritdb_format::Block;
use tritdb_page::{Page, PageKind, PAGE_SIZE};
use tritdb_radix::{pack, unpack, Radix, Trit};
use tritdb_sql::{QueryResult, SqlEngine};
use tritdb_store::{PageId, PageStore};
use tritdb_txn::TxnStore;
use tritdb_wal::{TailState, WalReader};

fn main() {
    // Windows gives the main thread a 1 MiB stack. Deeply nested SQL
    // expressions recurse through the parser before the depth limit trips, so
    // run on a worker with enough headroom.
    let worker = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(run)
        .expect("spawn tritdb worker");
    match worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            eprintln!("error: {error}");
            exit(1);
        }
        Err(_) => {
            eprintln!("error: tritdb worker panicked");
            exit(1);
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("radix") => run_radix(&args),
        Some("block") => run_block(&args),
        Some("page") => run_page(&args),
        Some("wal") => run_wal(&args),
        Some("txn") => run_txn(&args),
        Some("pstore") => run_pstore(&args),
        Some("sql") => run_sql(&args),
        Some("serve") => run_serve(&args),
        Some("scram-verifier") => run_scram_verifier(&args),
        Some("models") => run_models(&args),
        Some("repair") => run_repair(&args),
        Some("graph") => run_graph(&args),
        Some("cas") => run_cas(&args),
        Some("manifest") => run_manifest(&args),
        Some("anchor") => run_anchor(&args),
        Some("crosscheck") => run_crosscheck(),
        Some("help") | Some("--help") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!("unknown subcommand: {other}")),
    }
}

fn print_help() {
    println!("tritdb M0");
    println!("  radix --radix r3|r4|r16 --trits 1000000");
    println!("  block make --out block.trbl --radix r3 --trits 1000000");
    println!("  block check --in block.trbl");
    println!("  page make --out page.bin --kind data|index|manifest --text \"hello\"");
    println!("  page check --in page.bin");
    println!("  wal inspect --in test.wal");
    println!("  txn put --wal test.wal --key k --value v");
    println!("  txn get --wal test.wal --key k");
    println!("  txn dump --wal test.wal");
    println!("  txn checkpoint --wal test.wal [--archive] [--keep N]");
    println!("  txn history --wal test.wal [--prune --keep N]");
    println!("  txn recover --wal test.wal --to-lsn 42 | --to-time-ms 1700000000000");
    println!("  txn writer --wal test.wal --txns 1000 --keys-per-txn 4 --sleep-ms 1");
    println!("  pstore alloc --data pstore.pages --wal pstore.wal --kind data --text \"hello\"");
    println!("  pstore put --data pstore.pages --wal pstore.wal --id 0 --text \"hello\"");
    println!("  pstore get --data pstore.pages --wal pstore.wal --id 0");
    println!("  pstore dump --data pstore.pages --wal pstore.wal");
    println!("  pstore checkpoint --data pstore.pages --wal pstore.wal");
    println!("  pstore writer --data pstore.pages --wal pstore.wal --txns 1000 --sleep-ms 1");
    println!("  sql --db sql.db --sql \"SELECT * FROM t WHERE x IS NOT NULL\"");
    println!("  serve --db sql.db [--listen 127.0.0.1:5432]");
    println!(
        "        [--auth trust|password|md5|scram-sha-256 --password <pwd>|--password-file <file>]"
    );
    println!("        [--tls-cert <cert.pem> --tls-key <key.pem> [--require-tls]]");
    println!("        [--tls-client-ca <ca.pem> [--tls-client-revoked <file>] [--tls-client-ocsp <der>] [--tls-client-ocsp-url <http://...>]]");
    println!("        [--allow-plaintext-password]");
    println!("        [--scram-verifier-file <file>]");
    println!("  scram-verifier --password <pwd> [--iterations 4096] --out <file>");
    println!("  sql --db sql.db --file script.sql");
    println!("  models --vars 3 --expr \"a AND (NOT b OR c)\"");
    println!("  repair --facts 2 --denial 0,1 --query 0");
    println!("  graph build --wal graph.wal --name g [--nodes N] --edges 0-1,1-2 [--label 1-2=U]");
    println!("  graph load --wal graph.wal --name g");
    println!(
        "  graph reach [--wal graph.wal --name g | --nodes N --edges ...] --from 0 --to 2 [--max-hops 4]"
    );
    println!(
        "  graph neighbors [--wal graph.wal --name g | --nodes N --edges ...] --node 0 [--hops 1]"
    );
    println!("  graph put-points --wal graph.wal --name p --points \"1,2;3,4\" [--labels T,U]");
    println!("  graph knn [--wal graph.wal --name p | --points \"1,2;3,4\"] [--labels T,U] --query \"1,2\" [--k 3]");
    println!("  cas put --store dir --file blob");
    println!("  cas get --store dir --hash <hex> --out blob");
    println!("  manifest commit --store dir [--parent <hex>] --entry name=<hex>");
    println!("  cas gc --store dir --root <hex>");
    println!("  manifest verify --store dir --head <hex>");
    println!("  manifest snapshot --store dir --db sql.db [--parent <hex>]");
    println!("  manifest restore --store dir --head <hex> --db restored.db");
    println!("  manifest snapshot-pages --store dir --data file.pages [--parent <hex>]");
    println!("  manifest restore-pages --store dir --head <hex> --out file.pages");
    println!("  manifest replicate --from src --to dst --head <hex> [--entry name]");
    println!("  manifest show --store dir --hash <hex>");
    println!("  anchor write --out anchor.bin --head <hex> [--key-file secret]");
    println!("  anchor verify --file anchor.bin [--store dir] [--head <hex>] [--key-file secret]");
    println!("  crosscheck  (binary framed stdin/stdout transport for Python)");
}

fn run_radix(args: &[String]) -> Result<(), String> {
    let trits_count = arg_usize(args, "--trits", 1_000_000);
    if trits_count == 0 {
        return Err("--trits must be greater than 0".to_string());
    }

    let radices: Vec<Radix> = match arg_value(args, "--radix") {
        Some(name) => {
            vec![Radix::from_name(&name).ok_or_else(|| format!("unknown radix: {name}"))?]
        }
        None => Radix::ALL.to_vec(),
    };

    let mut rng = Rng::new(0x5eed_1234_abcd_ef01);
    let input: Vec<Trit> = (0..trits_count).map(|_| rng.trit()).collect();

    for radix in radices {
        let packed = pack(radix, &input).map_err(|error| error.to_string())?;
        let decoded = unpack(radix, &packed, input.len()).map_err(|error| error.to_string())?;
        if decoded != input {
            return Err(format!("roundtrip failed for {}", radix.name()));
        }
        println!(
            "radix={} trits={} bytes={} bits_per_trit={:.3} roundtrip=ok",
            radix.name(),
            trits_count,
            packed.len(),
            radix.bits_per_trit()
        );
    }

    Ok(())
}

fn run_block(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("make") => block_make(args),
        Some("check") => block_check(args),
        Some(other) => Err(format!("unknown block subcommand: {other}")),
        None => Err("block requires `make` or `check`".to_string()),
    }
}

fn block_make(args: &[String]) -> Result<(), String> {
    let out = required_arg(args, "--out")?;
    let radix = match arg_value(args, "--radix") {
        Some(name) => Radix::from_name(&name).ok_or_else(|| format!("unknown radix: {name}"))?,
        None => Radix::R3,
    };
    let trits_count = arg_usize(args, "--trits", 1000);
    if trits_count == 0 {
        return Err("--trits must be greater than 0".to_string());
    }

    let mut rng = Rng::new(0x5eed_1234_abcd_ef01);
    let input: Vec<Trit> = (0..trits_count).map(|_| rng.trit()).collect();
    let block = Block::encode(radix, &input).map_err(|error| error.to_string())?;
    std::fs::write(&out, block.as_bytes()).map_err(|error| format!("write {out}: {error}"))?;
    println!(
        "wrote {} ({} bytes, radix={}, trits={})",
        out,
        block.len(),
        radix.name(),
        trits_count
    );
    Ok(())
}

fn block_check(args: &[String]) -> Result<(), String> {
    let input = required_arg(args, "--in")?;
    let bytes = std::fs::read(&input).map_err(|error| format!("read {input}: {error}"))?;
    let view = Block::decode(&bytes).map_err(|error| error.to_string())?;

    println!(
        "radix={} trits={} payload_len={} block_len={} crc=ok",
        view.radix().name(),
        view.trit_count(),
        view.payload_len(),
        view.as_bytes().len()
    );

    let payload = view.payload();
    let preview_len = payload.len().min(32);
    let hex: Vec<String> = payload[..preview_len]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    println!("payload_hex={}", hex.join(""));

    let trits = view.trits().map_err(|error| error.to_string())?;
    println!("trits_roundtrip=ok ({} trits)", trits.len());
    Ok(())
}

fn run_page(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("make") => page_make(args),
        Some("check") => page_check(args),
        Some(other) => Err(format!("unknown page subcommand: {other}")),
        None => Err("page requires `make` or `check`".to_string()),
    }
}

fn page_kind(args: &[String]) -> Result<PageKind, String> {
    match arg_value(args, "--kind").as_deref() {
        None | Some("data") => Ok(PageKind::Data),
        Some("index") => Ok(PageKind::Index),
        Some("manifest") => Ok(PageKind::Manifest),
        Some(other) => Err(format!("unknown kind: {other}")),
    }
}

fn page_make(args: &[String]) -> Result<(), String> {
    let out = required_arg(args, "--out")?;
    let text = arg_value(args, "--text").unwrap_or_default();
    let kind = page_kind(args)?;

    let page = Page::new(kind, text.as_bytes()).map_err(|error| error.to_string())?;
    std::fs::write(&out, &page.as_bytes()[..]).map_err(|error| format!("write {out}: {error}"))?;
    println!(
        "wrote {} ({} bytes, payload={} bytes, kind={})",
        out,
        PAGE_SIZE,
        text.len(),
        kind.name()
    );
    Ok(())
}

fn page_check(args: &[String]) -> Result<(), String> {
    let input = required_arg(args, "--in")?;
    let bytes = std::fs::read(&input).map_err(|error| format!("read {input}: {error}"))?;
    let view = Page::parse_slice(&bytes).map_err(|error| error.to_string())?;

    println!(
        "kind={} payload_len={} checksum=ok",
        view.kind().name(),
        view.payload_len()
    );

    let payload = view.payload();
    match std::str::from_utf8(payload) {
        Ok(text) if !text.is_empty() => println!("payload_utf8={text:?}"),
        _ => {
            let preview_len = payload.len().min(32);
            let hex: Vec<String> = payload[..preview_len]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            println!("payload_hex={}", hex.join(""));
        }
    }

    Ok(())
}

fn run_wal(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("inspect") => wal_inspect(args),
        Some(other) => Err(format!("unknown wal subcommand: {other}")),
        None => Err("wal requires `inspect`".to_string()),
    }
}

fn wal_inspect(args: &[String]) -> Result<(), String> {
    let input = required_arg(args, "--in")?;
    let limit = arg_usize(args, "--limit", 32).max(1);
    let scan =
        WalReader::open(Path::new(&input)).map_err(|error| format!("read wal {input}: {error}"))?;

    println!(
        "first_lsn={} records={} valid_end={} file_len={}",
        scan.first_lsn,
        scan.records.len(),
        scan.valid_end,
        scan.file_len
    );
    match scan.tail {
        TailState::Clean => println!("tail=clean"),
        TailState::Torn { offset, bytes } => {
            println!("tail=torn offset={offset} bytes={bytes}")
        }
    }

    for record in scan.records.iter().take(limit) {
        println!(
            "lsn={} txn={} kind={} payload_len={}",
            record.lsn,
            record.txn_id,
            record.kind.name(),
            record.payload.len()
        );
    }
    if scan.records.len() > limit {
        println!("... {} more records", scan.records.len() - limit);
    }
    Ok(())
}

fn run_txn(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("put") => txn_put(args),
        Some("get") => txn_get(args),
        Some("dump") => txn_dump(args),
        Some("checkpoint") => txn_checkpoint(args),
        Some("history") => txn_history(args),
        Some("recover") => txn_recover(args),
        Some("writer") => txn_writer(args),
        Some(other) => Err(format!("unknown txn subcommand: {other}")),
        None => Err(
            "txn requires `put`, `get`, `dump`, `checkpoint`, `history`, `recover`, or `writer`"
                .to_string(),
        ),
    }
}

fn open_store(path: &str) -> Result<TxnStore, String> {
    TxnStore::open(Path::new(path)).map_err(|error| format!("open {path}: {error}"))
}

fn txn_put(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let key = required_arg(args, "--key")?;
    let value = arg_value(args, "--value").unwrap_or_default();
    let store = open_store(&wal)?;

    let mut txn = store.begin().map_err(|error| error.to_string())?;
    txn.put(key.as_bytes(), value.as_bytes())
        .map_err(|error| error.to_string())?;
    let info = txn.commit().map_err(|error| error.to_string())?;

    println!(
        "committed txn={} lsn={} ops={}",
        info.txn_id, info.commit_lsn, info.op_count
    );
    Ok(())
}

fn txn_get(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let key = required_arg(args, "--key")?;
    let store = open_store(&wal)?;

    match store
        .get(key.as_bytes())
        .map_err(|error| error.to_string())?
    {
        Some(value) => println!(
            "key={:?} value={} bytes={}",
            key,
            preview_bytes(&value),
            value.len()
        ),
        None => println!("key={key:?} not_found"),
    }
    Ok(())
}

fn txn_dump(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let limit = arg_usize(args, "--limit", 32).max(1);
    let store = open_store(&wal)?;
    let recovery = store.recovery();

    println!(
        "records={} records_skipped={} checkpoint_lsn={} committed_txns={} aborted_txns={} discarded_txns={} torn_tail_bytes={} last_lsn={}",
        recovery.records,
        recovery.records_skipped,
        recovery.checkpoint_lsn,
        recovery.committed_txns,
        recovery.aborted_txns,
        recovery.discarded_txns,
        recovery.torn_tail_bytes,
        recovery.last_lsn
    );

    let entries = store.iter().map_err(|error| error.to_string())?;
    println!("keys={}", entries.len());
    for (key, value) in entries.iter().take(limit) {
        println!("key={} value={}", preview_bytes(key), preview_bytes(value));
    }
    if entries.len() > limit {
        println!("... {} more keys", entries.len() - limit);
    }
    Ok(())
}

fn txn_checkpoint(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let archive = args.iter().any(|arg| arg == "--archive");
    let keep = arg_usize(args, "--keep", tritdb_txn::DEFAULT_ARCHIVE_KEEP);
    let store = open_store(&wal)?;
    let info = if archive {
        store.checkpoint_archive_keep(keep)
    } else {
        store.checkpoint()
    }
    .map_err(|error| error.to_string())?;
    println!(
        "checkpoint_lsn={} entries={} wal_bytes_before={} wal_bytes_after={} snapshot={} snapshot_bytes={} archived_wal={} archived_snapshot={} pruned_archives={}",
        info.checkpoint_lsn,
        info.entries,
        info.wal_bytes_before,
        info.wal_bytes_after,
        store.snapshot_path().display(),
        info.snapshot_bytes,
        info.archived_wal_bytes.map_or("-".to_string(), |n| n.to_string()),
        info.archived_snapshot_bytes
            .map_or("-".to_string(), |n| n.to_string()),
        info.pruned_archives
    );
    Ok(())
}

fn txn_history(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let mut dir = std::ffi::OsString::from(&wal);
    dir.push(".history");
    let dir = std::path::PathBuf::from(dir);

    if args.iter().any(|arg| arg == "--prune") {
        let keep = arg_usize(args, "--keep", 1);
        let info = TxnStore::prune_history(Path::new(&wal), keep)
            .map_err(|error| format!("prune {wal}: {error}"))?;
        println!(
            "pruned={} kept={}",
            join_lsns(&info.removed),
            join_lsns(&info.kept)
        );
        return Ok(());
    }

    if !dir.is_dir() {
        println!("history={} entries=0", dir.display());
        return Ok(());
    }
    let mut items: Vec<_> = std::fs::read_dir(&dir)
        .map_err(|error| format!("read {}: {error}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("wal"))
        .collect();
    items.sort();
    println!("history={} entries={}", dir.display(), items.len());
    for path in items {
        let lsn = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("?");
        let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let has_snapshot = path.with_extension("snap").is_file();
        println!("lsn={lsn} wal_bytes={bytes} snapshot={has_snapshot}");
    }
    Ok(())
}

fn join_lsns(lsns: &[u64]) -> String {
    if lsns.is_empty() {
        return "-".to_string();
    }
    lsns.iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn txn_recover(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let limit = arg_usize(args, "--limit", 32).max(1);
    let view = if let Some(ts) = arg_value(args, "--to-time-ms") {
        let ts: u64 = ts
            .parse()
            .map_err(|_| format!("invalid --to-time-ms: {ts}"))?;
        TxnStore::recover_at_time(Path::new(&wal), ts)
            .map_err(|error| format!("recover {wal} to time {ts}ms: {error}"))?
    } else {
        let target_lsn = arg_u64(args, "--to-lsn")?;
        TxnStore::recover_at(Path::new(&wal), target_lsn)
            .map_err(|error| format!("recover {wal} to lsn {target_lsn}: {error}"))?
    };
    println!(
        "target_lsn={} checkpoint_lsn={} applied_lsn={} committed_txns={} discarded_txns={} keys={}",
        view.target_lsn,
        view.checkpoint_lsn,
        view.applied_lsn,
        view.committed_txns,
        view.discarded_txns,
        view.entries.len()
    );
    for (key, value) in view.entries.iter().take(limit) {
        println!("key={} value={}", preview_bytes(key), preview_bytes(value));
    }
    if view.entries.len() > limit {
        println!("... {} more keys", view.entries.len() - limit);
    }
    Ok(())
}

fn txn_writer(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let txns = arg_usize(args, "--txns", 1000);
    let keys_per_txn = arg_usize(args, "--keys-per-txn", 4);
    let sleep_ms = arg_usize(args, "--sleep-ms", 0);
    if txns == 0 {
        return Err("--txns must be greater than 0".to_string());
    }
    if keys_per_txn == 0 {
        return Err("--keys-per-txn must be greater than 0".to_string());
    }

    let store = open_store(&wal)?;
    for txn_index in 0..txns {
        let mut txn = store.begin().map_err(|error| error.to_string())?;
        for key_index in 0..keys_per_txn {
            let key = format!("k{txn_index}-{key_index}");
            let value = format!("v{txn_index}-{key_index}");
            txn.put(key.as_bytes(), value.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        let info = txn.commit().map_err(|error| error.to_string())?;
        println!(
            "committed={} txn={} lsn={} ops={}",
            txn_index + 1,
            info.txn_id,
            info.commit_lsn,
            info.op_count
        );
        std::io::stdout()
            .flush()
            .map_err(|error| format!("flush stdout: {error}"))?;
        if sleep_ms > 0 {
            thread::sleep(Duration::from_millis(sleep_ms as u64));
        }
    }
    Ok(())
}

fn run_pstore(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("put") => pstore_put(args),
        Some("alloc") => pstore_alloc(args),
        Some("get") => pstore_get(args),
        Some("dump") => pstore_dump(args),
        Some("checkpoint") => pstore_checkpoint(args),
        Some("writer") => pstore_writer(args),
        Some(other) => Err(format!("unknown pstore subcommand: {other}")),
        None => Err(
            "pstore requires `put`, `alloc`, `get`, `dump`, `checkpoint`, or `writer`".to_string(),
        ),
    }
}

fn open_pstore(data: &str, wal: &str) -> Result<PageStore, String> {
    PageStore::open(Path::new(data), Path::new(wal))
        .map_err(|error| format!("open pstore data={data} wal={wal}: {error}"))
}

fn pstore_put(args: &[String]) -> Result<(), String> {
    let data = required_arg(args, "--data")?;
    let wal = required_arg(args, "--wal")?;
    let id = arg_u64(args, "--id")?;
    let text = arg_value(args, "--text").unwrap_or_default();
    let kind = page_kind(args)?;
    let store = open_pstore(&data, &wal)?;
    if id >= store.page_count() {
        return Err(format!(
            "page id {id} does not exist (page_count={}); use `pstore alloc`",
            store.page_count()
        ));
    }

    let page = Page::new(kind, text.as_bytes()).map_err(|error| error.to_string())?;
    let mut txn = store.begin().map_err(|error| error.to_string())?;
    txn.write_page(PageId::new(id), &page)
        .map_err(|error| error.to_string())?;
    let info = txn.commit().map_err(|error| error.to_string())?;
    println!(
        "committed txn={} lsn={} pages={} page={id}",
        info.txn_id, info.commit_lsn, info.pages
    );
    Ok(())
}

fn pstore_alloc(args: &[String]) -> Result<(), String> {
    let data = required_arg(args, "--data")?;
    let wal = required_arg(args, "--wal")?;
    let text = arg_value(args, "--text").unwrap_or_default();
    let kind = page_kind(args)?;
    let store = open_pstore(&data, &wal)?;

    let mut txn = store.begin().map_err(|error| error.to_string())?;
    let page = txn
        .allocate(kind, text.as_bytes())
        .map_err(|error| error.to_string())?;
    let info = txn.commit().map_err(|error| error.to_string())?;
    println!(
        "committed txn={} lsn={} pages={} page={}",
        info.txn_id,
        info.commit_lsn,
        info.pages,
        page.get()
    );
    Ok(())
}

fn pstore_get(args: &[String]) -> Result<(), String> {
    let data = required_arg(args, "--data")?;
    let wal = required_arg(args, "--wal")?;
    let id = arg_u64(args, "--id")?;
    let store = open_pstore(&data, &wal)?;

    match store
        .read_page(PageId::new(id))
        .map_err(|error| error.to_string())?
    {
        Some(page) => {
            let view = Page::parse(page.as_bytes()).map_err(|error| error.to_string())?;
            println!(
                "page={id} kind={} payload_len={} payload={}",
                view.kind().name(),
                view.payload_len(),
                preview_bytes(view.payload())
            );
        }
        None => println!("page={id} not_found"),
    }
    Ok(())
}

fn pstore_checkpoint(args: &[String]) -> Result<(), String> {
    let data = required_arg(args, "--data")?;
    let wal = required_arg(args, "--wal")?;
    let store = open_pstore(&data, &wal)?;
    let info = store.checkpoint().map_err(|error| error.to_string())?;
    println!(
        "checkpoint_lsn={} page_count={} wal_bytes_before={} wal_bytes_after={} checkpoint={}",
        info.checkpoint_lsn,
        info.page_count,
        info.wal_bytes_before,
        info.wal_bytes_after,
        store.checkpoint_path().display()
    );
    Ok(())
}

fn pstore_dump(args: &[String]) -> Result<(), String> {
    let data = required_arg(args, "--data")?;
    let wal = required_arg(args, "--wal")?;
    let limit = arg_usize(args, "--limit", 32).max(1);
    let store = open_pstore(&data, &wal)?;
    let recovery = store.recovery();

    println!(
        "records={} records_skipped={} checkpoint_lsn={} committed_txns={} aborted_txns={} discarded_txns={} pages_replayed={} torn_tail_bytes={} data_tail_bytes={} data_pages={} last_lsn={}",
        recovery.records,
        recovery.records_skipped,
        recovery.checkpoint_lsn,
        recovery.committed_txns,
        recovery.aborted_txns,
        recovery.discarded_txns,
        recovery.pages_replayed,
        recovery.torn_tail_bytes,
        recovery.data_tail_bytes,
        recovery.data_pages,
        recovery.last_lsn
    );
    println!(
        "page_count={} cached={}",
        store.page_count(),
        store.cache_len().map_err(|error| error.to_string())?
    );

    let count = store.page_count().min(limit as u64);
    for id in 0..count {
        let page = store
            .read_page(PageId::new(id))
            .map_err(|error| error.to_string())?;
        let Some(page) = page else {
            continue;
        };
        let view = Page::parse(page.as_bytes()).map_err(|error| error.to_string())?;
        println!(
            "page={id} kind={} payload_len={} payload={}",
            view.kind().name(),
            view.payload_len(),
            preview_bytes(view.payload())
        );
    }
    if store.page_count() > count {
        println!("... {} more pages", store.page_count() - count);
    }
    Ok(())
}

fn pstore_writer(args: &[String]) -> Result<(), String> {
    let data = required_arg(args, "--data")?;
    let wal = required_arg(args, "--wal")?;
    let txns = arg_usize(args, "--txns", 1000);
    let sleep_ms = arg_usize(args, "--sleep-ms", 0);
    if txns == 0 {
        return Err("--txns must be greater than 0".to_string());
    }

    let store = open_pstore(&data, &wal)?;
    for txn_index in 0..txns {
        let payload = format!("txn={txn_index}");
        let mut txn = store.begin().map_err(|error| error.to_string())?;
        let page = txn
            .allocate(PageKind::Data, payload.as_bytes())
            .map_err(|error| error.to_string())?;
        let info = txn.commit().map_err(|error| error.to_string())?;
        println!(
            "committed={} txn={} lsn={} pages={} page={}",
            txn_index + 1,
            info.txn_id,
            info.commit_lsn,
            info.pages,
            page.get()
        );
        std::io::stdout()
            .flush()
            .map_err(|error| format!("flush stdout: {error}"))?;
        if sleep_ms > 0 {
            thread::sleep(Duration::from_millis(sleep_ms as u64));
        }
    }
    Ok(())
}

fn arg_values(args: &[String], flag: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut index = 0;
    while index + 1 < args.len() {
        if args[index] == flag {
            values.push(args[index + 1].clone());
            index += 1;
        }
        index += 1;
    }
    values
}

fn run_cas(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("put") => {
            let root = required_arg(args, "--store")?;
            let file = required_arg(args, "--file")?;
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let bytes = std::fs::read(&file).map_err(|error| format!("read {file}: {error}"))?;
            let hash = store.put(&bytes).map_err(|error| error.to_string())?;
            println!("hash={hash} bytes={}", bytes.len());
            Ok(())
        }
        Some("get") => {
            let root = required_arg(args, "--store")?;
            let hash = required_arg(args, "--hash")?;
            let out = required_arg(args, "--out")?;
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let hash = tritdb_cas::Hash::from_hex(&hash).map_err(|error| error.to_string())?;
            let bytes = store.get(&hash).map_err(|error| error.to_string())?;
            std::fs::write(&out, &bytes).map_err(|error| format!("write {out}: {error}"))?;
            println!("hash={hash} bytes={}", bytes.len());
            Ok(())
        }
        Some("has") => {
            let root = required_arg(args, "--store")?;
            let hash = required_arg(args, "--hash")?;
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let hash = tritdb_cas::Hash::from_hex(&hash).map_err(|error| error.to_string())?;
            println!("hash={hash} present={}", store.has(&hash));
            Ok(())
        }
        Some("gc") => {
            let root = required_arg(args, "--store")?;
            let roots = arg_values(args, "--root");
            if roots.is_empty() {
                return Err("cas gc requires at least one --root <hex>".to_string());
            }
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let roots = roots
                .iter()
                .map(|text| tritdb_cas::Hash::from_hex(text))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            let report = store.gc(&roots).map_err(|error| error.to_string())?;
            println!(
                "reachable={} removed={} bytes_removed={}",
                report.reachable, report.removed, report.bytes_removed
            );
            Ok(())
        }
        Some(other) => Err(format!("unknown cas subcommand: {other}")),
        None => Err("cas requires `put`, `get`, `has`, or `gc`".to_string()),
    }
}

fn run_manifest(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("commit") => {
            let root = required_arg(args, "--store")?;
            let parent = arg_value(args, "--parent")
                .map(|text| tritdb_cas::Hash::from_hex(&text))
                .transpose()
                .map_err(|error| error.to_string())?;
            let mut entries = std::collections::BTreeMap::new();
            for entry in arg_values(args, "--entry") {
                let (name, hash) = entry
                    .split_once('=')
                    .ok_or_else(|| format!("--entry must be name=hash, got {entry}"))?;
                let hash = tritdb_cas::Hash::from_hex(hash).map_err(|error| error.to_string())?;
                entries.insert(name.to_string(), hash);
            }
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let manifest = tritdb_cas::Manifest::new(parent, entries);
            let hash = manifest.store(&store).map_err(|error| error.to_string())?;
            println!(
                "manifest={hash} parent={} entries={}",
                parent.map_or("-".to_string(), |hash| hash.to_hex()),
                manifest.entries.len()
            );
            Ok(())
        }
        Some("verify") => {
            let root = required_arg(args, "--store")?;
            let head = required_arg(args, "--head")?;
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let head = tritdb_cas::Hash::from_hex(&head).map_err(|error| error.to_string())?;
            let info =
                tritdb_cas::verify_chain(&store, &head).map_err(|error| error.to_string())?;
            println!(
                "head={} root={} length={}",
                info.head, info.root, info.length
            );
            Ok(())
        }
        Some("show") => {
            let root = required_arg(args, "--store")?;
            let hash = required_arg(args, "--hash")?;
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let hash = tritdb_cas::Hash::from_hex(&hash).map_err(|error| error.to_string())?;
            let manifest =
                tritdb_cas::Manifest::load(&store, &hash).map_err(|error| error.to_string())?;
            println!(
                "manifest={hash} version={} parent={} created_ms={} entries={}",
                manifest.version,
                manifest
                    .parent
                    .map_or("-".to_string(), |hash| hash.to_hex()),
                manifest.created_ms,
                manifest.entries.len()
            );
            for (name, hash) in &manifest.entries {
                println!("entry={name} hash={hash}");
            }
            Ok(())
        }
        Some("replicate") => {
            let from = required_arg(args, "--from")?;
            let to = required_arg(args, "--to")?;
            let head = required_arg(args, "--head")?;
            let source = tritdb_cas::ChunkStore::open(std::path::Path::new(&from))
                .map_err(|error| error.to_string())?;
            let target = tritdb_cas::ChunkStore::open(std::path::Path::new(&to))
                .map_err(|error| error.to_string())?;
            let head = tritdb_cas::Hash::from_hex(&head).map_err(|error| error.to_string())?;
            let names = arg_values(args, "--entry");
            let info = if names.is_empty() {
                tritdb_cas::replicate(&source, &target, &head)
            } else {
                tritdb_cas::replicate_subset(&source, &target, &head, &names)
            }
            .map_err(|error| error.to_string())?;
            println!(
                "head={} manifests={} chunks={} bytes={}",
                info.head, info.manifests, info.chunks, info.bytes
            );
            Ok(())
        }
        Some("snapshot-pages") => manifest_snapshot_pages(args),
        Some("restore-pages") => manifest_restore_pages(args),
        Some("snapshot") => {
            let root = required_arg(args, "--store")?;
            let db = required_arg(args, "--db")?;
            let parent = arg_value(args, "--parent")
                .map(|text| tritdb_cas::Hash::from_hex(&text))
                .transpose()
                .map_err(|error| error.to_string())?;
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let txn = tritdb_txn::TxnStore::open(std::path::Path::new(&db))
                .map_err(|error| error.to_string())?;
            let entries = txn.iter().map_err(|error| error.to_string())?;
            let mut manifest_entries = std::collections::BTreeMap::new();
            for (key, value) in entries {
                let name =
                    String::from_utf8(key).map_err(|error| format!("non-utf8 key: {error}"))?;
                let hash = store.put(&value).map_err(|error| error.to_string())?;
                manifest_entries.insert(name, hash);
            }
            drop(txn);
            let manifest = tritdb_cas::Manifest::new(parent, manifest_entries);
            let hash = manifest.store(&store).map_err(|error| error.to_string())?;
            println!(
                "manifest={hash} parent={} entries={}",
                parent.map_or("-".to_string(), |hash| hash.to_hex()),
                manifest.entries.len()
            );
            Ok(())
        }
        Some("restore") => {
            let root = required_arg(args, "--store")?;
            let db = required_arg(args, "--db")?;
            let head = required_arg(args, "--head")?;
            let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                .map_err(|error| error.to_string())?;
            let head = tritdb_cas::Hash::from_hex(&head).map_err(|error| error.to_string())?;
            let manifest =
                tritdb_cas::Manifest::load(&store, &head).map_err(|error| error.to_string())?;
            let txn_store = tritdb_txn::TxnStore::open(std::path::Path::new(&db))
                .map_err(|error| error.to_string())?;
            let mut txn = txn_store.begin().map_err(|error| error.to_string())?;
            for (name, hash) in &manifest.entries {
                let value = store.get(hash).map_err(|error| error.to_string())?;
                txn.put(name.as_bytes(), &value)
                    .map_err(|error| error.to_string())?;
            }
            txn.commit().map_err(|error| error.to_string())?;
            println!("restored={} db={db}", manifest.entries.len());
            Ok(())
        }
        Some(other) => Err(format!("unknown manifest subcommand: {other}")),
        None => Err(
            "manifest requires `commit`, `verify`, `show`, `replicate`, `snapshot`, `restore`, \
             `snapshot-pages`, or `restore-pages`"
                .to_string(),
        ),
    }
}

const PAGE_SNAPSHOT_LAYOUT: &[u8; 4] = b"TRPF";
const PAGE_SNAPSHOT_VERSION: u8 = 1;
const PAGE_SNAPSHOT_LAYOUT_LEN: usize = 18;

/// Splits the page file into one CAS chunk per 8 KiB page.
///
/// The manifest carries a `layout` entry (`TRPF`, version, page size, page
/// count) plus one `page/<index>` entry per page, so a restore can reject a
/// manifest that was built for a different page size.
fn manifest_snapshot_pages(args: &[String]) -> Result<(), String> {
    let root = required_arg(args, "--store")?;
    let data = required_arg(args, "--data")?;
    let parent = arg_value(args, "--parent")
        .map(|text| tritdb_cas::Hash::from_hex(&text))
        .transpose()
        .map_err(|error| error.to_string())?;
    let bytes = std::fs::read(&data).map_err(|error| format!("read {data}: {error}"))?;
    if bytes.len() % PAGE_SIZE != 0 {
        return Err(format!(
            "page file {data} is {} bytes, not a multiple of {PAGE_SIZE}",
            bytes.len()
        ));
    }
    let page_count = bytes.len() / PAGE_SIZE;

    let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
        .map_err(|error| error.to_string())?;
    let mut entries = std::collections::BTreeMap::new();
    for index in 0..page_count {
        let start = index * PAGE_SIZE;
        let hash = store
            .put(&bytes[start..start + PAGE_SIZE])
            .map_err(|error| error.to_string())?;
        entries.insert(format!("page/{index:016x}"), hash);
    }

    let mut layout = Vec::with_capacity(PAGE_SNAPSHOT_LAYOUT_LEN);
    layout.extend_from_slice(PAGE_SNAPSHOT_LAYOUT);
    layout.push(PAGE_SNAPSHOT_VERSION);
    layout.push(0);
    layout.extend_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    layout.extend_from_slice(&(page_count as u64).to_le_bytes());
    let layout_hash = store.put(&layout).map_err(|error| error.to_string())?;
    entries.insert("layout".to_string(), layout_hash);

    let manifest = tritdb_cas::Manifest::new(parent, entries);
    let hash = manifest.store(&store).map_err(|error| error.to_string())?;
    println!(
        "manifest={hash} parent={} pages={page_count} bytes={}",
        parent.map_or("-".to_string(), |hash| hash.to_hex()),
        bytes.len()
    );
    Ok(())
}

/// Rebuilds a page file from a snapshot produced by `manifest snapshot-pages`.
fn manifest_restore_pages(args: &[String]) -> Result<(), String> {
    let root = required_arg(args, "--store")?;
    let head = required_arg(args, "--head")?;
    let out = required_arg(args, "--out")?;
    let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
        .map_err(|error| error.to_string())?;
    let head = tritdb_cas::Hash::from_hex(&head).map_err(|error| error.to_string())?;
    let manifest = tritdb_cas::Manifest::load(&store, &head).map_err(|error| error.to_string())?;

    let layout_hash = manifest
        .entries
        .get("layout")
        .ok_or_else(|| format!("manifest {head} has no `layout` entry"))?;
    let layout = store.get(layout_hash).map_err(|error| error.to_string())?;
    if layout.len() != PAGE_SNAPSHOT_LAYOUT_LEN || &layout[0..4] != PAGE_SNAPSHOT_LAYOUT {
        return Err(format!("manifest {head} has a malformed page layout"));
    }
    if layout[4] != PAGE_SNAPSHOT_VERSION {
        return Err(format!("unsupported page layout version {}", layout[4]));
    }
    let page_size = u32::from_le_bytes([layout[6], layout[7], layout[8], layout[9]]) as usize;
    if page_size != PAGE_SIZE {
        return Err(format!(
            "page snapshot uses {page_size} byte pages, this build uses {PAGE_SIZE}"
        ));
    }
    let page_count = u64::from_le_bytes([
        layout[10], layout[11], layout[12], layout[13], layout[14], layout[15], layout[16],
        layout[17],
    ]);
    let expected_entries = usize::try_from(page_count)
        .ok()
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| "page count overflows this platform".to_string())?;
    if manifest.entries.len() != expected_entries {
        return Err(format!(
            "manifest {head} lists {} entries, expected {expected_entries}",
            manifest.entries.len()
        ));
    }

    let capacity = usize::try_from(page_count)
        .ok()
        .and_then(|count| count.checked_mul(page_size))
        .ok_or_else(|| "page file size overflows this platform".to_string())?;
    let mut bytes = Vec::with_capacity(capacity);
    for index in 0..page_count {
        let name = format!("page/{index:016x}");
        let hash = manifest
            .entries
            .get(&name)
            .ok_or_else(|| format!("manifest {head} is missing {name}"))?;
        let page = store.get(hash).map_err(|error| error.to_string())?;
        if page.len() != page_size {
            return Err(format!(
                "{name} is {} bytes, expected {page_size}",
                page.len()
            ));
        }
        bytes.extend_from_slice(&page);
    }

    let tmp = format!("{out}.tmp");
    std::fs::write(&tmp, &bytes).map_err(|error| format!("write {tmp}: {error}"))?;
    std::fs::rename(&tmp, &out).map_err(|error| format!("rename {tmp} -> {out}: {error}"))?;
    println!("pages={page_count} bytes={} out={out}", bytes.len());
    Ok(())
}

fn run_anchor(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("write") => {
            let out = required_arg(args, "--out")?;
            let head = required_arg(args, "--head")?;
            let head = tritdb_cas::Hash::from_hex(&head).map_err(|error| error.to_string())?;
            let mut anchor = tritdb_cas::Anchor::new(head);
            if let Some(key_file) = arg_value(args, "--key-file") {
                let key = std::fs::read(&key_file)
                    .map_err(|error| format!("read {key_file}: {error}"))?;
                anchor.sign(&key);
            }
            let bytes = anchor.encode();
            std::fs::write(&out, &bytes).map_err(|error| format!("write {out}: {error}"))?;
            println!(
                "anchor={out} head={} signed={}",
                anchor.head,
                anchor.hmac.is_some()
            );
            Ok(())
        }
        Some("verify") => {
            let file = required_arg(args, "--file")?;
            let bytes = std::fs::read(&file).map_err(|error| format!("read {file}: {error}"))?;
            let anchor = tritdb_cas::Anchor::decode(&bytes).map_err(|error| error.to_string())?;
            if let Some(expected) = arg_value(args, "--head") {
                let expected =
                    tritdb_cas::Hash::from_hex(&expected).map_err(|error| error.to_string())?;
                if expected != anchor.head {
                    return Err(format!(
                        "anchor head {} does not match expected {expected}",
                        anchor.head
                    ));
                }
            }
            if let Some(key_file) = arg_value(args, "--key-file") {
                let key = std::fs::read(&key_file)
                    .map_err(|error| format!("read {key_file}: {error}"))?;
                if !anchor.verify(&key) {
                    return Err(format!("anchor {file} signature mismatch"));
                }
            }
            let mut chain_length = None;
            if let Some(root) = arg_value(args, "--store") {
                let store = tritdb_cas::ChunkStore::open(std::path::Path::new(&root))
                    .map_err(|error| error.to_string())?;
                let info = tritdb_cas::verify_chain(&store, &anchor.head)
                    .map_err(|error| error.to_string())?;
                chain_length = Some(info.length);
            }
            println!(
                "anchor={file} head={} signed={} chain_length={}",
                anchor.head,
                anchor.hmac.is_some(),
                chain_length.map_or("-".to_string(), |length| length.to_string())
            );
            Ok(())
        }
        Some(other) => Err(format!("unknown anchor subcommand: {other}")),
        None => Err("anchor requires `write` or `verify`".to_string()),
    }
}

fn run_graph(args: &[String]) -> Result<(), String> {
    match args.get(1).map(String::as_str) {
        Some("build") => graph_build(args),
        Some("load") => graph_load(args),
        Some("put-points") => graph_put_points(args),
        Some("reach") => graph_reach(args),
        Some("neighbors") => graph_neighbors(args),
        Some("knn") => graph_knn(args),
        Some(other) => Err(format!("unknown graph subcommand: {other}")),
        None => Err("graph requires `build`, `load`, `reach`, `neighbors`, or `knn`".to_string()),
    }
}

fn parse_edges(text: &str) -> Result<Vec<(u32, u32)>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    text.split(',')
        .map(|part| {
            let (from, to) = part
                .split_once('-')
                .ok_or_else(|| format!("edge must be from-to, got {part}"))?;
            let from = from
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid node: {from}"))?;
            let to = to
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("invalid node: {to}"))?;
            Ok((from, to))
        })
        .collect()
}

fn parse_truth(text: &str) -> Result<TruthValue, String> {
    match text.trim().to_ascii_uppercase().as_str() {
        "T" | "TRUE" => Ok(TruthValue::True),
        "U" | "UNKNOWN" | "NULL" => Ok(TruthValue::Unknown),
        "F" | "FALSE" => Ok(TruthValue::False),
        other => Err(format!("invalid truth value: {other} (use T, U, or F)")),
    }
}

fn parse_truth_list(text: Option<&str>) -> Result<Vec<TruthValue>, String> {
    match text {
        Some(text) if !text.trim().is_empty() => text.split(',').map(parse_truth).collect(),
        _ => Ok(Vec::new()),
    }
}

fn parse_vector(text: &str) -> Result<Vec<i64>, String> {
    text.split(',')
        .map(|part| {
            part.trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid coordinate: {part}"))
        })
        .collect()
}

fn parse_points(text: &str) -> Result<Vec<Vec<i64>>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    text.split(';').map(parse_vector).collect()
}

fn graph_edges(args: &[String]) -> Result<Vec<(u32, u32, TruthValue)>, String> {
    let edges = parse_edges(&required_arg(args, "--edges")?)?;
    let mut labels = std::collections::BTreeMap::new();
    for spec in arg_values(args, "--label") {
        let (pair, value) = spec
            .split_once('=')
            .ok_or_else(|| format!("--label must be from-to=T|U|F, got {spec}"))?;
        let (from, to) = pair
            .split_once('-')
            .ok_or_else(|| format!("--label must be from-to=T|U|F, got {spec}"))?;
        let from = from
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("invalid node: {from}"))?;
        let to = to
            .trim()
            .parse::<u32>()
            .map_err(|_| format!("invalid node: {to}"))?;
        labels.insert((from, to), parse_truth(value)?);
    }
    Ok(edges
        .into_iter()
        .map(|(from, to)| {
            (
                from,
                to,
                labels.get(&(from, to)).copied().unwrap_or(TruthValue::True),
            )
        })
        .collect())
}

fn graph_node_count(args: &[String], edges: &[(u32, u32, TruthValue)]) -> Result<usize, String> {
    let declared = arg_usize(args, "--nodes", 0);
    if declared > 0 {
        return Ok(declared);
    }
    match edges.iter().map(|&(from, to, _)| from.max(to)).max() {
        Some(max) => Ok(max as usize + 1),
        None => Err("pass --nodes or --edges".to_string()),
    }
}

fn build_graph(args: &[String]) -> Result<tritdb_exec::graph::AnnotatedGraph, String> {
    match arg_value(args, "--wal") {
        Some(wal) => load_stored_graph(args, &wal),
        None => build_graph_from_edges(args),
    }
}

fn build_graph_from_edges(args: &[String]) -> Result<tritdb_exec::graph::AnnotatedGraph, String> {
    let edges = graph_edges(args)?;
    let nodes = graph_node_count(args, &edges)?;
    tritdb_exec::graph::AnnotatedGraph::from_edges(nodes, &edges).map_err(|error| error.to_string())
}

fn graph_key(name: &str) -> Vec<u8> {
    format!("graph/{name}").into_bytes()
}

fn load_stored_graph(
    args: &[String],
    wal: &str,
) -> Result<tritdb_exec::graph::AnnotatedGraph, String> {
    let name = required_arg(args, "--name")?;
    let store = TxnStore::open(std::path::Path::new(wal))
        .map_err(|error| format!("open {wal}: {error}"))?;
    let bytes = store
        .get(&graph_key(&name))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("graph {name} not found in {wal}"))?;
    tritdb_exec::graph::AnnotatedGraph::decode(&bytes).map_err(|error| error.to_string())
}

fn graph_build(args: &[String]) -> Result<(), String> {
    let graph = build_graph_from_edges(args)?;
    let wal = required_arg(args, "--wal")?;
    let name = required_arg(args, "--name")?;
    let bytes = graph.encode();
    let store = TxnStore::open(std::path::Path::new(&wal))
        .map_err(|error| format!("open {wal}: {error}"))?;
    let mut txn = store.begin().map_err(|error| error.to_string())?;
    txn.put(&graph_key(&name), &bytes)
        .map_err(|error| error.to_string())?;
    let info = txn.commit().map_err(|error| error.to_string())?;
    println!(
        "name={name} nodes={} edges={} bytes={} txn={} lsn={}",
        graph.node_count(),
        graph.edge_count(),
        bytes.len(),
        info.txn_id,
        info.commit_lsn
    );
    Ok(())
}

fn graph_load(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let name = required_arg(args, "--name")?;
    let graph = build_graph(args)?;
    println!(
        "name={name} wal={wal} nodes={} edges={}",
        graph.node_count(),
        graph.edge_count()
    );
    Ok(())
}

fn graph_reach(args: &[String]) -> Result<(), String> {
    let graph = build_graph(args)?;
    let from = required_arg(args, "--from")?
        .parse::<u32>()
        .map_err(|_| "invalid --from".to_string())?;
    let to = required_arg(args, "--to")?
        .parse::<u32>()
        .map_err(|_| "invalid --to".to_string())?;
    let max_hops = arg_usize(args, "--max-hops", graph.node_count());
    let reach = graph
        .reach(from, to, max_hops)
        .map_err(|error| error.to_string())?;
    let answer = match reach.answer {
        tritdb_exec::ann::RepairAnswer::Certain => "certain",
        tritdb_exec::ann::RepairAnswer::Contingent => "contingent",
        tritdb_exec::ann::RepairAnswer::Impossible => "impossible",
    };
    let hops = reach.hops.map_or("-".to_string(), |hops| hops.to_string());
    let path = if reach.path.is_empty() {
        "-".to_string()
    } else {
        reach
            .path
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    println!(
        "nodes={} edges={} from={from} to={to} max_hops={max_hops} answer={answer} hops={hops} path={path}",
        graph.node_count(),
        graph.edge_count()
    );
    Ok(())
}

fn graph_neighbors(args: &[String]) -> Result<(), String> {
    let graph = build_graph(args)?;
    let node = required_arg(args, "--node")?
        .parse::<u32>()
        .map_err(|_| "invalid --node".to_string())?;
    let hops = arg_usize(args, "--hops", 1);
    let reached = graph
        .reachable_within(node, hops)
        .map_err(|error| error.to_string())?;
    let list = if reached.is_empty() {
        "-".to_string()
    } else {
        reached
            .iter()
            .map(|(node, _)| node.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    println!(
        "node={node} hops={hops} count={} nodes={list}",
        reached.len()
    );
    Ok(())
}

fn graph_put_points(args: &[String]) -> Result<(), String> {
    let wal = required_arg(args, "--wal")?;
    let name = required_arg(args, "--name")?;
    let vectors = parse_points(&required_arg(args, "--points")?)?;
    let labels = parse_truth_list(arg_value(args, "--labels").as_deref())?;
    if !labels.is_empty() && labels.len() != vectors.len() {
        return Err(format!(
            "--labels has {} values for {} points",
            labels.len(),
            vectors.len()
        ));
    }
    let points: Vec<tritdb_exec::graph::Point> = vectors
        .into_iter()
        .enumerate()
        .map(|(id, vector)| tritdb_exec::graph::Point {
            id: id as u32,
            vector,
            label: labels.get(id).copied().unwrap_or(TruthValue::True),
        })
        .collect();
    let bytes = tritdb_exec::graph::encode_points(&points);
    let store = TxnStore::open(std::path::Path::new(&wal))
        .map_err(|error| format!("open {wal}: {error}"))?;
    let mut txn = store.begin().map_err(|error| error.to_string())?;
    txn.put(format!("points/{name}").as_bytes(), &bytes)
        .map_err(|error| error.to_string())?;
    let info = txn.commit().map_err(|error| error.to_string())?;
    println!(
        "name={name} points={} bytes={} txn={} lsn={}",
        points.len(),
        bytes.len(),
        info.txn_id,
        info.commit_lsn
    );
    Ok(())
}

fn graph_knn(args: &[String]) -> Result<(), String> {
    let points = if let Some(wal) = arg_value(args, "--wal") {
        let name = required_arg(args, "--name")?;
        let store = TxnStore::open(std::path::Path::new(&wal))
            .map_err(|error| format!("open {wal}: {error}"))?;
        let bytes = store
            .get(format!("points/{name}").as_bytes())
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("points {name} not found in {wal}"))?;
        tritdb_exec::graph::decode_points(&bytes).map_err(|error| error.to_string())?
    } else {
        let vectors = parse_points(&required_arg(args, "--points")?)?;
        let labels = parse_truth_list(arg_value(args, "--labels").as_deref())?;
        if !labels.is_empty() && labels.len() != vectors.len() {
            return Err(format!(
                "--labels has {} values for {} points",
                labels.len(),
                vectors.len()
            ));
        }
        vectors
            .into_iter()
            .enumerate()
            .map(|(id, vector)| tritdb_exec::graph::Point {
                id: id as u32,
                vector,
                label: labels.get(id).copied().unwrap_or(TruthValue::True),
            })
            .collect()
    };
    let query = parse_vector(&required_arg(args, "--query")?)?;
    let k = arg_usize(args, "--k", 3);
    let answer =
        tritdb_exec::graph::annotated_knn(&points, &query, k).map_err(|error| error.to_string())?;
    for (name, bucket) in [
        ("TRUE", &answer.certain),
        ("UNKNOWN", &answer.uncertain),
        ("FALSE", &answer.excluded),
    ] {
        for (rank, neighbor) in bucket.iter().enumerate() {
            println!(
                "label={name} rank={rank} id={} distance={}",
                neighbor.id, neighbor.distance
            );
        }
    }
    println!(
        "certain={} uncertain={} excluded={}",
        answer.certain.len(),
        answer.uncertain.len(),
        answer.excluded.len()
    );
    Ok(())
}

fn run_models(args: &[String]) -> Result<(), String> {
    let vars = arg_usize(args, "--vars", 0);
    let expr_text = required_arg(args, "--expr")?;
    let expr = parse_bool_expr(&expr_text, vars)?;
    let models = tritdb_exec::ann::count_models(&expr, vars).map_err(|error| error.to_string())?;
    println!("vars={vars} models={models}");
    Ok(())
}

fn run_repair(args: &[String]) -> Result<(), String> {
    let facts = arg_usize(args, "--facts", 0);
    let denials = arg_groups(args, "--denial")?;
    let query = arg_indices(args, "--query")?;
    let answer = tritdb_exec::ann::under_repair(facts, &denials, &query)
        .map_err(|error| error.to_string())?;
    let answer = match answer {
        tritdb_exec::ann::RepairAnswer::Certain => "certain",
        tritdb_exec::ann::RepairAnswer::Impossible => "impossible",
        tritdb_exec::ann::RepairAnswer::Contingent => "contingent",
    };
    println!(
        "facts={} denials={} query={} answer={answer}",
        facts,
        denials.len(),
        join_indices(&query)
    );
    Ok(())
}

fn arg_groups(args: &[String], flag: &str) -> Result<Vec<Vec<usize>>, String> {
    let mut groups = Vec::new();
    let mut index = 0;
    while index + 1 < args.len() {
        if args[index] == flag {
            groups.push(parse_indices(&args[index + 1])?);
            index += 1;
        }
        index += 1;
    }
    Ok(groups)
}

fn arg_indices(args: &[String], flag: &str) -> Result<Vec<usize>, String> {
    match arg_value(args, flag) {
        Some(value) => parse_indices(&value),
        None => Ok(Vec::new()),
    }
}

fn parse_indices(value: &str) -> Result<Vec<usize>, String> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<usize>()
                .map_err(|_| format!("invalid index: {part}"))
        })
        .collect()
}

fn join_indices(indices: &[usize]) -> String {
    if indices.is_empty() {
        return "-".to_string();
    }
    indices
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

struct BoolParser<'a> {
    input: &'a [u8],
    position: usize,
    vars: usize,
}

impl<'a> BoolParser<'a> {
    fn new(input: &'a str, vars: usize) -> Self {
        Self {
            input: input.as_bytes(),
            position: 0,
            vars,
        }
    }

    fn parse(mut self) -> Result<tritdb_exec::ann::BoolExpr, String> {
        let expr = self.parse_or()?;
        self.skip_ws();
        if self.position < self.input.len() {
            return Err(format!("unexpected input at byte {}", self.position));
        }
        Ok(expr)
    }

    fn parse_or(&mut self) -> Result<tritdb_exec::ann::BoolExpr, String> {
        let mut items = vec![self.parse_and()?];
        loop {
            self.skip_ws();
            if self.eat_word("OR") || self.eat("||") {
                items.push(self.parse_and()?);
            } else {
                break;
            }
        }
        if items.len() == 1 {
            Ok(items.pop().expect("one item"))
        } else {
            Ok(tritdb_exec::ann::BoolExpr::Or(items))
        }
    }

    fn parse_and(&mut self) -> Result<tritdb_exec::ann::BoolExpr, String> {
        let mut items = vec![self.parse_not()?];
        loop {
            self.skip_ws();
            if self.eat_word("AND") || self.eat("&&") {
                items.push(self.parse_not()?);
            } else {
                break;
            }
        }
        if items.len() == 1 {
            Ok(items.pop().expect("one item"))
        } else {
            Ok(tritdb_exec::ann::BoolExpr::And(items))
        }
    }

    fn parse_not(&mut self) -> Result<tritdb_exec::ann::BoolExpr, String> {
        self.skip_ws();
        if self.eat_word("NOT") || self.eat("!") {
            return Ok(tritdb_exec::ann::BoolExpr::Not(Box::new(self.parse_not()?)));
        }
        self.parse_atom()
    }

    fn parse_atom(&mut self) -> Result<tritdb_exec::ann::BoolExpr, String> {
        self.skip_ws();
        if self.eat("(") {
            let expr = self.parse_or()?;
            self.skip_ws();
            if !self.eat(")") {
                return Err("missing ')'".to_string());
            }
            return Ok(expr);
        }
        if self.eat_word("TRUE") {
            return Ok(tritdb_exec::ann::BoolExpr::Const(true));
        }
        if self.eat_word("FALSE") {
            return Ok(tritdb_exec::ann::BoolExpr::Const(false));
        }
        let name = self.ident()?;
        let index = parse_var_name(&name, self.vars)?;
        Ok(tritdb_exec::ann::BoolExpr::Var(index))
    }

    fn ident(&mut self) -> Result<String, String> {
        self.skip_ws();
        let start = self.position;
        while self.position < self.input.len()
            && (self.input[self.position].is_ascii_alphanumeric()
                || self.input[self.position] == b'_')
        {
            self.position += 1;
        }
        if start == self.position {
            return Err(format!("expected a variable at byte {start}"));
        }
        Ok(String::from_utf8_lossy(&self.input[start..self.position]).into_owned())
    }

    fn eat(&mut self, token: &str) -> bool {
        let bytes = token.as_bytes();
        if self.input[self.position..].starts_with(bytes) {
            self.position += bytes.len();
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        self.skip_ws();
        let bytes = word.as_bytes();
        if !self.input[self.position..].starts_with(bytes) {
            return false;
        }
        let end = self.position + bytes.len();
        if end < self.input.len()
            && (self.input[end].is_ascii_alphanumeric() || self.input[end] == b'_')
        {
            return false;
        }
        self.position = end;
        true
    }

    fn skip_ws(&mut self) {
        while self.position < self.input.len() && self.input[self.position].is_ascii_whitespace() {
            self.position += 1;
        }
    }
}

fn parse_var_name(name: &str, vars: usize) -> Result<usize, String> {
    let index = if name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic() {
        usize::from(name.as_bytes()[0] - b'a')
    } else if let Some(rest) = name.strip_prefix('v') {
        rest.parse::<usize>()
            .map_err(|_| format!("invalid variable: {name}"))?
    } else {
        name.parse::<usize>()
            .map_err(|_| format!("invalid variable: {name}"))?
    };
    if index >= vars {
        return Err(format!("variable {name} is outside 0..{vars}"));
    }
    Ok(index)
}

fn parse_bool_expr(text: &str, vars: usize) -> Result<tritdb_exec::ann::BoolExpr, String> {
    BoolParser::new(text, vars).parse()
}

fn run_serve(args: &[String]) -> Result<(), String> {
    let db = required_arg(args, "--db")?;
    let listen = arg_value(args, "--listen").unwrap_or_else(|| "127.0.0.1:5432".to_string());

    if arg_value(args, "--password").is_some() && arg_value(args, "--password-file").is_some() {
        return Err("use either --password or --password-file, not both".to_string());
    }
    let scram_verifier_file = arg_value(args, "--scram-verifier-file");
    if scram_verifier_file.is_some()
        && (arg_value(args, "--password").is_some() || arg_value(args, "--password-file").is_some())
    {
        return Err(
            "use either --password/--password-file or --scram-verifier-file, not both".to_string(),
        );
    }
    let auth = match arg_value(args, "--auth")
        .unwrap_or_else(|| "trust".to_string())
        .as_str()
    {
        "trust" => tritdb_pgwire::AuthMode::Trust,
        mode @ ("password" | "md5" | "scram-sha-256" | "scram") => {
            if let Some(path) = &scram_verifier_file {
                if mode != "scram-sha-256" && mode != "scram" {
                    return Err("--scram-verifier-file requires --auth scram-sha-256".to_string());
                }
                let text = std::fs::read_to_string(path)
                    .map_err(|error| format!("read {path}: {error}"))?;
                let verifier = tritdb_pgwire::ScramVerifier::parse(&text)
                    .map_err(|error| error.to_string())?;
                tritdb_pgwire::AuthMode::ScramSha256Verifier(verifier)
            } else {
                let password = if let Some(password) = arg_value(args, "--password") {
                    password
                } else if let Some(path) = arg_value(args, "--password-file") {
                    std::fs::read_to_string(&path)
                        .map_err(|error| format!("read {path}: {error}"))?
                        .trim_end_matches(['\r', '\n'])
                        .to_string()
                } else {
                    return Err(format!(
                        "--auth {mode} requires --password, --password-file, or --scram-verifier-file"
                    ));
                };
                if password.is_empty() {
                    return Err("password must not be empty".to_string());
                }
                match mode {
                    "password" => tritdb_pgwire::AuthMode::Password(password),
                    "md5" => tritdb_pgwire::AuthMode::Md5(password),
                    _ => tritdb_pgwire::AuthMode::ScramSha256(password),
                }
            }
        }
        other => {
            return Err(format!(
                "unknown --auth {other}; expected trust, password, md5, or scram-sha-256"
            ))
        }
    };

    let tls_cert = arg_value(args, "--tls-cert").map(std::path::PathBuf::from);
    let tls_key = arg_value(args, "--tls-key").map(std::path::PathBuf::from);
    let tls_client_ca = arg_value(args, "--tls-client-ca").map(std::path::PathBuf::from);
    let tls_client_revoked = arg_value(args, "--tls-client-revoked").map(std::path::PathBuf::from);
    if tls_client_revoked.is_some() && tls_client_ca.is_none() {
        return Err("--tls-client-revoked needs --tls-client-ca".to_string());
    }
    let tls_client_ocsp = arg_value(args, "--tls-client-ocsp").map(std::path::PathBuf::from);
    if tls_client_ocsp.is_some() && tls_client_ca.is_none() {
        return Err("--tls-client-ocsp needs --tls-client-ca".to_string());
    }
    let tls_client_ocsp_url = arg_value(args, "--tls-client-ocsp-url");
    if tls_client_ocsp_url.is_some() && tls_client_ca.is_none() {
        return Err("--tls-client-ocsp-url needs --tls-client-ca".to_string());
    }
    if tls_cert.is_some() != tls_key.is_some() {
        return Err("--tls-cert and --tls-key must be given together".to_string());
    }
    let require_tls = args.iter().any(|argument| argument == "--require-tls");
    if require_tls && tls_cert.is_none() {
        return Err("--require-tls needs --tls-cert and --tls-key".to_string());
    }
    let allow_plaintext_password = args
        .iter()
        .any(|argument| argument == "--allow-plaintext-password");
    if matches!(auth, tritdb_pgwire::AuthMode::Password(_))
        && tls_cert.is_none()
        && !allow_plaintext_password
    {
        return Err(
            "--auth password over a plaintext connection needs --allow-plaintext-password \
             (or --tls-cert/--tls-key)"
                .to_string(),
        );
    }

    let options = tritdb_pgwire::ServerOptions {
        auth,
        tls_cert,
        tls_key,
        tls_client_ca,
        tls_client_revoked,
        tls_client_ocsp,
        tls_client_ocsp_url,
        require_tls,
        allow_plaintext_password,
    };
    let listener =
        std::net::TcpListener::bind(&listen).map_err(|error| format!("bind {listen}: {error}"))?;
    let local = listener.local_addr().map_err(|error| error.to_string())?;
    println!("listening={local} db={db}");
    tritdb_pgwire::serve_with_options(
        std::path::PathBuf::from(db),
        listener,
        std::sync::Arc::new(options),
    )
    .map_err(|error| error.to_string())
}

fn run_scram_verifier(args: &[String]) -> Result<(), String> {
    let password = required_arg(args, "--password")?;
    if password.is_empty() {
        return Err("password must not be empty".to_string());
    }
    let iterations = match arg_value(args, "--iterations") {
        Some(value) => value
            .parse::<u32>()
            .map_err(|_| format!("bad --iterations: {value}"))?,
        None => 4_096,
    };
    let out = required_arg(args, "--out")?;
    let verifier = tritdb_pgwire::ScramVerifier::from_password(&password, iterations)
        .map_err(|error| error.to_string())?;
    std::fs::write(&out, format!("{}\n", verifier.encode()))
        .map_err(|error| format!("write {out}: {error}"))?;
    println!("wrote={out} iterations={iterations}");
    Ok(())
}

fn run_sql(args: &[String]) -> Result<(), String> {
    let db = required_arg(args, "--db")?;
    if arg_value(args, "--sql").is_some() && arg_value(args, "--file").is_some() {
        return Err("use either --sql or --file, not both".to_string());
    }
    let script = if let Some(sql) = arg_value(args, "--sql") {
        sql
    } else if let Some(file) = arg_value(args, "--file") {
        std::fs::read_to_string(&file).map_err(|error| format!("read {file}: {error}"))?
    } else {
        return Err("sql requires --sql or --file".to_string());
    };

    let mut engine =
        SqlEngine::open(Path::new(&db)).map_err(|error| format!("open {db}: {error}"))?;
    let results = engine
        .execute_script(&script)
        .map_err(|error| error.to_string())?;
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            println!();
        }
        print_query_result(result);
    }
    Ok(())
}

fn print_query_result(result: &QueryResult) {
    match result {
        QueryResult::Rows { columns, rows } => {
            println!("{}", columns.join("\t"));
            for row in rows {
                let values: Vec<String> = row.iter().map(|value| value.display()).collect();
                println!("{}", values.join("\t"));
            }
        }
        QueryResult::Message(message) => println!("{message}"),
    }
}

fn preview_bytes(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => format!("{text:?}"),
        Err(_) => {
            let preview_len = bytes.len().min(32);
            let hex: Vec<String> = bytes[..preview_len]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            format!("0x{}", hex.join(""))
        }
    }
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

fn arg_u64(args: &[String], flag: &str) -> Result<u64, String> {
    let raw = required_arg(args, flag)?;
    raw.parse()
        .map_err(|_| format!("invalid {flag}: {raw} (expected u64)"))
}

fn required_arg(args: &[String], flag: &str) -> Result<String, String> {
    arg_value(args, flag).ok_or_else(|| format!("missing required argument: {flag}"))
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

const XL_MAGIC: [u8; 4] = *b"TRX1";
const XL_MAX_NAME: usize = 64;
const XL_MAX_PAYLOAD: usize = 128 * 1024 * 1024;
type KvPair = (Vec<u8>, Vec<u8>);

/// Binary framed transport used only by `tools/crosscheck/byte_stream.py`.
///
/// Request and response frames are:
/// `TRX1`, u32 name length, name bytes, u64 payload length, payload bytes.
fn run_crosscheck() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();

    let mut header = [0u8; 12];
    loop {
        match input.read_exact(&mut header) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(format!("crosscheck: read frame header: {error}")),
        }
        if header[0..4] != XL_MAGIC {
            return Err("crosscheck: bad request magic".to_string());
        }
        let name_len = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if name_len == 0 || name_len > XL_MAX_NAME {
            return Err(format!(
                "crosscheck: invalid request name length {name_len}"
            ));
        }
        let payload_len =
            u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        if payload_len > XL_MAX_PAYLOAD {
            return Err(format!(
                "crosscheck: request payload too large: {payload_len}"
            ));
        }
        let mut name = vec![0u8; name_len];
        input
            .read_exact(&mut name)
            .map_err(|error| format!("crosscheck: read name: {error}"))?;
        let name = String::from_utf8(name).map_err(|_| "crosscheck: non-utf8 name".to_string())?;
        let mut payload = vec![0u8; payload_len];
        input
            .read_exact(&mut payload)
            .map_err(|error| format!("crosscheck: read payload: {error}"))?;

        let response = crosscheck_respond(&name, &payload)?;

        output
            .write_all(&XL_MAGIC)
            .and_then(|_| output.write_all(&(name_len as u32).to_le_bytes()))
            .and_then(|_| output.write_all(&(response.len() as u32).to_le_bytes()))
            .and_then(|_| output.write_all(name.as_bytes()))
            .and_then(|_| output.write_all(&response))
            .and_then(|_| output.flush())
            .map_err(|error| format!("crosscheck: write response: {error}"))?;
    }
    Ok(())
}

fn crosscheck_respond(name: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    match name {
        "block" => {
            let view = Block::decode(payload).map_err(|error| error.to_string())?;
            let block = Block::from_payload(view.radix(), view.trit_count(), view.payload())
                .map_err(|error| error.to_string())?;
            Ok(block.into_bytes())
        }
        "page" => {
            let view = Page::parse_slice(payload).map_err(|error| error.to_string())?;
            let page = Page::new(view.kind(), view.payload()).map_err(|error| error.to_string())?;
            Ok(page.into_bytes().to_vec())
        }
        "txn" => crosscheck_txn(payload),
        "pstore" => crosscheck_pstore(payload),
        other => Err(format!("crosscheck: unknown request kind: {other}")),
    }
}

fn crosscheck_txn(payload: &[u8]) -> Result<Vec<u8>, String> {
    let entries = decode_kv_pairs(payload)?;
    let dir = crosscheck_dir("txn");
    let wal = dir.join("store.wal");
    let result = (|| -> Result<Vec<u8>, String> {
        let store = TxnStore::open(&wal).map_err(|error| error.to_string())?;
        for (key, value) in &entries {
            let mut txn = store.begin().map_err(|error| error.to_string())?;
            txn.put(key, value).map_err(|error| error.to_string())?;
            txn.commit().map_err(|error| error.to_string())?;
        }
        store.checkpoint().map_err(|error| error.to_string())?;
        drop(store);
        let store = TxnStore::open(&wal).map_err(|error| error.to_string())?;
        let mut encoded = Vec::new();
        let entries = store.iter().map_err(|error| error.to_string())?;
        encode_kv_pairs(
            entries.iter().map(|entry| (&entry.0, &entry.1)),
            &mut encoded,
        );
        Ok(encoded)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn crosscheck_pstore(payload: &[u8]) -> Result<Vec<u8>, String> {
    let request = Page::parse_slice(payload).map_err(|error| error.to_string())?;
    let dir = crosscheck_dir("pstore");
    let data = dir.join("pages.dat");
    let wal = dir.join("pages.wal");
    let result = (|| -> Result<Vec<u8>, String> {
        let store = PageStore::open(&data, &wal).map_err(|error| error.to_string())?;
        let mut txn = store.begin().map_err(|error| error.to_string())?;
        txn.allocate(request.kind(), request.payload())
            .map_err(|error| error.to_string())?;
        txn.commit().map_err(|error| error.to_string())?;
        store.checkpoint().map_err(|error| error.to_string())?;
        drop(store);
        let store = PageStore::open(&data, &wal).map_err(|error| error.to_string())?;
        let page = store
            .read_page(PageId::new(0))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "crosscheck: page 0 missing after reopen".to_string())?;
        Ok(page.into_bytes().to_vec())
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn decode_kv_pairs(payload: &[u8]) -> Result<Vec<KvPair>, String> {
    let mut offset = 0;
    let count = read_u32(payload, &mut offset)? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let key_len = read_u32(payload, &mut offset)? as usize;
        let key = read_bytes(payload, &mut offset, key_len)?;
        let value_len = read_u32(payload, &mut offset)? as usize;
        let value = read_bytes(payload, &mut offset, value_len)?;
        entries.push((key, value));
    }
    if offset != payload.len() {
        return Err("crosscheck: trailing kv request bytes".to_string());
    }
    Ok(entries)
}

fn encode_kv_pairs<'a>(
    entries: impl Iterator<Item = (&'a Vec<u8>, &'a Vec<u8>)>,
    out: &mut Vec<u8>,
) {
    let count = entries.size_hint().0;
    let mut pairs = Vec::with_capacity(count);
    for (key, value) in entries {
        pairs.push((key, value));
    }
    out.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
    for (key, value) in pairs {
        out.extend_from_slice(&(key.len() as u32).to_le_bytes());
        out.extend_from_slice(key);
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> Result<u32, String> {
    let value = read_bytes(bytes, offset, 4)?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_bytes(bytes: &[u8], offset: &mut usize, len: usize) -> Result<Vec<u8>, String> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| "crosscheck: length overflow".to_string())?;
    if end > bytes.len() {
        return Err("crosscheck: short kv request".to_string());
    }
    let value = bytes[*offset..end].to_vec();
    *offset = end;
    Ok(value)
}

fn crosscheck_dir(prefix: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "tritdb-crosscheck-{prefix}-{}-{id}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create crosscheck temp dir");
    dir
}
