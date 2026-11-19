#!/usr/bin/env python3
"""Binary framed transport between the Python reference and the Rust CLI.

The Rust side reads one frame from stdin and writes one frame to stdout. Using
a binary pipe avoids shell quoting, temporary files and newline assumptions,
which makes this usable in CI on Windows, macOS and Linux.

Frame layout: b"TRX1" | u32 name_len | u32 payload_len | name | payload.
"""
from __future__ import annotations

import os
import subprocess
from pathlib import Path

MAGIC = b"TRX1"
HEADER_SIZE = 12
MAX_NAME = 64
MAX_PAYLOAD = 128 * 1024 * 1024


def _u32(value: int) -> bytes:
    if not 0 <= value <= 0xFFFFFFFF:
        raise ValueError(f"u32 out of range: {value}")
    return value.to_bytes(4, "little")


def frame(name: str, payload: bytes) -> bytes:
    encoded_name = name.encode("utf-8")
    if not 1 <= len(encoded_name) <= MAX_NAME:
        raise ValueError(f"invalid frame name: {name!r}")
    if len(payload) > MAX_PAYLOAD:
        raise ValueError(f"payload too large: {len(payload)}")
    return MAGIC + _u32(len(encoded_name)) + _u32(len(payload)) + encoded_name + payload


def _read_exact(stream, size: int) -> bytes:
    chunks: list[bytes] = []
    remaining = size
    while remaining:
        chunk = stream.read(remaining)
        if not chunk:
            raise ValueError(f"short read: wanted {size}, got {size - remaining}")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def _read_u32(stream) -> int:
    return int.from_bytes(_read_exact(stream, 4), "little")


def read_frame(stream) -> tuple[str, bytes]:
    header = _read_exact(stream, HEADER_SIZE)
    if header[:4] != MAGIC:
        raise ValueError(f"bad frame magic: {header[:4]!r}")
    name_len = int.from_bytes(header[4:8], "little")
    payload_len = int.from_bytes(header[8:12], "little")
    if not 1 <= name_len <= MAX_NAME:
        raise ValueError(f"invalid frame name length: {name_len}")
    if payload_len > MAX_PAYLOAD:
        raise ValueError(f"payload too large: {payload_len}")
    name = _read_exact(stream, name_len).decode("utf-8")
    payload = _read_exact(stream, payload_len)
    return name, payload


class Bridge:
    """A persistent Rust subprocess over stdin/stdout."""

    def __init__(self, executable: Path) -> None:
        self.executable = Path(executable)
        startup = None
        if os.name == "nt":
            startup = subprocess.STARTUPINFO()
            startup.dwFlags |= subprocess.STARTF_USESHOWWINDOW
        self.process = subprocess.Popen(
            [str(self.executable), "crosscheck"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            startupinfo=startup,
            bufsize=0,
        )

    def request(self, name: str, payload: bytes) -> bytes:
        if self.process.stdin is None or self.process.stdout is None:
            raise RuntimeError("bridge pipes unavailable")
        self.process.stdin.write(frame(name, payload))
        self.process.stdin.flush()
        response_name, response_payload = read_frame(self.process.stdout)
        if response_name != name:
            raise ValueError(
                f"response name {response_name!r} does not match request {name!r}"
            )
        return response_payload

    def close(self) -> None:
        if self.process.stdin is not None:
            self.process.stdin.close()
        try:
            code = self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            code = self.process.wait()
        stderr = b""
        if self.process.stderr is not None:
            stderr = self.process.stderr.read()
        if code != 0:
            raise RuntimeError(f"bridge exited with {code}: {stderr.decode(errors='replace')}")

    def __enter__(self) -> "Bridge":
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        if exc_type is None:
            self.close()
        else:
            self.process.kill()
            self.process.wait()
