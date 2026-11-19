#!/usr/bin/env python3
"""Generate and verify cross-implementation tritdb vectors.

The vectors are produced by ``reference.py`` from the format specs, not by the
Rust implementation. Rust tests decode them; a CI job also regenerates them and
fails on any diff.
"""
from __future__ import annotations

import argparse
import sys
from pathlib import Path

from reference import (
    KIND_ABORT,
    KIND_BEGIN,
    KIND_COMMIT,
    KIND_DELETE,
    KIND_PUT,
    delete_payload,
    make_block,
    make_page,
    make_snapshot,
    make_wal,
    pack,
    page_id_payload,
    put_payload,
)

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUT = ROOT / "testdata" / "vectors"


class Lcg:
    """A deterministic 32-bit LCG, independent of Python's random module."""

    def __init__(self, seed: int) -> None:
        self.state = seed & 0xFFFFFFFF

    def next_u32(self) -> int:
        self.state = (1664525 * self.state + 1013904223) & 0xFFFFFFFF
        return self.state

    def trit(self) -> int:
        return self.next_u32() % 3

    def byte(self) -> int:
        return (self.next_u32() >> 16) & 0xFF


def trit_string(trits: list[int]) -> str:
    return "".join(str(trit) for trit in trits)


def radix_lines() -> list[str]:
    counts = [
        0,
        1,
        2,
        3,
        4,
        5,
        7,
        8,
        15,
        16,
        17,
        31,
        32,
        33,
        63,
        64,
        65,
        127,
        128,
        129,
        255,
        256,
        257,
        511,
        512,
        513,
        1023,
        1024,
        1025,
        4096,
    ]
    lines = ["# tritdb crosscheck radix v1", "# radix\tname\tcount\ttrits\thex"]
    for index, count in enumerate(counts):
        for radix in (3, 4, 16):
            rng = Lcg(0xC0FFEE00 + index * 17 + radix)
            trits = [rng.trit() for _ in range(count)]
            packed = bytes(
                __import__("reference").pack(radix, trits)
            )
            lines.append(
                f"radix\tr{radix}\t{count}\t{trit_string(trits)}\t{packed.hex()}"
            )
    return lines


def block_lines() -> list[str]:
    counts = [0, 1, 5, 17, 64, 257, 1024, 4096, 8176]
    lines = ["# tritdb crosscheck trbl v1", "# block\tname\tcount\ttrits\thex"]
    for index, count in enumerate(counts):
        for radix in (3, 4, 16):
            rng = Lcg(0xB10C0000 + index * 19 + radix)
            trits = [rng.trit() for _ in range(count)]
            block = make_block(radix, trits)
            lines.append(
                f"block\tr{radix}\t{count}\t{trit_string(trits)}\t{block.hex()}"
            )
    return lines


def page_lines() -> list[str]:
    kinds = [("data", 1), ("index", 2), ("manifest", 3)]
    lengths = [0, 1, 5, 32, 255, 1024, 4095, 8176]
    lines = ["# tritdb crosscheck page v1", "# page\tkind\tlen\tpayload_hex\tpage_hex"]
    for kind_index, (name, code) in enumerate(kinds):
        for length_index, length in enumerate(lengths):
            rng = Lcg(0xDA6E0000 + kind_index * 23 + length_index)
            payload = bytes(rng.byte() for _ in range(length))
            page = make_page(code, payload)
            lines.append(f"page\t{name}\t{length}\t{payload.hex()}\t{page.hex()}")
    return lines


def wal_lines() -> list[str]:
    records = [
        (KIND_BEGIN, 42, 7, b""),
        (KIND_PUT, 42, 8, put_payload(b"alpha", b"one")),
        (KIND_PUT, 42, 9, page_id_payload(9, make_page(1, b"page-zero"))),
        (KIND_DELETE, 42, 10, delete_payload(b"alpha")),
        (KIND_PUT, 42, 11, put_payload(b"beta", b"\x00\x01\x02")),
        (KIND_ABORT, 43, 12, b""),
    ]
    wal = make_wal(records, 7)
    lines = [
        "# tritdb crosscheck wal v1",
        "# wal\tfirst_lsn\thex",
        "# rec\tlsn\ttxn\tkind\tpayload_hex",
        f"wal\t7\t{wal.hex()}",
    ]
    for kind, txn_id, lsn, payload in records:
        name = {
            KIND_BEGIN: "begin",
            KIND_PUT: "put",
            KIND_DELETE: "delete",
            KIND_COMMIT: "commit",
            KIND_ABORT: "abort",
        }[kind]
        lines.append(f"rec\t{lsn}\t{txn_id}\t{name}\t{payload.hex()}")
    return lines


def snapshot_lines() -> list[str]:
    cases: list[tuple[str, int, dict[bytes, bytes]]] = [
        ("empty", 0, {}),
        ("one", 7, {b"alpha": b"one"}),
        ("empty-key-value", 9, {b"": b""}),
        ("binary", 42, {b"\x00\xff": b"\x01\x02\x03"}),
        (
            "many",
            4096,
            {f"key-{index:03d}".encode(): f"value-{index}".encode() for index in range(100)},
        ),
        ("large-value", 17, {b"blob": b"x" * 5000}),
    ]
    lines = ["# tritdb crosscheck snapshot v1", "# snapshot\tname\tlsn\tcount\thex"]
    for name, lsn, entries in cases:
        snapshot = make_snapshot(lsn, entries)
        lines.append(f"snapshot\t{name}\t{lsn}\t{len(entries)}\t{snapshot.hex()}")
    return lines


def build_text() -> dict[str, str]:
    return {
        "radix.txt": "\n".join(radix_lines()) + "\n",
        "trbl.txt": "\n".join(block_lines()) + "\n",
        "page.txt": "\n".join(page_lines()) + "\n",
        "wal.txt": "\n".join(wal_lines()) + "\n",
        "snapshot.txt": "\n".join(snapshot_lines()) + "\n",
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out-dir", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()

    files = build_text()
    args.out_dir.mkdir(parents=True, exist_ok=True)
    failed = False
    for name, text in files.items():
        path = args.out_dir / name
        if args.check:
            if not path.exists():
                print(f"missing vector file: {path}", file=sys.stderr)
                failed = True
                continue
            existing = path.read_text(encoding="utf-8")
            if existing != text:
                print(f"vector drift: {path}", file=sys.stderr)
                failed = True
        else:
            path.write_text(text, encoding="utf-8", newline="\n")
            print(f"wrote {path} ({len(text.splitlines())} lines)")
    if args.check and not failed:
        print("crosscheck vectors are deterministic and up to date")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
