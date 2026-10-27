#!/usr/bin/env python3
"""Audit the upstream registry identity of the macOS-only xcap source copy."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import urllib.request


VERSION = "0.5.2"
ARCHIVE_SHA256 = "d8c280bc7ebbb756eb06816efc2b16d63f85d7ef6c10dcc09bb612503cd78964"
ARCHIVE_URL = f"https://static.crates.io/crates/xcap/xcap-{VERSION}.crate"
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024
VENDOR_ROOT = Path(__file__).resolve().parent


def source_archive(destination):
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    cached = sorted((cargo_home / "registry/cache").glob(f"*/xcap-{VERSION}.crate"))
    if cached:
        return cached[0]
    request = urllib.request.Request(ARCHIVE_URL, headers={"User-Agent": "LingXi-CI-xcap-audit/1.0"})
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(MAX_ARCHIVE_BYTES + 1)
    if len(data) > MAX_ARCHIVE_BYTES:
        raise ValueError("xcap registry archive exceeds the expected size limit")
    destination.write_bytes(data)
    return destination


def audit(workspace, archive=None):
    metadata = subprocess.run(
        ["cargo", "metadata", "--locked", "--format-version=1"],
        cwd=workspace, check=True, text=True, capture_output=True,
    )
    packages = [package for package in json.loads(metadata.stdout)["packages"] if package["name"] == "xcap"]
    if len(packages) != 1 or packages[0]["version"] != VERSION:
        raise ValueError(f"expected exactly one pinned xcap {VERSION} package in Cargo metadata")
    package_root = Path(packages[0]["manifest_path"]).resolve().parent
    if package_root != VENDOR_ROOT:
        raise ValueError("Cargo metadata resolved xcap outside the expected vendored source")
    identity = json.loads((package_root / "UPSTREAM.json").read_text(encoding="utf-8"))
    if (identity["name"], identity["version"], identity["artifact_sha256"]) != ("xcap", VERSION, ARCHIVE_SHA256):
        raise ValueError("vendored xcap upstream identity differs from the audit target")

    with tempfile.TemporaryDirectory(prefix="xcap-upstream-audit-") as temporary:
        directory = Path(temporary)
        source = archive if archive is not None else source_archive(directory / f"xcap-{VERSION}.crate")
        if hashlib.sha256(source.read_bytes()).hexdigest() != ARCHIVE_SHA256:
            raise ValueError("xcap source archive checksum differs from the pinned registry package")
        subprocess.run(
            [sys.executable, str(package_root / "verify-upstream.py"), "--archive", str(source)],
            check=True,
        )
        # cargo-audit reads only this lock, so unused Linux dependencies from the
        # upstream cross-platform manifest are not imported into the audit graph.
        lock = directory / "Cargo.lock"
        lock.write_text(
            'version = 4\n\n[[package]]\nname = "xcap"\n'
            f'version = "{VERSION}"\n'
            'source = "registry+https://github.com/rust-lang/crates.io-index"\n'
            f'checksum = "{ARCHIVE_SHA256}"\n',
            encoding="utf-8",
        )
        # cargo-audit prefers the current directory's config over the user's
        # global config. Use an empty ignore list for this upstream-only check.
        config = directory / ".cargo/audit.toml"
        config.parent.mkdir()
        config.write_text("[advisories]\nignore = []\n", encoding="utf-8")
        print(f"Auditing crates.io xcap {VERSION} itself; workspace dependencies are audited separately", flush=True)
        subprocess.run(["cargo", "audit", "--deny", "warnings", "--file", str(lock)], cwd=directory, check=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, default=Path.cwd())
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    try:
        audit(args.workspace.resolve(), args.archive.resolve() if args.archive else None)
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        print(f"xcap upstream audit: command failed with exit code {error.returncode}", file=sys.stderr)
        raise SystemExit(error.returncode if 0 < error.returncode < 126 else 1)
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        parser.exit(1, f"xcap upstream audit: {error}\n")
