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

use std::io::Write;
use std::path::Path;
use std::process::exit;
use std::thread;
use std::time::Duration;

use tritdb_format::Block;
use tritdb_page::{Page, PageKind, PAGE_SIZE};
use tritdb_radix::{pack, unpack, Radix, Trit};
use tritdb_store::{PageId, PageStore};
use tritdb_txn::TxnStore;
use tritdb_wal::{TailState, WalReader};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        exit(1);
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
    println!("  txn checkpoint --wal test.wal");
    println!("  txn writer --wal test.wal --txns 1000 --keys-per-txn 4 --sleep-ms 1");
    println!("  pstore alloc --data pstore.pages --wal pstore.wal --kind data --text \"hello\"");
    println!("  pstore put --data pstore.pages --wal pstore.wal --id 0 --text \"hello\"");
    println!("  pstore get --data pstore.pages --wal pstore.wal --id 0");
    println!("  pstore dump --data pstore.pages --wal pstore.wal");
    println!("  pstore checkpoint --data pstore.pages --wal pstore.wal");
    println!("  pstore writer --data pstore.pages --wal pstore.wal --txns 1000 --sleep-ms 1");
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
        Some("writer") => txn_writer(args),
        Some(other) => Err(format!("unknown txn subcommand: {other}")),
        None => Err("txn requires `put`, `get`, `dump`, `checkpoint`, or `writer`".to_string()),
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
    let store = open_store(&wal)?;
    let info = store.checkpoint().map_err(|error| error.to_string())?;
    println!(
        "checkpoint_lsn={} entries={} wal_bytes_before={} wal_bytes_after={} snapshot={} snapshot_bytes={}",
        info.checkpoint_lsn,
        info.entries,
        info.wal_bytes_before,
        info.wal_bytes_after,
        store.snapshot_path().display(),
        info.snapshot_bytes
    );
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
