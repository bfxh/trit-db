#![forbid(unsafe_code)]
//! TRWL write-ahead log v0.
//!
//! The file starts with a 32-byte segment header, then a sequence of 32-byte
//! record headers followed by payloads:
//!
//! Segment header:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `TRWS` |
//! | 4 | 1 | version (`0`) |
//! | 5 | 3 | reserved (must be `0`) |
//! | 8 | 8 | first LSN |
//! | 16 | 12 | reserved (must be `0`) |
//! | 28 | 4 | CRC32 (IEEE) over header `0..28` |
//!
//! Record header:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `TRWL` |
//! | 4 | 1 | version (`0`) |
//! | 5 | 1 | kind (`1` begin, `2` put, `3` delete, `4` commit, `5` abort) |
//! | 6 | 2 | flags (v0 must be `0`) |
//! | 8 | 8 | LSN (strictly increasing) |
//! | 16 | 8 | transaction id |
//! | 24 | 4 | payload length |
//! | 28 | 4 | CRC32 (IEEE) over header `0..28` + payload |
//! | 32 | … | payload |
//!
//! A record is appended with two writes (header, payload). A process crash may
//! leave a partial trailing record; [`WalReader`] reports it as
//! [`TailState::Torn`], and [`WalWriter::open`] truncates it before appending.
//! A *complete* record with a bad CRC or bad magic is corruption, not a torn
//! tail, and is rejected.
//!
//! CRC is corruption detection only, not a security mechanism. Parsing never
//! panics on arbitrary bytes.

use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use tritdb_page::crc32_parts;

/// Segment magic bytes at offset 0.
pub const SEGMENT_MAGIC: [u8; 4] = *b"TRWS";
/// Segment format version.
pub const SEGMENT_VERSION: u8 = 0;
/// Segment header size in bytes.
pub const SEGMENT_HEADER_SIZE: usize = 32;

/// Record magic bytes at offset 0.
pub const RECORD_MAGIC: [u8; 4] = *b"TRWL";
/// Record format version.
pub const RECORD_VERSION: u8 = 0;
/// Record header size in bytes.
pub const RECORD_HEADER_SIZE: usize = 32;
/// Maximum record payload in bytes.
pub const MAX_RECORD_PAYLOAD: usize = 64 * 1024 * 1024;

/// Logical record type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RecordKind {
    /// Transaction begins.
    Begin = 1,
    /// Key/value write.
    Put = 2,
    /// Key delete.
    Delete = 3,
    /// Transaction committed.
    Commit = 4,
    /// Transaction aborted.
    Abort = 5,
}

impl RecordKind {
    /// Numeric code stored in the record header.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Stable short name.
    pub const fn name(self) -> &'static str {
        match self {
            RecordKind::Begin => "begin",
            RecordKind::Put => "put",
            RecordKind::Delete => "delete",
            RecordKind::Commit => "commit",
            RecordKind::Abort => "abort",
        }
    }

    /// Parses a numeric code.
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(RecordKind::Begin),
            2 => Some(RecordKind::Put),
            3 => Some(RecordKind::Delete),
            4 => Some(RecordKind::Commit),
            5 => Some(RecordKind::Abort),
            _ => None,
        }
    }
}

/// Structured WAL errors.
#[derive(Debug)]
pub enum WalError {
    /// Underlying I/O error.
    Io(std::io::Error),
    /// Segment magic is not `TRWS`.
    BadSegmentMagic {
        /// Bytes that were found.
        found: [u8; 4],
    },
    /// Unsupported segment version.
    BadSegmentVersion {
        /// Version that was found.
        found: u8,
    },
    /// Segment header is too short or has non-zero reserved bytes.
    BadSegmentHeader {
        /// Human-readable reason.
        reason: &'static str,
    },
    /// Record magic is not `TRWL`.
    BadRecordMagic {
        /// Byte offset of the record.
        offset: u64,
        /// Bytes that were found.
        found: [u8; 4],
    },
    /// Unsupported record version.
    BadRecordVersion {
        /// Byte offset of the record.
        offset: u64,
        /// Version that was found.
        found: u8,
    },
    /// Unknown record kind.
    BadRecordKind {
        /// Byte offset of the record.
        offset: u64,
        /// Kind code that was found.
        found: u8,
    },
    /// Reserved record flags are non-zero.
    BadFlags {
        /// Byte offset of the record.
        offset: u64,
        /// Flags that were found.
        found: u16,
    },
    /// Record payload exceeds the v0 limit.
    PayloadTooLarge {
        /// Declared payload length.
        len: usize,
        /// Maximum allowed payload length.
        max: usize,
    },
    /// LSN sequence is not strictly increasing.
    LsnNotMonotonic {
        /// Previous valid LSN.
        previous: u64,
        /// LSN that was found.
        found: u64,
    },
    /// Record CRC does not match.
    ChecksumMismatch {
        /// Byte offset of the record.
        offset: u64,
        /// Stored CRC.
        expected: u32,
        /// Computed CRC.
        actual: u32,
    },
}

impl fmt::Display for WalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalError::Io(error) => write!(f, "wal io error: {error}"),
            WalError::BadSegmentMagic { found } => write!(
                f,
                "bad wal segment magic: found {found:?}, expected {SEGMENT_MAGIC:?}"
            ),
            WalError::BadSegmentVersion { found } => write!(
                f,
                "bad wal segment version: found {found}, expected {SEGMENT_VERSION}"
            ),
            WalError::BadSegmentHeader { reason } => {
                write!(f, "bad wal segment header: {reason}")
            }
            WalError::BadRecordMagic { offset, found } => write!(
                f,
                "bad wal record magic at {offset}: found {found:?}, expected {RECORD_MAGIC:?}"
            ),
            WalError::BadRecordVersion { offset, found } => write!(
                f,
                "bad wal record version at {offset}: found {found}, expected {RECORD_VERSION}"
            ),
            WalError::BadRecordKind { offset, found } => {
                write!(f, "bad wal record kind at {offset}: {found}")
            }
            WalError::BadFlags { offset, found } => {
                write!(f, "bad wal record flags at {offset}: {found}")
            }
            WalError::PayloadTooLarge { len, max } => {
                write!(f, "wal record payload too large: {len} bytes, max {max}")
            }
            WalError::LsnNotMonotonic { previous, found } => write!(
                f,
                "wal LSN not monotonic: previous {previous}, found {found}"
            ),
            WalError::ChecksumMismatch {
                offset,
                expected,
                actual,
            } => write!(
                f,
                "wal checksum mismatch at {offset}: stored {expected:#010x}, computed {actual:#010x}"
            ),
        }
    }
}

impl Error for WalError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            WalError::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for WalError {
    fn from(error: std::io::Error) -> Self {
        WalError::Io(error)
    }
}

/// State of the bytes after the last valid record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailState {
    /// File ends exactly after a valid record.
    Clean,
    /// A trailing record header or payload is incomplete.
    Torn {
        /// Byte offset where the torn record starts.
        offset: u64,
        /// Number of bytes present for that record.
        bytes: u64,
    },
}

/// One decoded WAL record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// Log sequence number.
    pub lsn: u64,
    /// Transaction id.
    pub txn_id: u64,
    /// Record kind.
    pub kind: RecordKind,
    /// Raw payload.
    pub payload: Vec<u8>,
}

/// Result of scanning a WAL file.
#[derive(Clone, Debug)]
pub struct WalScan {
    /// First LSN declared by the segment header.
    pub first_lsn: u64,
    /// Valid records in LSN order.
    pub records: Vec<Record>,
    /// Byte offset after the last valid record.
    pub valid_end: u64,
    /// Whether the trailing bytes are clean or torn.
    pub tail: TailState,
    /// Original file length at scan time.
    pub file_len: u64,
}

impl WalScan {
    /// Last valid LSN, or `0` for an empty WAL.
    pub fn last_lsn(&self) -> u64 {
        self.records.last().map_or(0, |record| record.lsn)
    }
}

/// Append-only WAL writer.
///
/// A writer holds an exclusive OS handle only in the sense that it is the
/// intended single writer; there is no cross-process lock in M0. Opening a
/// second writer on the same path is undefined.
pub struct WalWriter {
    file: File,
    next_lsn: u64,
}

impl WalWriter {
    /// Opens or creates a WAL, truncating a torn tail, and returns a scan of
    /// the bytes as they were before truncation.
    pub fn open(path: &Path) -> Result<(Self, WalScan), WalError> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let len = file.metadata()?.len();

        if len < SEGMENT_HEADER_SIZE as u64 {
            // A truncate-to-zero followed by a crash can leave a partial
            // segment header. There cannot be any valid record in fewer than
            // 32 bytes, so repair the file as a fresh empty segment.
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            write_segment_header(&mut file, 1)?;
            file.sync_all()?;
            let scan = WalScan {
                first_lsn: 1,
                records: Vec::new(),
                valid_end: SEGMENT_HEADER_SIZE as u64,
                tail: TailState::Clean,
                file_len: SEGMENT_HEADER_SIZE as u64,
            };
            return Ok((Self { file, next_lsn: 1 }, scan));
        }

        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::with_capacity(len as usize);
        file.read_to_end(&mut bytes)?;
        let scan = parse_wal(&bytes)?;

        if let TailState::Torn { offset, .. } = scan.tail {
            file.set_len(offset)?;
            file.seek(SeekFrom::Start(offset))?;
        } else {
            file.seek(SeekFrom::Start(scan.valid_end))?;
        }

        let next_lsn = scan
            .records
            .last()
            .map_or(scan.first_lsn, |record| record.lsn.saturating_add(1));

        Ok((Self { file, next_lsn }, scan))
    }

    /// Appends one record and optionally fsyncs it.
    pub fn append(
        &mut self,
        kind: RecordKind,
        txn_id: u64,
        payload: &[u8],
        sync: bool,
    ) -> Result<u64, WalError> {
        if payload.len() > MAX_RECORD_PAYLOAD {
            return Err(WalError::PayloadTooLarge {
                len: payload.len(),
                max: MAX_RECORD_PAYLOAD,
            });
        }

        let payload_len = u32::try_from(payload.len()).map_err(|_| WalError::PayloadTooLarge {
            len: payload.len(),
            max: MAX_RECORD_PAYLOAD,
        })?;
        let lsn = self.next_lsn;

        let mut header = [0u8; RECORD_HEADER_SIZE];
        header[0..4].copy_from_slice(&RECORD_MAGIC);
        header[4] = RECORD_VERSION;
        header[5] = kind.code();
        header[6..8].copy_from_slice(&0u16.to_le_bytes());
        header[8..16].copy_from_slice(&lsn.to_le_bytes());
        header[16..24].copy_from_slice(&txn_id.to_le_bytes());
        header[24..28].copy_from_slice(&payload_len.to_le_bytes());
        let crc = crc32_parts(&header[0..28], payload);
        header[28..32].copy_from_slice(&crc.to_le_bytes());

        self.file.write_all(&header)?;
        self.file.write_all(payload)?;
        if sync {
            self.file.sync_all()?;
        }

        self.next_lsn = self.next_lsn.saturating_add(1);
        Ok(lsn)
    }

    /// Flushes the file to stable storage.
    pub fn sync(&mut self) -> Result<(), WalError> {
        self.file.sync_all()?;
        Ok(())
    }

    /// Replaces the WAL with an empty segment whose first LSN is `first_lsn`.
    ///
    /// The old records are discarded only after the caller has made the durable
    /// state they describe durable elsewhere. A crash between truncation and
    /// header write is repaired by [`WalWriter::open`] as an empty segment.
    pub fn reset(&mut self, first_lsn: u64) -> Result<(), WalError> {
        if first_lsn == 0 {
            return Err(WalError::BadSegmentHeader {
                reason: "first LSN must be non-zero",
            });
        }
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        write_segment_header(&mut self.file, first_lsn)?;
        self.file.sync_all()?;
        self.next_lsn = first_lsn;
        Ok(())
    }

    /// LSN that the next appended record will use.
    pub const fn next_lsn(&self) -> u64 {
        self.next_lsn
    }
}

/// Read-only WAL scanner.
pub struct WalReader;

impl WalReader {
    /// Opens a WAL for reading and scans its current prefix.
    pub fn open(path: &Path) -> Result<WalScan, WalError> {
        let mut file = OpenOptions::new().read(true).open(path)?;
        let len = file.metadata()?.len();
        if len < SEGMENT_HEADER_SIZE as u64 {
            return Err(WalError::BadSegmentHeader {
                reason: "file is shorter than the segment header",
            });
        }
        let mut bytes = Vec::with_capacity(len as usize);
        file.read_to_end(&mut bytes)?;
        parse_wal(&bytes)
    }
}

/// Parses a complete WAL byte image.
pub fn parse_wal(bytes: &[u8]) -> Result<WalScan, WalError> {
    if bytes.len() < SEGMENT_HEADER_SIZE {
        return Err(WalError::BadSegmentHeader {
            reason: "file is shorter than the segment header",
        });
    }

    let found_magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if found_magic != SEGMENT_MAGIC {
        return Err(WalError::BadSegmentMagic { found: found_magic });
    }
    if bytes[4] != SEGMENT_VERSION {
        return Err(WalError::BadSegmentVersion { found: bytes[4] });
    }
    if bytes[5..8].iter().any(|byte| *byte != 0) || bytes[16..28].iter().any(|byte| *byte != 0) {
        return Err(WalError::BadSegmentHeader {
            reason: "reserved bytes must be zero",
        });
    }
    let first_lsn = read_u64(bytes, 8)?;
    if first_lsn == 0 {
        return Err(WalError::BadSegmentHeader {
            reason: "first LSN must be non-zero",
        });
    }
    let stored_crc = read_u32(bytes, 28)?;
    let computed_crc = crc32_parts(&bytes[0..28], &[]);
    if stored_crc != computed_crc {
        return Err(WalError::ChecksumMismatch {
            offset: 0,
            expected: stored_crc,
            actual: computed_crc,
        });
    }

    let mut records = Vec::new();
    let mut offset = SEGMENT_HEADER_SIZE;
    let mut previous_lsn = 0u64;
    let mut tail = TailState::Clean;

    while offset < bytes.len() {
        let remaining = bytes.len() - offset;
        if remaining < RECORD_HEADER_SIZE {
            tail = TailState::Torn {
                offset: offset as u64,
                bytes: remaining as u64,
            };
            break;
        }

        let header = &bytes[offset..offset + RECORD_HEADER_SIZE];
        let found_magic = [header[0], header[1], header[2], header[3]];
        if found_magic != RECORD_MAGIC {
            return Err(WalError::BadRecordMagic {
                offset: offset as u64,
                found: found_magic,
            });
        }
        if header[4] != RECORD_VERSION {
            return Err(WalError::BadRecordVersion {
                offset: offset as u64,
                found: header[4],
            });
        }
        let kind = RecordKind::from_u8(header[5]).ok_or(WalError::BadRecordKind {
            offset: offset as u64,
            found: header[5],
        })?;
        let flags = u16::from_le_bytes([header[6], header[7]]);
        if flags != 0 {
            return Err(WalError::BadFlags {
                offset: offset as u64,
                found: flags,
            });
        }
        let lsn = read_u64(header, 8)?;
        let txn_id = read_u64(header, 16)?;
        let payload_len = read_u32(header, 24)? as usize;
        if payload_len > MAX_RECORD_PAYLOAD {
            return Err(WalError::PayloadTooLarge {
                len: payload_len,
                max: MAX_RECORD_PAYLOAD,
            });
        }
        let total = RECORD_HEADER_SIZE + payload_len;
        if remaining < total {
            tail = TailState::Torn {
                offset: offset as u64,
                bytes: remaining as u64,
            };
            break;
        }

        let payload = &bytes[offset + RECORD_HEADER_SIZE..offset + total];
        let stored_crc = read_u32(header, 28)?;
        let computed_crc = crc32_parts(&header[0..28], payload);
        if stored_crc != computed_crc {
            return Err(WalError::ChecksumMismatch {
                offset: offset as u64,
                expected: stored_crc,
                actual: computed_crc,
            });
        }
        if lsn <= previous_lsn {
            return Err(WalError::LsnNotMonotonic {
                previous: previous_lsn,
                found: lsn,
            });
        }

        records.push(Record {
            lsn,
            txn_id,
            kind,
            payload: payload.to_vec(),
        });
        previous_lsn = lsn;
        offset += total;
    }

    Ok(WalScan {
        first_lsn,
        records,
        valid_end: offset as u64,
        tail,
        file_len: bytes.len() as u64,
    })
}

fn write_segment_header(file: &mut File, first_lsn: u64) -> Result<(), WalError> {
    let mut header = [0u8; SEGMENT_HEADER_SIZE];
    header[0..4].copy_from_slice(&SEGMENT_MAGIC);
    header[4] = SEGMENT_VERSION;
    header[8..16].copy_from_slice(&first_lsn.to_le_bytes());
    let crc = crc32_parts(&header[0..28], &[]);
    header[28..32].copy_from_slice(&crc.to_le_bytes());
    file.write_all(&header)?;
    Ok(())
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, WalError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(WalError::BadSegmentHeader {
            reason: "missing u32 field",
        })?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, WalError> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or(WalError::BadSegmentHeader {
            reason: "missing u64 field",
        })?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tritdb-wal-test-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
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

    #[test]
    fn creates_empty_segment_and_roundtrips_records() {
        let dir = TempDir::new();
        let path = dir.wal();
        let (mut writer, scan) = WalWriter::open(&path).expect("create wal");
        assert!(scan.records.is_empty());
        assert_eq!(scan.tail, TailState::Clean);
        assert_eq!(scan.valid_end, SEGMENT_HEADER_SIZE as u64);

        let l1 = writer
            .append(RecordKind::Begin, 7, &[], false)
            .expect("begin");
        let l2 = writer
            .append(RecordKind::Put, 7, b"payload", false)
            .expect("put");
        let l3 = writer
            .append(RecordKind::Commit, 7, &[], true)
            .expect("commit");
        assert_eq!([l1, l2, l3], [1, 2, 3]);
        drop(writer);

        let scan = WalReader::open(&path).expect("read wal");
        assert_eq!(scan.records.len(), 3);
        assert_eq!(scan.records[0].kind, RecordKind::Begin);
        assert_eq!(scan.records[1].payload, b"payload");
        assert_eq!(scan.records[2].txn_id, 7);
        assert_eq!(scan.last_lsn(), 3);
        assert_eq!(scan.tail, TailState::Clean);
    }

    #[test]
    fn torn_tail_is_reported_then_truncated() {
        let dir = TempDir::new();
        let path = dir.wal();
        let (mut writer, _) = WalWriter::open(&path).expect("create wal");
        writer
            .append(RecordKind::Begin, 1, &[], false)
            .expect("begin");
        drop(writer);

        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for append");
        file.write_all(b"TRW").expect("write torn header");
        drop(file);

        let scan = WalReader::open(&path).expect("scan torn");
        assert_eq!(
            scan.tail,
            TailState::Torn {
                offset: SEGMENT_HEADER_SIZE as u64 + RECORD_HEADER_SIZE as u64,
                bytes: 3
            }
        );

        let (mut writer, scan) = WalWriter::open(&path).expect("open and truncate");
        assert_eq!(scan.records.len(), 1);
        assert_eq!(
            scan.tail,
            TailState::Torn {
                offset: SEGMENT_HEADER_SIZE as u64 + RECORD_HEADER_SIZE as u64,
                bytes: 3
            }
        );
        assert_eq!(writer.next_lsn(), 2);
        assert_eq!(
            fs::metadata(&path).expect("metadata").len(),
            SEGMENT_HEADER_SIZE as u64 + RECORD_HEADER_SIZE as u64
        );

        let lsn = writer
            .append(RecordKind::Commit, 1, &[], true)
            .expect("append after truncate");
        assert_eq!(lsn, 2);
        drop(writer);
        let scan = WalReader::open(&path).expect("read repaired wal");
        assert_eq!(scan.records.len(), 2);
        assert_eq!(scan.tail, TailState::Clean);
    }

    #[test]
    fn reset_truncates_to_a_new_empty_segment() {
        let dir = TempDir::new();
        let path = dir.wal();
        let (mut writer, _) = WalWriter::open(&path).expect("create wal");
        writer
            .append(RecordKind::Begin, 1, &[], false)
            .expect("begin");
        writer.reset(100).expect("reset");
        assert_eq!(writer.next_lsn(), 100);
        drop(writer);

        let scan = WalReader::open(&path).expect("read reset wal");
        assert_eq!(scan.first_lsn, 100);
        assert!(scan.records.is_empty());
        assert_eq!(scan.valid_end, SEGMENT_HEADER_SIZE as u64);
        assert_eq!(scan.tail, TailState::Clean);

        let (mut writer, _) = WalWriter::open(&path).expect("reopen reset wal");
        let lsn = writer
            .append(RecordKind::Commit, 2, &[], true)
            .expect("append after reset");
        assert_eq!(lsn, 100);
    }

    #[test]
    fn open_repairs_a_short_segment_file() {
        let dir = TempDir::new();
        let path = dir.wal();
        fs::write(&path, b"TRW").expect("write short file");

        let (mut writer, scan) = WalWriter::open(&path).expect("repair short wal");
        assert_eq!(scan.first_lsn, 1);
        assert!(scan.records.is_empty());
        assert_eq!(scan.tail, TailState::Clean);
        assert_eq!(
            fs::metadata(&path).expect("metadata").len(),
            SEGMENT_HEADER_SIZE as u64
        );

        let lsn = writer
            .append(RecordKind::Begin, 9, &[], true)
            .expect("append after repair");
        assert_eq!(lsn, 1);
    }

    #[test]
    fn complete_record_with_bad_crc_is_corruption() {
        let dir = TempDir::new();
        let path = dir.wal();
        let (mut writer, _) = WalWriter::open(&path).expect("create wal");
        writer
            .append(RecordKind::Put, 1, b"abc", true)
            .expect("put");
        drop(writer);

        let mut bytes = fs::read(&path).expect("read");
        let payload_offset = SEGMENT_HEADER_SIZE + RECORD_HEADER_SIZE;
        bytes[payload_offset + 1] ^= 0xff;
        fs::write(&path, &bytes).expect("write");

        let error = WalReader::open(&path).expect_err("bad crc must fail");
        assert!(matches!(
            error,
            WalError::ChecksumMismatch {
                offset,
                ..
            } if offset == SEGMENT_HEADER_SIZE as u64
        ));
    }

    #[test]
    fn rejects_bad_segment_magic_and_version() {
        let dir = TempDir::new();
        let path = dir.wal();
        let (writer, _) = WalWriter::open(&path).expect("create wal");
        drop(writer);
        let original = fs::read(&path).expect("read");

        let mut bytes = original.clone();
        bytes[0] = b'X';
        fs::write(&path, &bytes).expect("write");
        assert!(matches!(
            WalReader::open(&path).expect_err("bad magic"),
            WalError::BadSegmentMagic { .. }
        ));

        let mut bytes = original;
        bytes[4] = 9;
        fs::write(&path, &bytes).expect("write");
        assert!(matches!(
            WalReader::open(&path).expect_err("bad version"),
            WalError::BadSegmentVersion { found: 9 }
        ));
    }

    #[test]
    fn rejects_bad_record_kind_and_flags() {
        let dir = TempDir::new();
        let path = dir.wal();
        let (mut writer, _) = WalWriter::open(&path).expect("create wal");
        writer
            .append(RecordKind::Begin, 1, &[], true)
            .expect("begin");
        drop(writer);
        let original = fs::read(&path).expect("read");
        let record_offset = SEGMENT_HEADER_SIZE;

        let mut bytes = original.clone();
        bytes[record_offset + 5] = 99;
        fs::write(&path, &bytes).expect("write");
        assert!(matches!(
            WalReader::open(&path).expect_err("bad kind"),
            WalError::BadRecordKind { found: 99, .. }
        ));

        let mut bytes = original;
        bytes[record_offset + 6] = 1;
        fs::write(&path, &bytes).expect("write");
        assert!(matches!(
            WalReader::open(&path).expect_err("bad flags"),
            WalError::BadFlags { found: 1, .. }
        ));
    }
}
