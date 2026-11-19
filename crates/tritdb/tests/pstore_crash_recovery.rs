#![forbid(unsafe_code)]
//! Hard-kill recovery test for the page store.
//!
//! The child allocates one page per transaction and commits it. The parent
//! kills the process after the first commit line. Recovery must expose a
//! contiguous page prefix, with every page payload aligned to its page id, and
//! must not lose a committed page. The exact transaction count is deliberately
//! not asserted: scheduling and fsync latency make it non-deterministic.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use tritdb_page::Page;
use tritdb_store::{PageId, PageStore};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "tritdb-pstore-crash-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    fn data(&self) -> PathBuf {
        self.0.join("data.pages")
    }

    fn wal(&self) -> PathBuf {
        self.0.join("data.wal")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn hard_kill_leaves_contiguous_committed_pages() {
    for attempt in 0..5 {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();

        let mut child = Command::new(env!("CARGO_BIN_EXE_tritdb"))
            .arg("pstore")
            .arg("writer")
            .arg("--data")
            .arg(&data)
            .arg("--wal")
            .arg(&wal)
            .arg("--txns")
            .arg("2000")
            .arg("--sleep-ms")
            .arg("1")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn pstore writer");

        let stdout = child.stdout.take().expect("child stdout");
        let mut lines = BufReader::new(stdout).lines();
        let first = lines
            .next()
            .expect("child produced no output")
            .expect("read child output");
        assert!(
            first.starts_with("committed="),
            "unexpected child line: {first}"
        );

        // Kill while the child is likely between WAL append and data fsync.
        thread::sleep(Duration::from_millis(30));
        child.kill().expect("kill child");
        let status = child.wait().expect("wait child");
        assert!(!status.success(), "child should have been killed");

        let store = PageStore::open(&data, &wal).expect("recover page store");
        let recovery = store.recovery();
        let pages = store.page_count();
        eprintln!(
            "attempt {attempt}: pages={pages} records={} committed={} discarded={} replayed={} data_tail={} wal_torn={}",
            recovery.records,
            recovery.committed_txns,
            recovery.discarded_txns,
            recovery.pages_replayed,
            recovery.data_tail_bytes,
            recovery.torn_tail_bytes
        );
        assert!(pages >= 1, "attempt {attempt}: no committed pages");
        assert_eq!(
            pages, recovery.committed_txns as u64,
            "attempt {attempt}: page count must equal committed transactions"
        );
        assert!(
            recovery.discarded_txns <= 1,
            "attempt {attempt}: discarded={}",
            recovery.discarded_txns
        );
        assert_eq!(
            recovery.aborted_txns, 0,
            "attempt {attempt}: no aborts expected"
        );
        assert!(
            recovery.data_pages >= pages,
            "attempt {attempt}: data_pages={} page_count={pages}",
            recovery.data_pages
        );

        for id in 0..pages {
            let page = store
                .read_page(PageId::new(id))
                .unwrap_or_else(|error| panic!("attempt {attempt}: read page {id}: {error}"))
                .unwrap_or_else(|| panic!("attempt {attempt}: page {id} missing"));
            let view = Page::parse(page.as_bytes())
                .unwrap_or_else(|error| panic!("attempt {attempt}: parse page {id}: {error}"));
            let payload = view.payload();
            let text = std::str::from_utf8(payload)
                .unwrap_or_else(|error| panic!("attempt {attempt}: page {id} payload: {error}"));
            let txn = text
                .strip_prefix("txn=")
                .unwrap_or_else(|| panic!("attempt {attempt}: bad page {id} payload {text:?}"))
                .parse::<u64>()
                .unwrap_or_else(|error| panic!("attempt {attempt}: parse page {id} txn: {error}"));
            assert_eq!(
                txn, id,
                "attempt {attempt}: pages must be contiguous and payload-aligned"
            );
        }
    }
}
