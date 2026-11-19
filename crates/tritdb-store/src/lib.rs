#![forbid(unsafe_code)]
//! 8 KiB page store with a single writer, many readers, and WAL page images.
//!
//! Data file layout: page `N` starts at byte `N * PAGE_SIZE`; there is no
//! separate superblock. The file length must be a multiple of [`PAGE_SIZE`].
//!
//! A transaction logs each page image as a TRWL `Put` record whose payload is
//! `page_id u64` followed by the 8 KiB page bytes. `Commit` is fsynced first;
//! only then are page images written to the data file and published to the
//! shared cache.
//!
//! Recovery replays only committed page images. Uncommitted `Begin`/`Put`
//! records are discarded. The WAL may have a torn trailing record; the WAL
//! layer truncates it before this store scans.

use std::collections::{BTreeMap, HashMap};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, RwLock};

use tritdb_page::{crc32, Page, PageError, PageKind, PAGE_SIZE};
use tritdb_wal::{RecordKind, TailState, WalError, WalScan, WalWriter};

const CHECKPOINT_MAGIC: [u8; 4] = *b"TRCK";
const CHECKPOINT_VERSION: u8 = 0;
const CHECKPOINT_SIZE: usize = 32;

/// Page number in the data file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(u64);

impl PageId {
    /// Creates a page id.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Numeric id.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Structured page-store errors.
#[derive(Debug)]
pub enum StoreError {
    /// Underlying I/O error.
    Io(std::io::Error),
    /// Invalid page bytes.
    Page(PageError),
    /// WAL error.
    Wal(WalError),
    /// Data file length is not a multiple of the page size.
    BadDataLength {
        /// Observed length.
        len: u64,
        /// Required page size.
        page_size: usize,
    },
    /// A page-image WAL payload has the wrong length.
    MalformedPagePayload {
        /// Observed payload length.
        len: usize,
    },
    /// Page id times page size overflows a byte offset.
    PageIdOverflow {
        /// Page id.
        id: u64,
    },
    /// A WAL record kind is not valid in a page-store WAL.
    UnsupportedRecord {
        /// Record kind.
        kind: RecordKind,
    },
    /// A page image references a transaction with no `Begin`.
    UnknownTxn {
        /// Transaction id.
        txn_id: u64,
    },
    /// A second `Begin` was seen for the same transaction.
    DuplicateBegin {
        /// Transaction id.
        txn_id: u64,
    },
    /// `Commit`/`Abort` has no matching `Begin`.
    MissingBegin {
        /// Transaction id.
        txn_id: u64,
    },
    /// Checkpoint sidecar is malformed.
    BadCheckpoint {
        /// Human-readable reason.
        reason: &'static str,
    },
    /// Checkpoint claims more pages than the recovered data file contains.
    CheckpointPageCount {
        /// Page count recorded in the checkpoint.
        checkpoint: u64,
        /// Page count found in the data file after recovery.
        data: u64,
    },
    /// A lock was poisoned by a panicking thread.
    Poisoned,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(error) => write!(f, "page store io error: {error}"),
            StoreError::Page(error) => write!(f, "page store page error: {error}"),
            StoreError::Wal(error) => write!(f, "page store wal error: {error}"),
            StoreError::BadDataLength { len, page_size } => write!(
                f,
                "data file length {len} is not a multiple of page size {page_size}"
            ),
            StoreError::MalformedPagePayload { len } => {
                write!(f, "malformed page-image payload: {len} bytes")
            }
            StoreError::PageIdOverflow { id } => {
                write!(f, "page id {id} overflows the data-file offset")
            }
            StoreError::UnsupportedRecord { kind } => {
                write!(
                    f,
                    "unsupported record kind in page-store wal: {}",
                    kind.name()
                )
            }
            StoreError::UnknownTxn { txn_id } => {
                write!(f, "page image for unknown txn {txn_id}")
            }
            StoreError::DuplicateBegin { txn_id } => {
                write!(f, "duplicate begin for txn {txn_id}")
            }
            StoreError::MissingBegin { txn_id } => {
                write!(f, "commit/abort for txn {txn_id} without begin")
            }
            StoreError::BadCheckpoint { reason } => {
                write!(f, "bad page-store checkpoint: {reason}")
            }
            StoreError::CheckpointPageCount { checkpoint, data } => write!(
                f,
                "checkpoint page count {checkpoint} exceeds recovered data pages {data}"
            ),
            StoreError::Poisoned => write!(f, "page-store lock poisoned"),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            StoreError::Io(error) => Some(error),
            StoreError::Page(error) => Some(error),
            StoreError::Wal(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        StoreError::Io(error)
    }
}

impl From<PageError> for StoreError {
    fn from(error: PageError) -> Self {
        StoreError::Page(error)
    }
}

impl From<WalError> for StoreError {
    fn from(error: WalError) -> Self {
        StoreError::Wal(error)
    }
}

/// Recovery report captured at open time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryInfo {
    /// Valid WAL records scanned.
    pub records: usize,
    /// WAL records skipped because they are at or before the checkpoint.
    pub records_skipped: usize,
    /// LSN covered by the checkpoint sidecar; `0` means no checkpoint.
    pub checkpoint_lsn: u64,
    /// Transactions applied because they had a `Commit` record.
    pub committed_txns: usize,
    /// Transactions with an `Abort` record.
    pub aborted_txns: usize,
    /// Number of `Begin` transactions with no `Commit`/`Abort`; discarded.
    pub discarded_txns: usize,
    /// Page images rewritten to the data file from committed transactions.
    pub pages_replayed: usize,
    /// Bytes in the torn trailing WAL record, if any.
    pub torn_tail_bytes: u64,
    /// Bytes trimmed from the data file because they were not a whole page.
    ///
    /// A crash can interrupt a page write and leave a partial page. Committed
    /// page images are already durable in the WAL, so the partial tail is
    /// discarded and the committed images are replayed.
    pub data_tail_bytes: u64,
    /// Data-file pages after recovery.
    pub data_pages: u64,
    /// Last known durable LSN: the WAL tail, or the checkpoint if the WAL was
    /// reset and has no records.
    pub last_lsn: u64,
}

/// Result of a successful checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointInfo {
    /// Last WAL LSN whose page images are durable in the data file.
    pub checkpoint_lsn: u64,
    /// Data-file page count at checkpoint time.
    pub page_count: u64,
    /// WAL bytes before the reset.
    pub wal_bytes_before: u64,
    /// WAL bytes after the reset.
    pub wal_bytes_after: u64,
}

/// Result of a successful page commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageCommitInfo {
    /// Transaction id.
    pub txn_id: u64,
    /// LSN of the `Commit` record.
    pub commit_lsn: u64,
    /// Number of page images in the transaction.
    pub pages: usize,
}

/// A page store with one writer and many readers.
pub struct PageStore {
    data_path: PathBuf,
    wal_path: PathBuf,
    checkpoint_path: PathBuf,
    data: Mutex<File>,
    wal: Mutex<WalWriter>,
    cache: RwLock<HashMap<PageId, Page>>,
    page_count: AtomicU64,
    next_page_id: AtomicU64,
    next_txn_id: AtomicU64,
    recovery: RecoveryInfo,
}

impl PageStore {
    /// Opens or creates a page store and replays committed page images.
    pub fn open(data_path: &Path, wal_path: &Path) -> Result<Self, StoreError> {
        let mut data = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(data_path)?;
        let data_tail_bytes = truncate_data_tail(&mut data)?;

        let checkpoint_path = checkpoint_path_for(data_path);
        let checkpoint = read_checkpoint(&checkpoint_path)?;
        let checkpoint_lsn = checkpoint.map_or(0, |info| info.checkpoint_lsn);

        let (wal, scan) = WalWriter::open(wal_path)?;
        let recovered = recover_pages(&mut data, &scan, checkpoint_lsn)?;

        let data_len = data.metadata()?.len();
        let data_pages = data_len / PAGE_SIZE as u64;
        data.sync_all()?;

        if let Some(info) = checkpoint {
            if info.page_count > data_pages {
                return Err(StoreError::CheckpointPageCount {
                    checkpoint: info.page_count,
                    data: data_pages,
                });
            }
        }

        let mut recovery = recovered.info;
        recovery.data_pages = data_pages;
        recovery.data_tail_bytes = data_tail_bytes;
        recovery.checkpoint_lsn = checkpoint_lsn;
        let next_page_id = recovered
            .max_page_id
            .map_or(data_pages, |id| data_pages.max(id.saturating_add(1)));
        let next_txn_id = recovered.max_txn_id.saturating_add(1).max(1);

        Ok(Self {
            data_path: data_path.to_path_buf(),
            wal_path: wal_path.to_path_buf(),
            checkpoint_path,
            data: Mutex::new(data),
            wal: Mutex::new(wal),
            cache: RwLock::new(HashMap::new()),
            page_count: AtomicU64::new(data_pages),
            next_page_id: AtomicU64::new(next_page_id),
            next_txn_id: AtomicU64::new(next_txn_id),
            recovery,
        })
    }

    /// Data-file path.
    pub fn data_path(&self) -> &Path {
        &self.data_path
    }

    /// WAL path.
    pub fn wal_path(&self) -> &Path {
        &self.wal_path
    }

    /// Checkpoint sidecar path (`<data>.ckpt`).
    pub fn checkpoint_path(&self) -> &Path {
        &self.checkpoint_path
    }

    /// Makes all committed pages durable, records a checkpoint, and resets
    /// the WAL to an empty segment.
    ///
    /// The checkpoint sidecar is written before the WAL reset. If a crash
    /// happens between those two steps, recovery sees the checkpoint and skips
    /// the old page images instead of replaying them over newer data.
    pub fn checkpoint(&self) -> Result<CheckpointInfo, StoreError> {
        let mut wal = self.wal.lock().map_err(|_| StoreError::Poisoned)?;
        let data = self.data.lock().map_err(|_| StoreError::Poisoned)?;
        data.sync_all()?;
        let page_count = data.metadata()?.len() / PAGE_SIZE as u64;
        drop(data);

        let checkpoint_lsn = wal.next_lsn().saturating_sub(1);
        let wal_bytes_before = fs::metadata(&self.wal_path)?.len();
        write_checkpoint(&self.checkpoint_path, checkpoint_lsn, page_count)?;
        wal.reset(checkpoint_lsn.saturating_add(1).max(1))?;
        let wal_bytes_after = fs::metadata(&self.wal_path)?.len();

        Ok(CheckpointInfo {
            checkpoint_lsn,
            page_count,
            wal_bytes_before,
            wal_bytes_after,
        })
    }

    /// Recovery report captured at open time.
    pub fn recovery(&self) -> &RecoveryInfo {
        &self.recovery
    }

    /// Number of pages in the data file.
    pub fn page_count(&self) -> u64 {
        self.page_count.load(Ordering::SeqCst)
    }

    /// Number of pages currently cached.
    pub fn cache_len(&self) -> Result<usize, StoreError> {
        let cache = self.cache.read().map_err(|_| StoreError::Poisoned)?;
        Ok(cache.len())
    }

    /// Reads a committed page.
    ///
    /// A cache hit returns immediately. A miss reads the data file and fills
    /// the cache. There is no per-page versioning; this is not MVCC.
    pub fn read_page(&self, id: PageId) -> Result<Option<Page>, StoreError> {
        {
            let cache = self.cache.read().map_err(|_| StoreError::Poisoned)?;
            if let Some(page) = cache.get(&id) {
                return Ok(Some(page.clone()));
            }
        }

        let mut data = self.data.lock().map_err(|_| StoreError::Poisoned)?;
        let len = data.metadata()?.len();
        let offset = page_offset(id)?;
        if offset >= len {
            return Ok(None);
        }

        let mut bytes = [0u8; PAGE_SIZE];
        data.seek(SeekFrom::Start(offset))?;
        data.read_exact(&mut bytes)?;
        drop(data);

        let view = Page::parse(&bytes)?;
        let page = Page::new(view.kind(), view.payload())?;

        let mut cache = self.cache.write().map_err(|_| StoreError::Poisoned)?;
        cache.insert(id, page.clone());
        Ok(Some(page))
    }

    /// Starts the single active writer transaction.
    ///
    /// The returned [`PageTxn`] holds the WAL mutex. Readers do not need it.
    pub fn begin(&self) -> Result<PageTxn<'_>, StoreError> {
        let mut wal = self.wal.lock().map_err(|_| StoreError::Poisoned)?;
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::SeqCst);
        wal.append(RecordKind::Begin, txn_id, &[], false)?;
        Ok(PageTxn {
            store: self,
            wal,
            txn_id,
            pending: BTreeMap::new(),
            next_alloc: None,
        })
    }
}

/// The single active writer transaction over a [`PageStore`].
pub struct PageTxn<'a> {
    store: &'a PageStore,
    wal: MutexGuard<'a, WalWriter>,
    txn_id: u64,
    pending: BTreeMap<PageId, Page>,
    next_alloc: Option<u64>,
}

impl<'a> PageTxn<'a> {
    /// Transaction id.
    pub const fn id(&self) -> u64 {
        self.txn_id
    }

    /// Reads a page, preferring this transaction's uncommitted images.
    pub fn read_page(&self, id: PageId) -> Result<Option<Page>, StoreError> {
        if let Some(page) = self.pending.get(&id) {
            return Ok(Some(page.clone()));
        }
        self.store.read_page(id)
    }

    /// Logs a page image and buffers it for commit.
    pub fn write_page(&mut self, id: PageId, page: &Page) -> Result<u64, StoreError> {
        Page::parse(page.as_bytes())?;
        let payload = encode_page_payload(id, page);
        let lsn = self
            .wal
            .append(RecordKind::Put, self.txn_id, &payload, false)?;
        self.pending.insert(id, page.clone());
        Ok(lsn)
    }

    /// Allocates a new page id and logs a fresh page image.
    ///
    /// The store's page id allocator is only advanced when the transaction
    /// commits; aborting therefore does not leak ids or create file holes.
    pub fn allocate(&mut self, kind: PageKind, payload: &[u8]) -> Result<PageId, StoreError> {
        let page = Page::new(kind, payload)?;
        let next = self
            .next_alloc
            .unwrap_or_else(|| self.store.next_page_id.load(Ordering::SeqCst));
        let id = PageId::new(next);
        self.write_page(id, &page)?;
        self.next_alloc = Some(next.saturating_add(1));
        Ok(id)
    }

    /// Appends `Commit`, fsyncs the WAL, writes page images to the data file,
    /// fsyncs the data file, then publishes the pages to the shared cache.
    pub fn commit(mut self) -> Result<PageCommitInfo, StoreError> {
        let commit_lsn = self
            .wal
            .append(RecordKind::Commit, self.txn_id, &[], true)?;
        let pages = std::mem::take(&mut self.pending);
        let page_count = pages.len();

        // Keep the writer mutex until the data file and cache are updated.
        let mut data = self.store.data.lock().map_err(|_| StoreError::Poisoned)?;
        let mut max_page = self.store.page_count.load(Ordering::SeqCst);
        for (id, page) in &pages {
            write_page_to_file(&mut data, *id, page)?;
            max_page = max_page.max(id.get().saturating_add(1));
        }
        data.sync_all()?;
        drop(data);

        let mut cache = self.store.cache.write().map_err(|_| StoreError::Poisoned)?;
        for (id, page) in pages {
            cache.insert(id, page);
        }
        drop(cache);
        self.store.page_count.store(max_page, Ordering::SeqCst);
        self.store.next_page_id.store(max_page, Ordering::SeqCst);
        drop(self.wal);

        Ok(PageCommitInfo {
            txn_id: self.txn_id,
            commit_lsn,
            pages: page_count,
        })
    }

    /// Appends `Abort` and drops buffered page images.
    pub fn abort(mut self) -> Result<u64, StoreError> {
        let abort_lsn = self
            .wal
            .append(RecordKind::Abort, self.txn_id, &[], false)?;
        drop(self.wal);
        Ok(abort_lsn)
    }
}

struct RecoveredPages {
    info: RecoveryInfo,
    max_page_id: Option<u64>,
    max_txn_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CheckpointFile {
    checkpoint_lsn: u64,
    page_count: u64,
}

fn checkpoint_path_for(data_path: &Path) -> PathBuf {
    let mut name = data_path.as_os_str().to_owned();
    name.push(".ckpt");
    PathBuf::from(name)
}

fn checkpoint_tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

fn read_checkpoint(path: &Path) -> Result<Option<CheckpointFile>, StoreError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if bytes.len() != CHECKPOINT_SIZE {
        return Err(StoreError::BadCheckpoint {
            reason: "checkpoint must be exactly 32 bytes",
        });
    }
    if bytes[0..4] != CHECKPOINT_MAGIC {
        return Err(StoreError::BadCheckpoint {
            reason: "bad magic",
        });
    }
    if bytes[4] != CHECKPOINT_VERSION {
        return Err(StoreError::BadCheckpoint {
            reason: "bad version",
        });
    }
    if bytes[5..8].iter().any(|byte| *byte != 0) || bytes[24..28].iter().any(|byte| *byte != 0) {
        return Err(StoreError::BadCheckpoint {
            reason: "reserved bytes must be zero",
        });
    }
    let stored_crc = u32::from_le_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]);
    let computed_crc = crc32(&bytes[0..28]);
    if stored_crc != computed_crc {
        return Err(StoreError::BadCheckpoint {
            reason: "checksum mismatch",
        });
    }

    let checkpoint_lsn = u64::from_le_bytes([
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    ]);
    let page_count = u64::from_le_bytes([
        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21], bytes[22], bytes[23],
    ]);
    Ok(Some(CheckpointFile {
        checkpoint_lsn,
        page_count,
    }))
}

fn write_checkpoint(path: &Path, checkpoint_lsn: u64, page_count: u64) -> Result<(), StoreError> {
    let mut bytes = [0u8; CHECKPOINT_SIZE];
    bytes[0..4].copy_from_slice(&CHECKPOINT_MAGIC);
    bytes[4] = CHECKPOINT_VERSION;
    bytes[8..16].copy_from_slice(&checkpoint_lsn.to_le_bytes());
    bytes[16..24].copy_from_slice(&page_count.to_le_bytes());
    let crc = crc32(&bytes[0..28]);
    bytes[28..32].copy_from_slice(&crc.to_le_bytes());

    let tmp = checkpoint_tmp_path(path);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

fn recover_pages(
    data: &mut File,
    scan: &WalScan,
    replay_from_lsn: u64,
) -> Result<RecoveredPages, StoreError> {
    let mut pending: BTreeMap<u64, Vec<(PageId, Page)>> = BTreeMap::new();
    let mut committed_txns = 0usize;
    let mut aborted_txns = 0usize;
    let mut pages_replayed = 0usize;
    let mut max_page_id: Option<u64> = None;
    let mut max_txn_id = 0u64;
    let mut records = 0usize;
    let mut records_skipped = 0usize;

    for record in &scan.records {
        if record.lsn <= replay_from_lsn {
            records_skipped += 1;
            continue;
        }
        records += 1;
        max_txn_id = max_txn_id.max(record.txn_id);
        match record.kind {
            RecordKind::Begin => {
                if pending.contains_key(&record.txn_id) {
                    return Err(StoreError::DuplicateBegin {
                        txn_id: record.txn_id,
                    });
                }
                require_empty(record.kind, &record.payload)?;
                pending.insert(record.txn_id, Vec::new());
            }
            RecordKind::Put => {
                let (page_id, page) = decode_page_payload(&record.payload)?;
                max_page_id = Some(max_page_id.map_or(page_id.get(), |id| id.max(page_id.get())));
                pending
                    .get_mut(&record.txn_id)
                    .ok_or(StoreError::UnknownTxn {
                        txn_id: record.txn_id,
                    })?
                    .push((page_id, page));
            }
            RecordKind::Delete => {
                return Err(StoreError::UnsupportedRecord {
                    kind: RecordKind::Delete,
                });
            }
            RecordKind::Commit => {
                require_empty(record.kind, &record.payload)?;
                let pages = pending
                    .remove(&record.txn_id)
                    .ok_or(StoreError::MissingBegin {
                        txn_id: record.txn_id,
                    })?;
                for (page_id, page) in pages {
                    write_page_to_file(data, page_id, &page)?;
                    pages_replayed += 1;
                }
                committed_txns += 1;
            }
            RecordKind::Abort => {
                require_empty(record.kind, &record.payload)?;
                pending
                    .remove(&record.txn_id)
                    .ok_or(StoreError::MissingBegin {
                        txn_id: record.txn_id,
                    })?;
                aborted_txns += 1;
            }
        }
    }

    let torn_tail_bytes = match scan.tail {
        TailState::Clean => 0,
        TailState::Torn { bytes, .. } => bytes,
    };

    Ok(RecoveredPages {
        info: RecoveryInfo {
            records,
            records_skipped,
            checkpoint_lsn: replay_from_lsn,
            committed_txns,
            aborted_txns,
            discarded_txns: pending.len(),
            pages_replayed,
            torn_tail_bytes,
            data_tail_bytes: 0,
            data_pages: 0,
            last_lsn: scan.last_lsn().max(replay_from_lsn),
        },
        max_page_id,
        max_txn_id,
    })
}

fn require_empty(kind: RecordKind, payload: &[u8]) -> Result<(), StoreError> {
    if payload.is_empty() {
        Ok(())
    } else {
        Err(StoreError::UnsupportedRecord { kind })
    }
}

fn encode_page_payload(id: PageId, page: &Page) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8 + PAGE_SIZE);
    payload.extend_from_slice(&id.get().to_le_bytes());
    payload.extend_from_slice(page.as_bytes());
    payload
}

fn decode_page_payload(payload: &[u8]) -> Result<(PageId, Page), StoreError> {
    if payload.len() != 8 + PAGE_SIZE {
        return Err(StoreError::MalformedPagePayload { len: payload.len() });
    }
    let id = u64::from_le_bytes([
        payload[0], payload[1], payload[2], payload[3], payload[4], payload[5], payload[6],
        payload[7],
    ]);
    let view = Page::parse_slice(&payload[8..])?;
    let page = Page::new(view.kind(), view.payload())?;
    Ok((PageId::new(id), page))
}

fn write_page_to_file(data: &mut File, id: PageId, page: &Page) -> Result<(), StoreError> {
    let offset = page_offset(id)?;
    data.seek(SeekFrom::Start(offset))?;
    data.write_all(page.as_bytes())?;
    Ok(())
}

fn page_offset(id: PageId) -> Result<u64, StoreError> {
    id.get()
        .checked_mul(PAGE_SIZE as u64)
        .ok_or(StoreError::PageIdOverflow { id: id.get() })
}

fn truncate_data_tail(data: &mut File) -> Result<u64, StoreError> {
    let len = data.metadata()?.len();
    let aligned = len - (len % PAGE_SIZE as u64);
    if aligned == len {
        return Ok(0);
    }
    data.set_len(aligned)?;
    data.sync_all()?;
    Ok(len - aligned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::thread;

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tritdb-store-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn data(&self) -> PathBuf {
            self.0.join("test.pages")
        }

        fn wal(&self) -> PathBuf {
            self.0.join("test.wal")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn page_text(page: &Page) -> &[u8] {
        Page::parse(page.as_bytes()).expect("parse page").payload()
    }

    #[test]
    fn commit_and_reopen_pages() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let (a, b) = {
            let store = PageStore::open(&data, &wal).expect("open");
            let mut txn = store.begin().expect("begin");
            let a = txn
                .allocate(PageKind::Data, b"hello")
                .expect("allocate data");
            let b = txn
                .allocate(PageKind::Index, b"idx")
                .expect("allocate index");
            let info = txn.commit().expect("commit");
            assert_eq!(info.pages, 2);
            assert_eq!(store.page_count(), 2);
            assert_eq!(
                page_text(&store.read_page(a).expect("read").expect("page")),
                b"hello"
            );
            (a, b)
        };

        let store = PageStore::open(&data, &wal).expect("reopen");
        assert_eq!(store.page_count(), 2);
        assert_eq!(
            page_text(&store.read_page(a).expect("read").expect("page")),
            b"hello"
        );
        assert_eq!(
            page_text(&store.read_page(b).expect("read").expect("page")),
            b"idx"
        );
        assert_eq!(store.recovery().records, 4);
        assert_eq!(store.recovery().committed_txns, 1);
        assert_eq!(store.recovery().discarded_txns, 0);
        assert_eq!(store.recovery().pages_replayed, 2);
        assert_eq!(store.recovery().data_pages, 2);
        assert_eq!(store.recovery().data_tail_bytes, 0);
    }

    #[test]
    fn checkpoint_truncates_wal_and_reopen() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let (a, b, info) = {
            let store = PageStore::open(&data, &wal).expect("open");
            let mut txn = store.begin().expect("begin");
            let a = txn.allocate(PageKind::Data, b"hello").expect("allocate");
            let b = txn.allocate(PageKind::Data, b"world").expect("allocate");
            txn.commit().expect("commit");
            let info = store.checkpoint().expect("checkpoint");
            assert_eq!(info.page_count, 2);
            assert!(info.wal_bytes_before > info.wal_bytes_after);
            assert_eq!(info.wal_bytes_after, 32);
            assert!(info.checkpoint_lsn >= 1);
            assert_eq!(
                fs::metadata(store.checkpoint_path())
                    .expect("ckpt metadata")
                    .len(),
                32
            );
            (a, b, info)
        };

        let store = PageStore::open(&data, &wal).expect("reopen");
        assert_eq!(store.recovery().checkpoint_lsn, info.checkpoint_lsn);
        assert_eq!(store.recovery().records, 0);
        assert_eq!(store.recovery().records_skipped, 0);
        assert_eq!(store.recovery().pages_replayed, 0);
        assert_eq!(store.page_count(), 2);
        assert_eq!(
            page_text(&store.read_page(a).expect("read").expect("page")),
            b"hello"
        );
        assert_eq!(
            page_text(&store.read_page(b).expect("read").expect("page")),
            b"world"
        );
    }

    #[test]
    fn checkpoint_skips_stale_wal_records() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let (page_id, info) = {
            let store = PageStore::open(&data, &wal).expect("open");
            let mut txn = store.begin().expect("begin");
            let page_id = txn.allocate(PageKind::Data, b"stale").expect("allocate");
            txn.commit().expect("commit");
            let old_wal = fs::read(&wal).expect("read old wal");
            let info = store.checkpoint().expect("checkpoint");
            drop(store);
            // Simulate a crash after the sidecar was durable but before WAL reset.
            fs::write(&wal, &old_wal).expect("restore stale wal");
            (page_id, info)
        };

        let store = PageStore::open(&data, &wal).expect("reopen");
        assert_eq!(store.recovery().checkpoint_lsn, info.checkpoint_lsn);
        assert_eq!(store.recovery().records, 0);
        assert_eq!(store.recovery().records_skipped, 3);
        assert_eq!(store.recovery().pages_replayed, 0);
        assert_eq!(store.page_count(), 1);
        assert_eq!(
            page_text(&store.read_page(page_id).expect("read").expect("page")),
            b"stale"
        );
    }

    #[test]
    fn checkpoint_replaces_existing_sidecar() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let (a, b, info) = {
            let store = PageStore::open(&data, &wal).expect("open");
            let mut txn = store.begin().expect("begin");
            let a = txn.allocate(PageKind::Data, b"one").expect("allocate");
            txn.commit().expect("commit");
            let first = store.checkpoint().expect("first checkpoint");
            assert_eq!(first.page_count, 1);

            let mut txn = store.begin().expect("begin");
            let b = txn.allocate(PageKind::Data, b"two").expect("allocate");
            txn.commit().expect("commit");
            let second = store.checkpoint().expect("second checkpoint");
            assert_eq!(second.page_count, 2);
            assert!(second.checkpoint_lsn > first.checkpoint_lsn);
            (a, b, second)
        };

        let store = PageStore::open(&data, &wal).expect("reopen");
        assert_eq!(store.recovery().checkpoint_lsn, info.checkpoint_lsn);
        assert_eq!(store.page_count(), 2);
        assert_eq!(
            page_text(&store.read_page(a).expect("read").expect("page")),
            b"one"
        );
        assert_eq!(
            page_text(&store.read_page(b).expect("read").expect("page")),
            b"two"
        );
    }

    #[test]
    fn torn_data_tail_is_truncated() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let page_id = {
            let store = PageStore::open(&data, &wal).expect("open");
            let mut txn = store.begin().expect("begin");
            let page_id = txn.allocate(PageKind::Data, b"survives").expect("allocate");
            txn.commit().expect("commit");
            page_id
        };

        // Simulate a crash in the middle of the next page write.
        let mut data_file = fs::OpenOptions::new()
            .append(true)
            .open(&data)
            .expect("open data for append");
        std::io::Write::write_all(&mut data_file, &[0xa5; 37]).expect("append torn tail");
        drop(data_file);

        let store = PageStore::open(&data, &wal).expect("reopen");
        assert_eq!(store.recovery().data_tail_bytes, 37);
        assert_eq!(store.page_count(), 1);
        assert_eq!(
            page_text(&store.read_page(page_id).expect("read").expect("page")),
            b"survives"
        );
    }

    #[test]
    fn uncommitted_pages_are_not_applied() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        {
            let store = PageStore::open(&data, &wal).expect("open");
            let mut txn = store.begin().expect("begin");
            let id = txn.allocate(PageKind::Data, b"ghost").expect("allocate");
            assert_eq!(store.page_count(), 0);
            drop(txn);
            assert!(store.read_page(id).expect("read").is_none());
        }

        let store = PageStore::open(&data, &wal).expect("reopen");
        assert_eq!(store.page_count(), 0);
        assert!(store.read_page(PageId::new(0)).expect("read").is_none());
        assert_eq!(store.recovery().committed_txns, 0);
        assert_eq!(store.recovery().discarded_txns, 1);
        assert_eq!(store.recovery().pages_replayed, 0);
    }

    #[test]
    fn abort_discards_pages() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        {
            let store = PageStore::open(&data, &wal).expect("open");
            let mut txn = store.begin().expect("begin");
            txn.allocate(PageKind::Data, b"aborted").expect("allocate");
            let lsn = txn.abort().expect("abort");
            assert!(lsn > 0);
        }

        let store = PageStore::open(&data, &wal).expect("reopen");
        assert_eq!(store.page_count(), 0);
        assert_eq!(store.recovery().aborted_txns, 1);
        assert_eq!(store.recovery().discarded_txns, 0);
    }

    #[test]
    fn aborted_allocate_does_not_leak_page_id() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let store = PageStore::open(&data, &wal).expect("open");
        {
            let mut txn = store.begin().expect("begin");
            txn.allocate(PageKind::Data, b"aborted").expect("allocate");
            txn.abort().expect("abort");
        }

        let mut txn = store.begin().expect("begin again");
        let id = txn
            .allocate(PageKind::Data, b"committed")
            .expect("allocate");
        assert_eq!(id, PageId::new(0));
        txn.commit().expect("commit");
        assert_eq!(store.page_count(), 1);
        let page = store.read_page(id).expect("read").expect("page");
        let view = Page::parse(page.as_bytes()).expect("parse");
        assert_eq!(view.payload(), b"committed");
    }

    #[test]
    fn reads_are_cache_hits_after_commit() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let store = PageStore::open(&data, &wal).expect("open");
        let mut txn = store.begin().expect("begin");
        let id = txn.allocate(PageKind::Data, b"cached").expect("allocate");
        txn.commit().expect("commit");

        store.read_page(id).expect("first read");
        store.read_page(id).expect("second read");
        assert_eq!(store.cache_len().expect("cache len"), 1);
    }

    #[test]
    fn multiple_readers_see_committed_pages() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let store = Arc::new(PageStore::open(&data, &wal).expect("open"));
        for index in 0..8u64 {
            let mut txn = store.begin().expect("begin");
            txn.allocate(PageKind::Data, format!("page-{index}").as_bytes())
                .expect("allocate");
            txn.commit().expect("commit");
        }

        let mut handles = Vec::new();
        for _ in 0..4 {
            let store = Arc::clone(&store);
            handles.push(thread::spawn(move || {
                for _ in 0..1_000 {
                    let page = store
                        .read_page(PageId::new(7))
                        .expect("read")
                        .expect("page");
                    assert_eq!(page_text(&page), b"page-7");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("reader thread");
        }
    }

    #[test]
    fn page_ids_are_monotonic() {
        let dir = TempDir::new();
        let data = dir.data();
        let wal = dir.wal();
        let store = PageStore::open(&data, &wal).expect("open");
        let mut txn = store.begin().expect("begin");
        let first = txn.allocate(PageKind::Data, b"one").expect("allocate");
        let second = txn.allocate(PageKind::Data, b"two").expect("allocate");
        assert_eq!(first.get(), 0);
        assert_eq!(second.get(), 1);
        txn.commit().expect("commit");
        assert_eq!(store.page_count(), 2);
    }

    #[test]
    fn store_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<PageStore>();
    }
}
