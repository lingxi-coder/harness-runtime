#!/usr/bin/env python3
"""Re-encode verified immutable Syntect 5.3.0 assets for the runtime's CBOR codec.

The retired decoder is used only in this temporary conversion tool, never in the
runtime dependency graph. Input is the checksum-pinned upstream crate archive.
"""

from pathlib import Path
import hashlib
import io
import subprocess
import tarfile
import tempfile
import urllib.request

UPSTREAM_SHA256 = "656b45c05d95a5704399aeef6bd0ddec7b2b3531b7c9e900abbf7c4d2190c925"


def main() -> None:
    root = Path(__file__).resolve().parents[2]
    with urllib.request.urlopen(
        "https://static.crates.io/crates/syntect/syntect-5.3.0.crate", timeout=60
    ) as response:
        archive = response.read()
    if hashlib.sha256(archive).hexdigest() != UPSTREAM_SHA256:
        raise ValueError("upstream Syntect archive checksum differs from the pinned source")
    with tempfile.TemporaryDirectory(prefix="harness-syntax-export-") as temporary:
        task = Path(temporary)
        with tarfile.open(fileobj=io.BytesIO(archive)) as source:
            source.extractall(task, filter="data")
        upstream = task / "syntect-5.3.0"
        source_file = upstream / "src/parsing/syntax_set.rs"
        source_file.write_text(source_file.read_text() + (root / "scripts/syntect/export_assets.rs").read_text())
        manifest = upstream / "Cargo.toml"
        manifest.write_text(manifest.read_text() + '\n[dependencies.ciborium]\nversion = "=0.2.2"\n')
        (task / "src").mkdir()
        (task / "src/main.rs").write_text(
            'fn main() { syntect::parsing::export_cbor_assets(std::path::Path::new(&std::env::args().nth(1).unwrap())); }\n'
        )
        (task / "Cargo.toml").write_text(
            '[package]\nname = "harness-syntect-export"\nversion = "0.1.0"\nedition = "2021"\n[workspace]\n'
            '[dependencies]\nsyntect = { path = "syntect-5.3.0", default-features = false, '
            'features = ["parsing", "regex-fancy", "default-syntaxes", "default-themes", "metadata"] }\n'
        )
        subprocess.run(["cargo", "generate-lockfile", "--manifest-path", str(task / "Cargo.toml")], check=True)
        subprocess.run([
            "cargo", "run", "--locked", "--manifest-path", str(task / "Cargo.toml"), "--",
            str(root / "third_party/syntect/assets"),
        ], check=True)


if __name__ == "__main__":
    main()
