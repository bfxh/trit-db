#!/usr/bin/env python3
"""Cross-check Rust CLI output with the Python byte-level reference.

Direction tested here: Rust encoder -> Python decoder. The Rust vector tests
cover Python encoder -> Rust decoder.
"""
from __future__ import annotations

import os
import subprocess
import sys
import tempfile
from pathlib import Path

from byte_stream import Bridge
from reference import (
    KIND_BEGIN,
    KIND_COMMIT,
    KIND_PUT,
    PAGE_SIZE,
    encode_kv_request,
    decode_kv_pairs,
    make_block,
    make_page,
    parse_block,
    parse_checkpoint,
    parse_page,
    parse_put_payload,
    parse_snapshot,
    parse_wal,
)

ROOT = Path(__file__).resolve().parents[2]


def rust_binary() -> Path | None:
    env = os.environ.get("TRITDB_BIN")
    if env:
        path = Path(env)
        if path.exists():
            return path
    suffix = ".exe" if os.name == "nt" else ""
    path = ROOT / "target" / "debug" / f"tritdb{suffix}"
    return path if path.exists() else None


def run(args: list[str], expected: int = 0) -> str:
    binary = rust_binary()
    if binary is not None:
        command = [str(binary), *args]
    else:
        command = ["cargo", "run", "-q", "-p", "tritdb", "--", *args]
    result = subprocess.run(
        command,
        cwd=ROOT,
        text=True,
        encoding="utf-8",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode != expected:
        raise AssertionError(
            f"command failed ({result.returncode}, expected {expected}): {command}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result.stdout + result.stderr


def check_radix() -> None:
    text = run(["radix", "--radix", "r3", "--trits", "4096"])
    assert "roundtrip=ok" in text, text
    assert "bytes=820" in text, text
    text = run(["radix", "--radix", "r16", "--trits", "4096"])
    assert "roundtrip=ok" in text, text
    assert "bytes=2048" in text, text


def check_block(tmp: Path) -> None:
    for radix, name in ((3, "r3"), (4, "r4"), (16, "r16")):
        path = tmp / f"block-{name}.trbl"
        run(
            [
                "block",
                "make",
                "--out",
                str(path),
                "--radix",
                name,
                "--trits",
                "4096",
            ]
        )
        raw = path.read_bytes()
        info = parse_block(raw)
        assert info.radix == radix, info
        assert info.trit_count == 4096, info
        assert make_block(radix, info.trits) == raw
        check = run(["block", "check", "--in", str(path)])
        assert "crc=ok" in check, check
        assert "trits_roundtrip=ok" in check, check


def check_page(tmp: Path) -> None:
    for kind, name in ((1, "data"), (2, "index"), (3, "manifest")):
        path = tmp / f"page-{name}.bin"
        run(["page", "make", "--out", str(path), "--kind", name, "--text", name])
        raw = path.read_bytes()
        assert len(raw) == PAGE_SIZE
        info = parse_page(raw)
        assert info.kind == kind, info
        assert info.payload == name.encode(), info
        assert make_page(kind, info.payload) == raw
        check = run(["page", "check", "--in", str(path)])
        assert "checksum=ok" in check, check


def check_txn(tmp: Path) -> None:
    wal = tmp / "txn.wal"
    put = run(
        [
            "txn",
            "put",
            "--wal",
            str(wal),
            "--key",
            "alpha",
            "--value",
            "one",
        ]
    )
    assert "committed txn=" in put, put
    info = parse_wal(wal.read_bytes())
    assert info.first_lsn == 1, info
    assert info.tail_kind == "clean", info
    kinds = [record.kind for record in info.records]
    assert kinds == [KIND_BEGIN, KIND_PUT, KIND_COMMIT], kinds
    key, value = parse_put_payload(info.records[1].payload)
    assert (key, value) == (b"alpha", b"one")
    assert info.records[2].payload == b""

    get = run(["txn", "get", "--wal", str(wal), "--key", "alpha"])
    assert 'value="one"' in get, get
    dump = run(["txn", "dump", "--wal", str(wal)])
    assert "committed_txns=1" in dump, dump
    checkpoint = run(["txn", "checkpoint", "--wal", str(wal)])
    assert "entries=1" in checkpoint, checkpoint
    assert wal.stat().st_size == 32, wal.stat().st_size
    snapshot_path = tmp / "txn.wal.snap"
    assert snapshot_path.exists()
    snapshot = parse_snapshot(snapshot_path.read_bytes())
    assert snapshot.entries == {b"alpha": b"one"}, snapshot.entries
    assert snapshot.checkpoint_lsn > 0, snapshot.checkpoint_lsn


def check_pstore(tmp: Path) -> None:
    data = tmp / "pstore.pages"
    wal = tmp / "pstore.wal"
    alloc = run(
        [
            "pstore",
            "alloc",
            "--data",
            str(data),
            "--wal",
            str(wal),
            "--kind",
            "data",
            "--text",
            "hello",
        ]
    )
    assert "page=0" in alloc, alloc
    info = parse_wal(wal.read_bytes())
    kinds = [record.kind for record in info.records]
    assert kinds == [KIND_BEGIN, KIND_PUT, KIND_COMMIT], kinds
    page_payload = info.records[1].payload
    assert len(page_payload) == 8 + PAGE_SIZE, len(page_payload)
    page_id = int.from_bytes(page_payload[:8], "little")
    assert page_id == 0
    page = parse_page(page_payload[8:])
    assert page.kind == 1
    assert page.payload == b"hello"

    get = run(
        [
            "pstore",
            "get",
            "--data",
            str(data),
            "--wal",
            str(wal),
            "--id",
            "0",
        ]
    )
    assert 'payload="hello"' in get, get
    dump = run(["pstore", "dump", "--data", str(data), "--wal", str(wal)])
    assert "page_count=1" in dump, dump
    checkpoint = run(
        ["pstore", "checkpoint", "--data", str(data), "--wal", str(wal)]
    )
    assert "page_count=1" in checkpoint, checkpoint
    assert data.stat().st_size == PAGE_SIZE
    assert wal.stat().st_size == 32
    checkpoint_path = tmp / "pstore.pages.ckpt"
    assert checkpoint_path.exists()
    sidecar = parse_checkpoint(checkpoint_path.read_bytes())
    assert sidecar.page_count == 1, sidecar
    assert sidecar.checkpoint_lsn > 0, sidecar


def check_bridge() -> None:
    binary = rust_binary()
    if binary is None:
        raise AssertionError("Rust binary not found; run `cargo build -p tritdb` first")
    with Bridge(binary) as bridge:
        for radix in (3, 4, 16):
            trits = [(index * 7 + radix) % 3 for index in range(1000)]
            request = make_block(radix, trits)
            response = bridge.request("block", request)
            info = parse_block(response)
            assert info.radix == radix
            assert info.trits == trits, "TRBL bridge changed trits"
            assert response == request, "TRBL bridge changed canonical bytes"

        for kind in (1, 2, 3):
            request = make_page(kind, f"bridge-{kind}".encode())
            response = bridge.request("page", request)
            info = parse_page(response)
            assert info.kind == kind
            assert info.payload == f"bridge-{kind}".encode()
            assert response == request, "page bridge changed canonical bytes"

        entries = [
            (b"alpha", b"one"),
            (b"beta", b"\x00\x01\x02"),
            (b"empty-value", b""),
            (b"", b"empty-key"),
        ]
        response = bridge.request("txn", encode_kv_request(entries))
        assert decode_kv_pairs(response) == dict(entries), "txn bridge lost entries"

        request = make_page(2, b"p" * 5000)
        response = bridge.request("pstore", request)
        info = parse_page(response)
        assert info.kind == 2
        assert info.payload == b"p" * 5000, "pstore bridge lost payload"
        assert response == request, "pstore bridge changed canonical page bytes"


def check_errors() -> None:
    text = run(["radix", "--trits", "0"], expected=1)
    assert "must be greater than 0" in text, text
    text = run(["unknown-subcommand"], expected=1)
    assert "unknown subcommand" in text, text


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="tritdb-crosscheck-") as raw_tmp:
        tmp = Path(raw_tmp)
        check_radix()
        check_block(tmp)
        check_page(tmp)
        check_txn(tmp)
        check_pstore(tmp)
        check_bridge()
        check_errors()
    print("Rust CLI output is accepted by the Python reference")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:  # noqa: BLE001 - CLI script should print a clean failure
        print(f"crosscheck failed: {error}", file=sys.stderr)
        raise
