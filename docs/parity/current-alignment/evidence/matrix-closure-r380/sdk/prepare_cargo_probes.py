#!/usr/bin/env python3
"""Prepare temporary public-SDK probe crates for an explicitly selected checkout."""
from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

EXPECTED_SDK_HEAD = "a363c5b3a237d53eea12c3abfb3b98648ef15d3e"
CRATES = ("current-sdk-probe", "foundry-public-transport-probe")


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def verify_package_manifest(package: Path) -> None:
    manifest_path = package / "contentmanifest.json"
    manifest = json.loads(manifest_path.read_text())
    for item in manifest["files"]:
        path = package / item["path"]
        data = path.read_bytes()
        actual = hashlib.sha256(data).hexdigest()
        if len(data) != item["bytes"] or actual != item["sha256"]:
            raise SystemExit(f"delivery package content mismatch: {item['path']}")


def sdk_head(sdk_path: Path) -> str:
    result = subprocess.run(
        ["git", "-C", str(sdk_path), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    )
    return result.stdout.strip()


def rewrite_manifest(source: Path, destination: Path, sdk_path: Path) -> None:
    text = source.read_text()
    old = 'path = "../../../../../llm-client"'
    if text.count(old) != 1:
        raise SystemExit(f"expected exactly one stored local SDK dependency in {source}")
    # JSON string quoting is valid TOML basic-string quoting and handles spaces.
    dependency = f"path = {json.dumps(str(sdk_path))}"
    destination.write_text(text.replace(old, dependency))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--sdk-path",
        required=True,
        type=Path,
        help="explicit path to the llm-client checkout pinned by this evidence package",
    )
    parser.add_argument(
        "--destination",
        type=Path,
        help="new scratch directory to create; default is a fresh system temporary directory",
    )
    args = parser.parse_args()

    package = Path(__file__).resolve().parent
    verify_package_manifest(package)
    sdk_path = args.sdk_path.expanduser().resolve(strict=True)
    if not (sdk_path / "Cargo.toml").is_file():
        raise SystemExit("--sdk-path must point to the llm-client crate root")
    actual_head = sdk_head(sdk_path)
    if actual_head != EXPECTED_SDK_HEAD:
        raise SystemExit(f"llm-client revision mismatch: expected {EXPECTED_SDK_HEAD}, got {actual_head}")

    if args.destination is None:
        destination = Path(tempfile.mkdtemp(prefix="r380-sdk-probes-")).resolve()
    else:
        destination = args.destination.expanduser().resolve()
        if destination.exists():
            raise SystemExit(f"refusing to overwrite existing destination: {destination}")
        destination.mkdir(parents=True)

    for crate in CRATES:
        source = package / crate
        target = destination / crate
        (target / "src").mkdir(parents=True, exist_ok=True)
        (target / "tests").mkdir(parents=True, exist_ok=True)
        shutil.copy2(source / "Cargo.lock", target / "Cargo.lock")
        rewrite_manifest(source / "Cargo.toml", target / "Cargo.toml", sdk_path)
        if crate == "current-sdk-probe":
            shutil.copy2(source / "src/lib.rs", target / "src/lib.rs")
            shutil.copy2(
                source / "tests/current_sdk_statusless_sse.rs",
                target / "tests/current_sdk_statusless_sse.rs",
            )
            shutil.copytree(package / "fixtures", destination / "fixtures")
        else:
            shutil.copy2(
                source / "tests/foundry_public_transport.rs",
                target / "tests/foundry_public_transport.rs",
            )

    prepared = {
        "sdk_head": actual_head,
        "sdk_path": str(sdk_path),
        "original_package_manifests_and_locks_modified": False,
        "copied_lockfiles_byte_identical": all(
            sha256(package / crate / "Cargo.lock") == sha256(destination / crate / "Cargo.lock")
            for crate in CRATES
        ),
        "rewritten_files": [f"{crate}/Cargo.toml" for crate in CRATES],
        "cargo_was_not_run": True,
    }
    (destination / "prepared-probes.json").write_text(
        json.dumps(prepared, indent=2) + "\n"
    )
    # stdout is the sole output so callers can capture this path safely.
    print(destination)


if __name__ == "__main__":
    main()
