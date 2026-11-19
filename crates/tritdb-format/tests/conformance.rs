//! TRBL block conformance, truncation fuzzing, and page composition.

use tritdb_format::{Block, BlockError, BLOCK_HEADER_SIZE, BLOCK_MAGIC, BLOCK_VERSION};
use tritdb_page::{crc32_parts, Page, PageKind, MAX_PAYLOAD};
use tritdb_radix::{pack, Radix, Trit};
use tritdb_testkit::Rng;

fn trits(values: &[u8]) -> Vec<Trit> {
    values
        .iter()
        .map(|value| Trit::from_u8(*value).expect("trit"))
        .collect()
}

#[test]
fn all_lengths_all_radices_roundtrip() {
    for radix in Radix::ALL {
        for count in [
            0, 1, 2, 3, 4, 5, 7, 8, 16, 17, 63, 64, 65, 255, 256, 257, 4096,
        ] {
            let mut rng = Rng::new(0xB10C_0000 + count as u64 + radix.code() as u64);
            let values: Vec<u8> = (0..count).map(|_| rng.trit()).collect();
            let input = trits(&values);
            let block = Block::from_trits(radix, &input).expect("build block");
            assert_eq!(block.len(), BLOCK_HEADER_SIZE + radix.packed_len(count));
            let view = Block::decode(block.as_bytes()).expect("decode block");
            assert_eq!(view.radix(), radix);
            assert_eq!(view.trit_count(), count as u32);
            assert_eq!(view.payload_len(), radix.packed_len(count));
            assert_eq!(view.trits().expect("trits"), input);
            assert_eq!(
                Block::from_trits(radix, &input)
                    .expect("re-encode")
                    .as_bytes(),
                block.as_bytes()
            );
        }
    }
}

#[test]
fn header_layout_matches_spec() {
    let block = Block::from_trits(Radix::R3, &trits(&[0, 1, 2, 0, 1, 2]))
        .expect("block")
        .into_bytes();
    assert_eq!(&block[0..4], &BLOCK_MAGIC);
    assert_eq!(block[4], BLOCK_VERSION);
    assert_eq!(block[5], 3);
    assert_eq!(&block[6..8], &[0, 0]);
    assert_eq!(&block[8..12], &6u32.to_le_bytes());
    assert_eq!(&block[12..16], &2u32.to_le_bytes());
    let expected_crc = crc32_parts(&block[0..16], &block[BLOCK_HEADER_SIZE..]);
    assert_eq!(&block[16..20], &expected_crc.to_le_bytes());
}

#[test]
fn truncation_at_every_offset_is_rejected() {
    let block = Block::from_trits(
        Radix::R3,
        &trits(&(0..256).map(|value| (value % 3) as u8).collect::<Vec<_>>()),
    )
    .expect("block");
    let raw = block.as_bytes();
    for cut in 0..raw.len() {
        let result = Block::decode(&raw[..cut]);
        assert!(result.is_err(), "truncated at {cut} unexpectedly decoded");
    }
    assert!(Block::decode(raw).is_ok());
}

#[test]
fn single_byte_corruption_at_every_offset_is_rejected() {
    let mut block = Block::from_trits(
        Radix::R4,
        &trits(&(0..128).map(|v| (v % 3) as u8).collect::<Vec<_>>()),
    )
    .expect("block")
    .into_bytes();
    for index in 0..block.len() {
        let original = block[index];
        block[index] ^= 0xff;
        let error = Block::decode(&block).expect_err("corruption must be detected");
        match index {
            0..=15 => assert!(
                matches!(
                    error,
                    BlockError::BadMagic { .. }
                        | BlockError::BadVersion { .. }
                        | BlockError::BadRadix { .. }
                        | BlockError::BadFlags { .. }
                        | BlockError::PayloadLengthMismatch { .. }
                        | BlockError::ChecksumMismatch { .. }
                ),
                "unexpected header error at byte {index}: {error:?}"
            ),
            _ => assert!(
                matches!(error, BlockError::ChecksumMismatch { .. }),
                "unexpected payload/CRC error at byte {index}: {error:?}"
            ),
        }
        block[index] = original;
    }
}

#[test]
fn from_payload_rejects_non_canonical_and_mismatched_data() {
    let payload = pack(Radix::R3, &trits(&[1, 2, 0])).expect("pack");
    let block = Block::from_payload(Radix::R3, 3, &payload).expect("canonical payload");
    assert_eq!(
        Block::decode(block.as_bytes())
            .expect("decode")
            .trit_count(),
        3
    );

    let mut bad = payload.clone();
    bad[0] += 3u8.pow(3);
    let error = Block::from_payload(Radix::R3, 3, &bad).expect_err("non-canonical");
    assert!(matches!(error, BlockError::PackedPayload(_)));

    let error = Block::from_payload(Radix::R3, 6, &payload[..1]).expect_err("short payload");
    assert!(matches!(
        error,
        BlockError::PayloadLengthMismatch {
            expected: 2,
            actual: 1
        }
    ));

    let error = Block::from_payload(Radix::R3, 11, &payload[..1]).expect_err("wrong count");
    assert!(matches!(
        error,
        BlockError::PayloadLengthMismatch {
            expected: 3,
            actual: 1
        }
    ));
}

#[test]
fn trailing_bytes_are_rejected() {
    let block = Block::from_trits(Radix::R16, &trits(&[0, 1, 2]))
        .expect("block")
        .into_bytes();
    let mut extended = block.clone();
    extended.push(0);
    let error = Block::decode(&extended).expect_err("trailing byte");
    assert!(matches!(error, BlockError::TrailingBytes { .. }));
}

#[test]
fn empty_block_roundtrips() {
    for radix in Radix::ALL {
        let block = Block::from_trits(radix, &[]).expect("empty block");
        assert!(block.is_empty());
        assert_eq!(block.len(), BLOCK_HEADER_SIZE);
        let view = Block::decode(block.as_bytes()).expect("decode empty");
        assert_eq!(view.trit_count(), 0);
        assert_eq!(view.payload_len(), 0);
        assert!(view.trits().expect("empty trits").is_empty());
    }
}

#[test]
fn block_composes_into_a_full_page() {
    // 40,780 trits at r3 => 8,156-byte payload => 8,176-byte block, exactly
    // the largest page payload.
    let count = (MAX_PAYLOAD - BLOCK_HEADER_SIZE) * 5;
    let mut rng = Rng::new(0xF00D);
    let input = trits(&(0..count).map(|_| rng.trit()).collect::<Vec<_>>());
    let block = Block::from_trits(Radix::R3, &input).expect("max block");
    assert_eq!(block.len(), MAX_PAYLOAD);
    let page = Page::new(PageKind::Data, block.as_bytes()).expect("max page");
    let view = Page::parse(page.as_bytes()).expect("parse page");
    assert_eq!(view.payload_len(), MAX_PAYLOAD);
    let decoded = Block::decode(view.payload()).expect("block inside page");
    assert_eq!(decoded.trits().expect("trits"), input);
}

#[test]
fn block_inside_page_roundtrip_is_stable() {
    let mut rng = Rng::new(0xCAFE);
    let input = trits(&(0..1000).map(|_| rng.trit()).collect::<Vec<_>>());
    let block = Block::from_trits(Radix::R16, &input).expect("block");
    let page = Page::new(PageKind::Index, block.as_bytes()).expect("page");
    let parsed = Page::parse(page.as_bytes()).expect("page parse");
    let block_view = Block::decode(parsed.payload()).expect("block parse");
    assert_eq!(block_view.trits().expect("trits"), input);
}

#[test]
fn crc_is_checked_before_padding() {
    let block = Block::from_trits(Radix::R3, &trits(&[0, 1, 2]))
        .expect("block")
        .into_bytes();
    let mut broken = block.clone();
    broken[BLOCK_HEADER_SIZE] ^= 0x80;
    let error = Block::decode(&broken).expect_err("CRC must fail");
    assert!(matches!(error, BlockError::ChecksumMismatch { .. }));
}

#[test]
fn block_debug_reports_bytes() {
    let block = Block::from_trits(Radix::R4, &trits(&[0, 1, 2])).expect("block");
    assert!(format!("{block:?}").contains("Block"));
}
