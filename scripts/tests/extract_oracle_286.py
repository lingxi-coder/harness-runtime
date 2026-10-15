#!/usr/bin/env python3
"""Extract inspectable JS from a supplied, hash-pinned native reference.

No download, execution, credentials or global installation changes. Generated
reference source stays outside the repository; it is not a shipping asset.
"""

import argparse
import hashlib
import json
from pathlib import Path
import re

VERSION = "2.1.286"
BINARY_SHA256 = "75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433"
HEADER = b"// @bun @bytecode\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--check", action="store_true", help="Compare existing chunks without writing")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    data = binary.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if digest != BINARY_SHA256:
        parser.error(f"Unexpected native oracle SHA-256: {digest}")
    offsets = [match.start() for match in re.finditer(re.escape(HEADER), data)]
    if len(offsets) != 2140:
        parser.error(f"Expected 2140 source chunks; found {len(offsets)}")
    output = args.output.resolve()
    if not args.check:
        output.mkdir(parents=True, exist_ok=False)
    names = set()
    for index, start in enumerate(offsets):
        end = offsets[index + 1] if index + 1 < len(offsets) else len(data)
        blob = data[start:end]
        export = blob.rfind(b"export{")
        nul = blob.find(b"\0", max(0, export))
        if nul >= 0:
            blob = blob[:nul]
        name = f"src_{start}.js"
        names.add(name)
        path = output / name
        if args.check:
            if path.read_bytes() != blob:
                raise ValueError(f"Oracle chunk differs: {path}")
        else:
            path.write_bytes(blob)
    if {path.name for path in output.glob("src_*.js")} != names:
        raise ValueError("Oracle source set has unexpected or missing chunks")
    if not args.check:
        (output / "capture.json").write_text(json.dumps({
            "version": VERSION, "binary": str(binary), "binary_bytes": len(data),
            "binary_sha256": digest, "chunk_count": len(offsets),
            "extraction": "Plain-JS chunks split on // @bun @bytecode newline; trim first NUL after final export{ or offset 0",
            "output": str(output),
        }, indent=2) + "\n")
    print(f"Verified {len(offsets)} reference chunks from Claude Code {VERSION} ({digest})")


if __name__ == "__main__":
    main()
