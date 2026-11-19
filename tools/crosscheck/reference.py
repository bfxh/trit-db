#!/usr/bin/env python3
"""Independent Python reference for tritdb v0 on-disk formats.

This file deliberately does not import the Rust crate and does not call the
Rust implementation to obtain expected values. It exists to catch symmetric
bugs that a Rust-only roundtrip test cannot see.
"""
from __future__ import annotations

import struct
import zlib
from dataclasses import dataclass
from typing import Iterable

BLOCK_MAGIC = b"TRBL"
BLOCK_VERSION = 0
BLOCK_HEADER_SIZE = 20

PAGE_MAGIC = b"TRPG"
PAGE_VERSION = 1
PAGE_SIZE = 8192
PAGE_HEADER_SIZE = 16
MAX_PAGE_PAYLOAD = PAGE_SIZE - PAGE_HEADER_SIZE

SEGMENT_MAGIC = b"TRWS"
SEGMENT_VERSION = 0
SEGMENT_HEADER_SIZE = 32
RECORD_MAGIC = b"TRWL"
RECORD_VERSION = 0
RECORD_HEADER_SIZE = 32
MAX_RECORD_PAYLOAD = 64 * 1024 * 1024

KIND_BEGIN = 1
KIND_PUT = 2
KIND_DELETE = 3
KIND_COMMIT = 4
KIND_ABORT = 5
KIND_NAMES = {
    KIND_BEGIN: "begin",
    KIND_PUT: "put",
    KIND_DELETE: "delete",
    KIND_COMMIT: "commit",
    KIND_ABORT: "abort",
}
KIND_CODES = {name: code for code, name in KIND_NAMES.items()}

RADIX_CODES = {3: 3, 4: 4, 16: 16}


def crc32(data: bytes) -> int:
    return zlib.crc32(data) & 0xFFFFFFFF


def packed_len(radix: int, trit_count: int) -> int:
    trits_per_byte = {3: 5, 4: 4, 16: 2}[radix]
    return (trit_count + trits_per_byte - 1) // trits_per_byte


def pack(radix: int, trits: Iterable[int]) -> bytes:
    values = list(trits)
    if any(value not in (0, 1, 2) for value in values):
        raise ValueError("trit outside 0..=2")
    out: list[int] = []
    if radix == 3:
        for start in range(0, len(values), 5):
            chunk = values[start : start + 5]
            place = 1
            value = 0
            for trit in chunk:
                value += trit * place
                place *= 3
            out.append(value)
    elif radix == 4:
        for start in range(0, len(values), 4):
            chunk = values[start : start + 4]
            byte = 0
            for index, trit in enumerate(chunk):
                byte |= trit << (2 * index)
            out.append(byte)
    elif radix == 16:
        for start in range(0, len(values), 2):
            chunk = values[start : start + 2]
            byte = 0
            for index, trit in enumerate(chunk):
                byte |= trit << (4 * index)
            out.append(byte)
    else:
        raise ValueError(f"unsupported radix: {radix}")
    return bytes(out)


def unpack(radix: int, payload: bytes, trit_count: int) -> list[int]:
    if len(payload) != packed_len(radix, trit_count):
        raise ValueError(
            f"payload length {len(payload)} != packed_len({radix}, {trit_count})"
        )
    out: list[int] = []
    if radix == 3:
        full, rem = divmod(trit_count, 5)
        for byte in payload[:full]:
            if byte >= 243:
                raise ValueError(f"r3 byte out of range: {byte}")
            value = byte
            for _ in range(5):
                out.append(value % 3)
                value //= 3
        if rem:
            byte = payload[full]
            if byte >= 3**rem:
                raise ValueError(f"r3 non-canonical last byte: {byte}")
            value = byte
            for _ in range(rem):
                out.append(value % 3)
                value //= 3
    elif radix == 4:
        full, rem = divmod(trit_count, 4)
        for byte in payload:
            for slot in range(4):
                out.append((byte >> (2 * slot)) & 0b11)
        if rem:
            last = payload[-1]
            for slot in range(rem, 4):
                if (last >> (2 * slot)) & 0b11:
                    raise ValueError(f"r4 non-canonical padding: {last}")
    elif radix == 16:
        full, rem = divmod(trit_count, 2)
        for byte in payload:
            out.append(byte & 0b1111)
            out.append((byte >> 4) & 0b1111)
        if rem:
            if payload[-1] >> 4:
                raise ValueError(f"r16 non-canonical padding: {payload[-1]}")
    else:
        raise ValueError(f"unsupported radix: {radix}")
    if any(value not in (0, 1, 2) for value in out):
        raise ValueError("payload contains a value outside 0..=2")
    if len(out) != trit_count:
        raise ValueError(f"decoded {len(out)} trits, expected {trit_count}")
    return out


def make_block(radix: int, trits: Iterable[int]) -> bytes:
    values = list(trits)
    payload = pack(radix, values)
    header = bytearray(BLOCK_HEADER_SIZE)
    header[0:4] = BLOCK_MAGIC
    header[4] = BLOCK_VERSION
    header[5] = RADIX_CODES[radix]
    header[6:8] = struct.pack("<H", 0)
    header[8:12] = struct.pack("<I", len(values))
    header[12:16] = struct.pack("<I", len(payload))
    header[16:20] = struct.pack("<I", crc32(bytes(header[0:16]) + payload))
    return bytes(header) + payload


@dataclass
class BlockInfo:
    radix: int
    trit_count: int
    payload: bytes
    trits: list[int]
    crc: int
    raw: bytes


def parse_block(raw: bytes) -> BlockInfo:
    if len(raw) < BLOCK_HEADER_SIZE:
        raise ValueError("short block header")
    header = raw[:BLOCK_HEADER_SIZE]
    payload = raw[BLOCK_HEADER_SIZE:]
    if header[0:4] != BLOCK_MAGIC:
        raise ValueError("bad block magic")
    if header[4] != BLOCK_VERSION:
        raise ValueError(f"bad block version: {header[4]}")
    radix = header[5]
    if radix not in (3, 4, 16):
        raise ValueError(f"bad block radix: {radix}")
    if struct.unpack("<H", header[6:8])[0] != 0:
        raise ValueError("block flags must be zero")
    trit_count = struct.unpack("<I", header[8:12])[0]
    payload_len = struct.unpack("<I", header[12:16])[0]
    if payload_len != len(payload):
        raise ValueError(f"payload length {payload_len} != actual {len(payload)}")
    expected_payload = packed_len(radix, trit_count)
    if payload_len != expected_payload:
        raise ValueError(f"payload length {payload_len} != expected {expected_payload}")
    expected_crc = crc32(header[0:16] + payload)
    stored_crc = struct.unpack("<I", header[16:20])[0]
    if stored_crc != expected_crc:
        raise ValueError(f"block crc {stored_crc:#x} != {expected_crc:#x}")
    trits = unpack(radix, payload, trit_count)
    return BlockInfo(radix, trit_count, payload, trits, stored_crc, raw)


def make_page(kind: int, payload: bytes) -> bytes:
    if len(payload) > MAX_PAGE_PAYLOAD:
        raise ValueError("page payload too large")
    page = bytearray(PAGE_SIZE)
    page[0:4] = PAGE_MAGIC
    page[4] = PAGE_VERSION
    page[5] = kind
    page[6:8] = struct.pack("<H", 0)
    page[8:12] = struct.pack("<I", len(payload))
    page[PAGE_HEADER_SIZE : PAGE_HEADER_SIZE + len(payload)] = payload
    crc = crc32(bytes(page[0:12]) + payload)
    page[12:16] = struct.pack("<I", crc)
    return bytes(page)


@dataclass
class PageInfo:
    kind: int
    payload: bytes
    crc: int
    raw: bytes


def parse_page(raw: bytes) -> PageInfo:
    if len(raw) != PAGE_SIZE:
        raise ValueError(f"page length {len(raw)} != {PAGE_SIZE}")
    header = raw[:PAGE_HEADER_SIZE]
    if header[0:4] != PAGE_MAGIC:
        raise ValueError("bad page magic")
    if header[4] != PAGE_VERSION:
        raise ValueError(f"bad page version: {header[4]}")
    kind = header[5]
    if kind not in (1, 2, 3):
        raise ValueError(f"bad page kind: {kind}")
    if struct.unpack("<H", header[6:8])[0] != 0:
        raise ValueError("page flags must be zero")
    payload_len = struct.unpack("<I", header[8:12])[0]
    if payload_len > MAX_PAGE_PAYLOAD:
        raise ValueError(f"page payload length too large: {payload_len}")
    payload = raw[PAGE_HEADER_SIZE : PAGE_HEADER_SIZE + payload_len]
    expected_crc = crc32(header[0:12] + payload)
    stored_crc = struct.unpack("<I", header[12:16])[0]
    if stored_crc != expected_crc:
        raise ValueError(f"page crc {stored_crc:#x} != {expected_crc:#x}")
    return PageInfo(kind, payload, stored_crc, raw)


def record_bytes(kind: int, txn_id: int, lsn: int, payload: bytes) -> bytes:
    if len(payload) > MAX_RECORD_PAYLOAD:
        raise ValueError("record payload too large")
    header = bytearray(RECORD_HEADER_SIZE)
    header[0:4] = RECORD_MAGIC
    header[4] = RECORD_VERSION
    header[5] = kind
    header[6:8] = struct.pack("<H", 0)
    header[8:16] = struct.pack("<Q", lsn)
    header[16:24] = struct.pack("<Q", txn_id)
    header[24:28] = struct.pack("<I", len(payload))
    header[28:32] = struct.pack("<I", crc32(bytes(header[0:28]) + payload))
    return bytes(header) + payload


def make_wal(records: Iterable[tuple[int, int, int, bytes]], first_lsn: int) -> bytes:
    """records: (kind, txn_id, lsn, payload). first_lsn is the segment header."""
    header = bytearray(SEGMENT_HEADER_SIZE)
    header[0:4] = SEGMENT_MAGIC
    header[4] = SEGMENT_VERSION
    header[8:16] = struct.pack("<Q", first_lsn)
    header[28:32] = struct.pack("<I", crc32(bytes(header[0:28])))
    out = bytes(header)
    for kind, txn_id, lsn, payload in records:
        out += record_bytes(kind, txn_id, lsn, payload)
    return out


@dataclass
class WalRecord:
    lsn: int
    txn_id: int
    kind: int
    payload: bytes


@dataclass
class WalInfo:
    first_lsn: int
    records: list[WalRecord]
    valid_end: int
    tail_kind: str
    tail_offset: int
    tail_bytes: int


def parse_wal(raw: bytes) -> WalInfo:
    if len(raw) < SEGMENT_HEADER_SIZE:
        raise ValueError("short segment header")
    header = raw[:SEGMENT_HEADER_SIZE]
    if header[0:4] != SEGMENT_MAGIC:
        raise ValueError("bad segment magic")
    if header[4] != SEGMENT_VERSION:
        raise ValueError(f"bad segment version: {header[4]}")
    if any(header[5:8]) or any(header[16:28]):
        raise ValueError("segment reserved bytes must be zero")
    first_lsn = struct.unpack("<Q", header[8:16])[0]
    if first_lsn == 0:
        raise ValueError("segment first_lsn must be non-zero")
    stored_crc = struct.unpack("<I", header[28:32])[0]
    expected_crc = crc32(header[0:28])
    if stored_crc != expected_crc:
        raise ValueError("segment header crc mismatch")

    records: list[WalRecord] = []
    offset = SEGMENT_HEADER_SIZE
    previous_lsn = 0
    tail_kind = "clean"
    tail_offset = offset
    tail_bytes = 0
    while offset < len(raw):
        remaining = len(raw) - offset
        if remaining < RECORD_HEADER_SIZE:
            tail_kind = "torn"
            tail_offset = offset
            tail_bytes = remaining
            break
        rec_header = raw[offset : offset + RECORD_HEADER_SIZE]
        if rec_header[0:4] != RECORD_MAGIC:
            raise ValueError(f"bad record magic at {offset}")
        if rec_header[4] != RECORD_VERSION:
            raise ValueError(f"bad record version at {offset}")
        kind = rec_header[5]
        if kind not in KIND_NAMES:
            raise ValueError(f"bad record kind at {offset}: {kind}")
        if struct.unpack("<H", rec_header[6:8])[0] != 0:
            raise ValueError(f"bad record flags at {offset}")
        lsn = struct.unpack("<Q", rec_header[8:16])[0]
        txn_id = struct.unpack("<Q", rec_header[16:24])[0]
        payload_len = struct.unpack("<I", rec_header[24:28])[0]
        total = RECORD_HEADER_SIZE + payload_len
        if remaining < total:
            tail_kind = "torn"
            tail_offset = offset
            tail_bytes = remaining
            break
        payload = raw[offset + RECORD_HEADER_SIZE : offset + total]
        stored = struct.unpack("<I", rec_header[28:32])[0]
        expected = crc32(rec_header[0:28] + payload)
        if stored != expected:
            raise ValueError(f"record crc mismatch at {offset}")
        if lsn <= previous_lsn:
            raise ValueError(f"lsn not monotonic at {offset}: {lsn} after {previous_lsn}")
        records.append(WalRecord(lsn, txn_id, kind, payload))
        previous_lsn = lsn
        offset += total
    return WalInfo(first_lsn, records, offset, tail_kind, tail_offset, tail_bytes)


def put_payload(key: bytes, value: bytes) -> bytes:
    return struct.pack("<I", len(key)) + key + struct.pack("<I", len(value)) + value


def delete_payload(key: bytes) -> bytes:
    return struct.pack("<I", len(key)) + key


def parse_put_payload(payload: bytes) -> tuple[bytes, bytes]:
    if len(payload) < 4:
        raise ValueError("short put key length")
    key_len = struct.unpack("<I", payload[0:4])[0]
    if 4 + key_len + 4 > len(payload):
        raise ValueError("short put key")
    key = payload[4 : 4 + key_len]
    value_len = struct.unpack("<I", payload[4 + key_len : 8 + key_len])[0]
    if 8 + key_len + value_len != len(payload):
        raise ValueError("put value length mismatch")
    value = payload[8 + key_len :]
    return key, value


def page_id_payload(page_id: int, page: bytes) -> bytes:
    if len(page) != PAGE_SIZE:
        raise ValueError("page mirror must be exactly one page")
    return struct.pack("<Q", page_id) + page


SNAPSHOT_MAGIC = b"TRSN"
SNAPSHOT_VERSION = 0
SNAPSHOT_HEADER_SIZE = 32

CHECKPOINT_MAGIC = b"TRCK"
CHECKPOINT_VERSION = 0
CHECKPOINT_SIZE = 32


def make_snapshot(checkpoint_lsn: int, entries: dict[bytes, bytes]) -> bytes:
    payload = bytearray()
    for key, value in sorted(entries.items()):
        payload += struct.pack("<I", len(key)) + key
        payload += struct.pack("<I", len(value)) + value
    header = bytearray(SNAPSHOT_HEADER_SIZE)
    header[0:4] = SNAPSHOT_MAGIC
    header[4] = SNAPSHOT_VERSION
    header[8:16] = struct.pack("<Q", checkpoint_lsn)
    header[16:24] = struct.pack("<Q", len(entries))
    header[24:28] = struct.pack("<I", len(payload))
    header[28:32] = struct.pack("<I", crc32(bytes(header[0:28]) + bytes(payload)))
    return bytes(header) + bytes(payload)


@dataclass
class SnapshotInfo:
    checkpoint_lsn: int
    entries: dict[bytes, bytes]
    payload_len: int
    crc: int
    raw: bytes


def parse_snapshot(raw: bytes) -> SnapshotInfo:
    if len(raw) < SNAPSHOT_HEADER_SIZE:
        raise ValueError("short snapshot header")
    header = raw[:SNAPSHOT_HEADER_SIZE]
    if header[0:4] != SNAPSHOT_MAGIC:
        raise ValueError("bad snapshot magic")
    if header[4] != SNAPSHOT_VERSION:
        raise ValueError(f"bad snapshot version: {header[4]}")
    if any(header[5:8]):
        raise ValueError("snapshot reserved bytes must be zero")
    checkpoint_lsn = struct.unpack("<Q", header[8:16])[0]
    entry_count = struct.unpack("<Q", header[16:24])[0]
    payload_len = struct.unpack("<I", header[24:28])[0]
    payload = raw[SNAPSHOT_HEADER_SIZE:]
    if payload_len != len(payload):
        raise ValueError(f"snapshot payload length {payload_len} != actual {len(payload)}")
    expected_crc = crc32(header[0:28] + payload)
    stored_crc = struct.unpack("<I", header[28:32])[0]
    if stored_crc != expected_crc:
        raise ValueError(f"snapshot crc {stored_crc:#x} != {expected_crc:#x}")

    entries: dict[bytes, bytes] = {}
    offset = 0
    for _ in range(entry_count):
        key_len = struct.unpack("<I", payload[offset : offset + 4])[0]
        offset += 4
        key = payload[offset : offset + key_len]
        offset += key_len
        value_len = struct.unpack("<I", payload[offset : offset + 4])[0]
        offset += 4
        value = payload[offset : offset + value_len]
        offset += value_len
        if key in entries:
            raise ValueError(f"duplicate snapshot key: {key!r}")
        entries[key] = value
    if offset != len(payload):
        raise ValueError(f"trailing snapshot bytes: {len(payload) - offset}")
    return SnapshotInfo(checkpoint_lsn, entries, payload_len, stored_crc, raw)


def make_checkpoint(checkpoint_lsn: int, page_count: int) -> bytes:
    header = bytearray(CHECKPOINT_SIZE)
    header[0:4] = CHECKPOINT_MAGIC
    header[4] = CHECKPOINT_VERSION
    header[8:16] = struct.pack("<Q", checkpoint_lsn)
    header[16:24] = struct.pack("<Q", page_count)
    header[28:32] = struct.pack("<I", crc32(bytes(header[0:28])))
    return bytes(header)


@dataclass
class CheckpointInfo:
    checkpoint_lsn: int
    page_count: int
    crc: int
    raw: bytes


def parse_checkpoint(raw: bytes) -> CheckpointInfo:
    if len(raw) != CHECKPOINT_SIZE:
        raise ValueError(f"checkpoint length {len(raw)} != {CHECKPOINT_SIZE}")
    if raw[0:4] != CHECKPOINT_MAGIC:
        raise ValueError("bad checkpoint magic")
    if raw[4] != CHECKPOINT_VERSION:
        raise ValueError(f"bad checkpoint version: {raw[4]}")
    if any(raw[5:8]) or any(raw[24:28]):
        raise ValueError("checkpoint reserved bytes must be zero")
    expected_crc = crc32(raw[0:28])
    stored_crc = struct.unpack("<I", raw[28:32])[0]
    if stored_crc != expected_crc:
        raise ValueError(f"checkpoint crc {stored_crc:#x} != {expected_crc:#x}")
    return CheckpointInfo(
        struct.unpack("<Q", raw[8:16])[0],
        struct.unpack("<Q", raw[16:24])[0],
        stored_crc,
        raw,
    )


def encode_kv_request(entries: list[tuple[bytes, bytes]]) -> bytes:
    payload = bytearray(struct.pack("<I", len(entries)))
    for key, value in entries:
        payload += struct.pack("<I", len(key)) + key
        payload += struct.pack("<I", len(value)) + value
    return bytes(payload)


def decode_kv_pairs(payload: bytes) -> dict[bytes, bytes]:
    if len(payload) < 4:
        raise ValueError("short kv pair count")
    count = struct.unpack("<I", payload[0:4])[0]
    offset = 4
    entries: dict[bytes, bytes] = {}
    for _ in range(count):
        key_len = struct.unpack("<I", payload[offset : offset + 4])[0]
        offset += 4
        key = payload[offset : offset + key_len]
        offset += key_len
        value_len = struct.unpack("<I", payload[offset : offset + 4])[0]
        offset += 4
        value = payload[offset : offset + value_len]
        offset += value_len
        if key in entries:
            raise ValueError(f"duplicate kv key: {key!r}")
        entries[key] = value
    if offset != len(payload):
        raise ValueError(f"trailing kv bytes: {len(payload) - offset}")
    return entries
