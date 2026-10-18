#!/usr/bin/env python3
"""Verify the pinned Native artifact and extract only the bound source ranges."""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path

NATIVE_SHA256 = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
RANGES = (
    ("parser-source-chunk.js", 190263488, 309067, "ab4dbd9d43b80f965be5dbd2f6a2ba069509b41ead7cfc87b2da1853c30a4abd"),
    ("api-error-source-chunk.js", 186056234, 10345, "a462b175f36ed4ae079da572a7a6da5993bc4c8fd2b53f4dfbbb82e14d3a0b52"),
)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("native_artifact", type=Path, help="path to the pinned Native 2.1.295 artifact")
    parser.add_argument("output_dir", type=Path, help="directory for the two extracted source ranges")
    args = parser.parse_args()

    native = args.native_artifact.read_bytes()
    actual_native_hash = sha256(native)
    if actual_native_hash != NATIVE_SHA256:
        raise SystemExit(f"Native artifact SHA-256 mismatch: {actual_native_hash}")

    args.output_dir.mkdir(parents=True, exist_ok=True)
    for filename, start, length, expected_hash in RANGES:
        data = native[start : start + length]
        if len(data) != length or sha256(data) != expected_hash:
            raise SystemExit(f"source range fingerprint mismatch: {filename}")
        (args.output_dir / filename).write_bytes(data)
        print(f"{filename}: offset={start} bytes={length} sha256={expected_hash}")

    excerpt = Path(__file__).resolve().parent / "extracted-295-probe" / "api-error-source-extracted.js"
    if excerpt.read_bytes() not in (args.output_dir / "api-error-source-chunk.js").read_bytes():
        raise SystemExit("retained APIError excerpt is not contained in the verified source range")


if __name__ == "__main__":
    main()
