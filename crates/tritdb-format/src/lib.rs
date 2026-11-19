#![forbid(unsafe_code)]
//! TRBL block format v0.
//!
//! A block is a 20-byte header followed by a packed ternary payload:
//!
//! | offset | size | field |
//! |---|---|---|
//! | 0 | 4 | magic `TRBL` |
//! | 4 | 1 | version (`0`) |
//! | 5 | 1 | radix (`3`, `4`, `16`) |
//! | 6 | 2 | flags (v0 must be `0`) |
//! | 8 | 4 | trit count |
//! | 12 | 4 | payload length (bytes) |
//! | 16 | 4 | CRC32 (IEEE) over header `0..16` + payload |
//! | 20 | … | payload |
//!
//! The payload must be exactly `radix.packed_len(trit_count)` bytes and must
//! use canonical padding. CRC is corruption detection only, not a security
//! mechanism. Decoding never panics on arbitrary bytes.

use std::error::Error;
use std::fmt;

use tritdb_page::crc32_parts;
use tritdb_radix::{pack, unpack_into, Radix, RadixError, Trit};

/// Magic bytes at offset 0.
pub const BLOCK_MAGIC: [u8; 4] = *b"TRBL";
/// Block format version.
pub const BLOCK_VERSION: u8 = 0;
/// Header size in bytes.
pub const BLOCK_HEADER_SIZE: usize = 20;

/// Structured errors returned by TRBL encoding/decoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// Magic bytes are not `TRBL`.
    BadMagic {
        /// The bytes that were found.
        found: [u8; 4],
    },
    /// Unsupported version.
    BadVersion {
        /// The version that was found.
        found: u8,
    },
    /// Unknown radix code.
    BadRadix {
        /// The radix code that was found.
        found: u8,
    },
    /// Reserved flags are non-zero.
    BadFlags {
        /// The flags that were found.
        found: u16,
    },
    /// Input is shorter than the header or the declared payload.
    Truncated {
        /// Expected byte count.
        expected: usize,
        /// Actual byte count.
        actual: usize,
    },
    /// Input has bytes after the declared payload.
    TrailingBytes {
        /// Expected byte count.
        expected: usize,
        /// Actual byte count.
        actual: usize,
    },
    /// `payload_len` does not equal `radix.packed_len(trit_count)`.
    PayloadLengthMismatch {
        /// Expected byte count.
        expected: usize,
        /// Declared/actual byte count.
        actual: usize,
    },
    /// A trit count does not fit in the header's `u32`.
    TritCountOverflow {
        /// The requested count.
        count: usize,
    },
    /// The payload is too large to encode in a `u32` length field.
    PayloadTooLarge {
        /// The requested payload length.
        len: usize,
    },
    /// Stored CRC does not match the computed CRC.
    ChecksumMismatch {
        /// CRC stored in the header.
        expected: u32,
        /// CRC computed over header `0..16` + payload.
        actual: u32,
    },
    /// Packed payload is invalid (bad byte or non-canonical padding).
    PackedPayload(RadixError),
}

impl fmt::Display for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockError::BadMagic { found } => {
                write!(
                    f,
                    "bad block magic: found {found:?}, expected {BLOCK_MAGIC:?}"
                )
            }
            BlockError::BadVersion { found } => {
                write!(
                    f,
                    "bad block version: found {found}, expected {BLOCK_VERSION}"
                )
            }
            BlockError::BadRadix { found } => write!(f, "bad block radix code: {found}"),
            BlockError::BadFlags { found } => {
                write!(f, "bad block flags: found {found}, expected 0")
            }
            BlockError::Truncated { expected, actual } => {
                write!(
                    f,
                    "truncated block: expected {expected} bytes, got {actual}"
                )
            }
            BlockError::TrailingBytes { expected, actual } => {
                write!(f, "trailing block bytes: expected {expected}, got {actual}")
            }
            BlockError::PayloadLengthMismatch { expected, actual } => write!(
                f,
                "payload length mismatch: expected {expected} bytes, got {actual}"
            ),
            BlockError::TritCountOverflow { count } => {
                write!(f, "trit count {count} does not fit in u32")
            }
            BlockError::PayloadTooLarge { len } => {
                write!(f, "payload length {len} does not fit in u32")
            }
            BlockError::ChecksumMismatch { expected, actual } => write!(
                f,
                "block checksum mismatch: stored {expected:#010x}, computed {actual:#010x}"
            ),
            BlockError::PackedPayload(error) => write!(f, "packed payload: {error}"),
        }
    }
}

impl Error for BlockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            BlockError::PackedPayload(error) => Some(error),
            _ => None,
        }
    }
}

/// An owned TRBL block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    bytes: Vec<u8>,
}

impl Block {
    /// Encodes `trits` into a canonical TRBL block.
    pub fn encode(radix: Radix, trits: &[Trit]) -> Result<Self, BlockError> {
        let trit_count = u32::try_from(trits.len())
            .map_err(|_| BlockError::TritCountOverflow { count: trits.len() })?;
        let payload = pack(radix, trits).map_err(BlockError::PackedPayload)?;
        Self::from_payload(radix, trit_count, &payload)
    }

    /// Alias for [`Block::encode`].
    pub fn from_trits(radix: Radix, trits: &[Trit]) -> Result<Self, BlockError> {
        Self::encode(radix, trits)
    }

    /// Builds a block from an already packed payload.
    ///
    /// The payload is validated for exact length and canonical padding.
    pub fn from_payload(radix: Radix, trit_count: u32, payload: &[u8]) -> Result<Self, BlockError> {
        let expected = radix.packed_len(trit_count as usize);
        if payload.len() != expected {
            return Err(BlockError::PayloadLengthMismatch {
                expected,
                actual: payload.len(),
            });
        }
        radix
            .validate_packed(payload, trit_count as usize)
            .map_err(BlockError::PackedPayload)?;

        let payload_len = u32::try_from(payload.len())
            .map_err(|_| BlockError::PayloadTooLarge { len: payload.len() })?;
        let total = BLOCK_HEADER_SIZE + payload.len();
        let mut bytes = vec![0u8; total];
        bytes[0..4].copy_from_slice(&BLOCK_MAGIC);
        bytes[4] = BLOCK_VERSION;
        bytes[5] = radix.code();
        bytes[6..8].copy_from_slice(&0u16.to_le_bytes());
        bytes[8..12].copy_from_slice(&trit_count.to_le_bytes());
        bytes[12..16].copy_from_slice(&payload_len.to_le_bytes());
        bytes[BLOCK_HEADER_SIZE..].copy_from_slice(payload);

        let crc = crc32_parts(&bytes[0..16], &bytes[BLOCK_HEADER_SIZE..]);
        bytes[16..20].copy_from_slice(&crc.to_le_bytes());

        Ok(Self { bytes })
    }

    /// Borrows the full block bytes (header + payload).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consumes the block and returns its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Total block length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Returns `true` for an empty payload block.
    pub fn is_empty(&self) -> bool {
        self.bytes.len() == BLOCK_HEADER_SIZE
    }

    /// Decodes exactly one block, validating header, length, CRC, and padding.
    pub fn decode(bytes: &[u8]) -> Result<BlockView<'_>, BlockError> {
        if bytes.len() < BLOCK_HEADER_SIZE {
            return Err(BlockError::Truncated {
                expected: BLOCK_HEADER_SIZE,
                actual: bytes.len(),
            });
        }

        let found_magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if found_magic != BLOCK_MAGIC {
            return Err(BlockError::BadMagic { found: found_magic });
        }

        let version = bytes[4];
        if version != BLOCK_VERSION {
            return Err(BlockError::BadVersion { found: version });
        }

        let radix = Radix::from_code(bytes[5]).ok_or(BlockError::BadRadix { found: bytes[5] })?;
        let flags = u16::from_le_bytes([bytes[6], bytes[7]]);
        if flags != 0 {
            return Err(BlockError::BadFlags { found: flags });
        }

        let trit_count = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let payload_len = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
        let expected_payload = radix.packed_len(trit_count as usize);
        if payload_len != expected_payload {
            return Err(BlockError::PayloadLengthMismatch {
                expected: expected_payload,
                actual: payload_len,
            });
        }

        let expected_total = BLOCK_HEADER_SIZE + payload_len;
        if bytes.len() < expected_total {
            return Err(BlockError::Truncated {
                expected: expected_total,
                actual: bytes.len(),
            });
        }
        if bytes.len() > expected_total {
            return Err(BlockError::TrailingBytes {
                expected: expected_total,
                actual: bytes.len(),
            });
        }

        let payload = &bytes[BLOCK_HEADER_SIZE..expected_total];
        let stored_crc = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        let computed_crc = crc32_parts(&bytes[0..16], payload);
        if stored_crc != computed_crc {
            return Err(BlockError::ChecksumMismatch {
                expected: stored_crc,
                actual: computed_crc,
            });
        }

        radix
            .validate_packed(payload, trit_count as usize)
            .map_err(BlockError::PackedPayload)?;

        Ok(BlockView {
            bytes,
            radix,
            trit_count,
            payload_len,
        })
    }
}

/// A validated view over one TRBL block.
#[derive(Clone, Copy, Debug)]
pub struct BlockView<'a> {
    bytes: &'a [u8],
    radix: Radix,
    trit_count: u32,
    payload_len: usize,
}

impl<'a> BlockView<'a> {
    /// Packing radix.
    pub fn radix(&self) -> Radix {
        self.radix
    }

    /// Number of trits in the block.
    pub fn trit_count(&self) -> u32 {
        self.trit_count
    }

    /// Packed payload length in bytes.
    pub fn payload_len(&self) -> usize {
        self.payload_len
    }

    /// Stored CRC32.
    pub fn crc32(&self) -> u32 {
        u32::from_le_bytes([
            self.bytes[16],
            self.bytes[17],
            self.bytes[18],
            self.bytes[19],
        ])
    }

    /// Borrowed payload.
    pub fn payload(&self) -> &'a [u8] {
        &self.bytes[BLOCK_HEADER_SIZE..BLOCK_HEADER_SIZE + self.payload_len]
    }

    /// Full block bytes.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Decodes the payload into trits.
    pub fn trits(&self) -> Result<Vec<Trit>, RadixError> {
        let mut out = Vec::with_capacity(self.trit_count as usize);
        self.trits_into(&mut out)?;
        Ok(out)
    }

    /// Decodes the payload into an existing buffer, clearing it first.
    pub fn trits_into(&self, out: &mut Vec<Trit>) -> Result<(), RadixError> {
        unpack_into(self.radix, self.payload(), self.trit_count as usize, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trits(values: &[u8]) -> Vec<Trit> {
        values
            .iter()
            .copied()
            .map(|value| Trit::from_u8(value).expect("valid trit"))
            .collect()
    }

    fn fix_crc(bytes: &mut [u8]) {
        let crc = crc32_parts(&bytes[0..16], &bytes[BLOCK_HEADER_SIZE..]);
        bytes[16..20].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn header_layout_and_roundtrip() {
        let input = trits(&[0, 1, 2]);
        let block = Block::encode(Radix::R3, &input).expect("encode");
        assert_eq!(block.len(), BLOCK_HEADER_SIZE + 1);

        let bytes = block.as_bytes();
        assert_eq!(&bytes[0..4], b"TRBL");
        assert_eq!(bytes[4], 0);
        assert_eq!(bytes[5], 3);
        assert_eq!(u16::from_le_bytes([bytes[6], bytes[7]]), 0);
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            3
        );
        assert_eq!(
            u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]),
            1
        );
        // 0 + 1*3 + 2*9 = 21.
        assert_eq!(bytes[20], 21);

        let view = Block::decode(bytes).expect("decode");
        assert_eq!(view.radix(), Radix::R3);
        assert_eq!(view.trit_count(), 3);
        assert_eq!(view.payload_len(), 1);
        assert_eq!(view.trits().expect("trits"), input);
    }

    #[test]
    fn empty_block_roundtrip() {
        for radix in Radix::ALL {
            let block = Block::encode(radix, &[]).expect("encode");
            assert_eq!(block.len(), BLOCK_HEADER_SIZE);
            assert!(block.is_empty());
            let view = Block::decode(block.as_bytes()).expect("decode");
            assert_eq!(view.trit_count(), 0);
            assert_eq!(view.payload_len(), 0);
            assert!(view.trits().expect("trits").is_empty());
        }
    }

    #[test]
    fn all_radices_and_lengths_roundtrip() {
        for radix in Radix::ALL {
            for len in 0..=128 {
                let input: Vec<Trit> = (0..len)
                    .map(|index| Trit::from_u8((index % 3) as u8).expect("valid"))
                    .collect();
                let block = Block::encode(radix, &input).expect("encode");
                let view = Block::decode(block.as_bytes()).expect("decode");
                assert_eq!(view.radix(), radix);
                assert_eq!(view.trit_count() as usize, len);
                assert_eq!(
                    view.trits().expect("trits"),
                    input,
                    "radix={radix:?} len={len}"
                );
            }
        }
    }

    #[test]
    fn trits_into_reuses_buffer() {
        let input = trits(&[2, 1, 0, 2, 1]);
        let block = Block::encode(Radix::R3, &input).expect("encode");
        let view = Block::decode(block.as_bytes()).expect("decode");
        let mut out = vec![Trit::T0; 99];
        view.trits_into(&mut out).expect("trits_into");
        assert_eq!(out, input);
    }

    #[test]
    fn rejects_bad_magic() {
        let block = Block::encode(Radix::R3, &trits(&[0])).expect("encode");
        let mut bytes = block.into_bytes();
        bytes[0] = b'X';
        assert_eq!(
            Block::decode(&bytes).expect_err("must reject"),
            BlockError::BadMagic { found: *b"XRBL" }
        );
    }

    #[test]
    fn rejects_bad_version() {
        let block = Block::encode(Radix::R3, &trits(&[0])).expect("encode");
        let mut bytes = block.into_bytes();
        bytes[4] = 1;
        assert_eq!(
            Block::decode(&bytes).expect_err("must reject"),
            BlockError::BadVersion { found: 1 }
        );
    }

    #[test]
    fn rejects_bad_radix() {
        let block = Block::encode(Radix::R3, &trits(&[0])).expect("encode");
        let mut bytes = block.into_bytes();
        bytes[5] = 5;
        assert_eq!(
            Block::decode(&bytes).expect_err("must reject"),
            BlockError::BadRadix { found: 5 }
        );
    }

    #[test]
    fn rejects_bad_flags() {
        let block = Block::encode(Radix::R3, &trits(&[0])).expect("encode");
        let mut bytes = block.into_bytes();
        bytes[6] = 1;
        assert_eq!(
            Block::decode(&bytes).expect_err("must reject"),
            BlockError::BadFlags { found: 1 }
        );
    }

    #[test]
    fn rejects_truncated_header_and_payload() {
        let block = Block::encode(Radix::R3, &trits(&[0])).expect("encode");
        let bytes = block.into_bytes();
        assert_eq!(
            Block::decode(&bytes[..19]).expect_err("must reject"),
            BlockError::Truncated {
                expected: BLOCK_HEADER_SIZE,
                actual: 19
            }
        );
        assert_eq!(
            Block::decode(&bytes[..BLOCK_HEADER_SIZE]).expect_err("must reject"),
            BlockError::Truncated {
                expected: BLOCK_HEADER_SIZE + 1,
                actual: BLOCK_HEADER_SIZE
            }
        );
    }

    #[test]
    fn rejects_trailing_bytes() {
        let block = Block::encode(Radix::R3, &trits(&[0])).expect("encode");
        let mut bytes = block.into_bytes();
        bytes.push(0);
        assert_eq!(
            Block::decode(&bytes).expect_err("must reject"),
            BlockError::TrailingBytes {
                expected: BLOCK_HEADER_SIZE + 1,
                actual: BLOCK_HEADER_SIZE + 2
            }
        );
    }

    #[test]
    fn rejects_payload_length_mismatch() {
        let block = Block::encode(Radix::R3, &trits(&[0, 1, 2])).expect("encode");
        let mut bytes = block.into_bytes();
        bytes[12..16].copy_from_slice(&2u32.to_le_bytes());
        assert_eq!(
            Block::decode(&bytes).expect_err("must reject"),
            BlockError::PayloadLengthMismatch {
                expected: 1,
                actual: 2
            }
        );
    }

    #[test]
    fn rejects_checksum_mismatch() {
        let block = Block::encode(Radix::R3, &trits(&[0, 1, 2])).expect("encode");
        let mut bytes = block.into_bytes();
        bytes[BLOCK_HEADER_SIZE] ^= 0xff;
        let error = Block::decode(&bytes).expect_err("must reject");
        match error {
            BlockError::ChecksumMismatch { expected, actual } => assert_ne!(expected, actual),
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn rejects_non_canonical_padding_after_fixing_crc() {
        let block = Block::encode(Radix::R3, &trits(&[2])).expect("encode");
        let mut bytes = block.into_bytes();
        // n=1 canonical byte is 2; 5 has a non-zero unused slot.
        bytes[BLOCK_HEADER_SIZE] = 5;
        fix_crc(&mut bytes);
        assert_eq!(
            Block::decode(&bytes).expect_err("must reject"),
            BlockError::PackedPayload(RadixError::NonCanonicalPadding {
                radix: Radix::R3,
                byte: 5
            })
        );
    }

    #[test]
    fn rejects_invalid_packed_byte_after_fixing_crc() {
        let block = Block::encode(Radix::R3, &trits(&[0, 0, 0, 0, 0])).expect("encode");
        let mut bytes = block.into_bytes();
        bytes[BLOCK_HEADER_SIZE] = 243;
        fix_crc(&mut bytes);
        assert_eq!(
            Block::decode(&bytes).expect_err("must reject"),
            BlockError::PackedPayload(RadixError::InvalidPackedByte {
                radix: Radix::R3,
                byte: 243
            })
        );
    }

    #[test]
    fn from_payload_rejects_length_and_padding() {
        assert_eq!(
            Block::from_payload(Radix::R3, 3, &[0, 0]).expect_err("must reject"),
            BlockError::PayloadLengthMismatch {
                expected: 1,
                actual: 2
            }
        );
        assert_eq!(
            Block::from_payload(Radix::R3, 1, &[5]).expect_err("must reject"),
            BlockError::PackedPayload(RadixError::NonCanonicalPadding {
                radix: Radix::R3,
                byte: 5
            })
        );
    }
}
