#!/usr/bin/env python3
"""Read-only source, pin, toolchain, and disk guard shared by r342 drivers."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tomllib


INTEGRATION = Path("/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration")
PRODUCT = INTEGRATION / "lingxi-app"
HARNESS = INTEGRATION / "harness-runtime"
SDK = INTEGRATION / "llm-client"
MOBILE = Path("/Users/luolingfeng/lingxi/mobile-linux-runtime")
TARGET = Path("/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/target")
BUN = Path("/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/bun-1.4.3/bun")
CARGO = Path("/Users/luolingfeng/.rustup/toolchains/1.94.0-aarch64-apple-darwin/bin/cargo")
PYTHON = Path("/Users/luolingfeng/.cache/codex-runtimes/codex-primary-runtime/dependencies/python/bin/python3.12")
MIN_FREE_BYTES = 3 * 1024**3
HISTORICAL_H_FILE = Path("local_app.rs")
HISTORICAL_H_FILE_SHA256 = "f60db6dd49e1da64f8e24fc1671c1c228f9eceea26ba4682a24750007c98bd19"
URLS = {
    "harness": "https://github.com/lingxi-coder/harness-runtime.git",
    "sdk": "https://github.com/lingxi-coder/llm-client.git",
    "mobile": "https://github.com/lingxi-coder/mobile-linux-runtime.git",
}


class GuardError(RuntimeError):
    pass


def run(*args: str, cwd: Path) -> str:
    try:
        result = subprocess.run(
            args, cwd=cwd, check=True, capture_output=True, text=True, encoding="utf-8"
        )
    except (OSError, subprocess.CalledProcessError) as error:
        raise GuardError(f"command failed: {args[0]} ({cwd})") from error
    return result.stdout.strip()


def require_sha(value: str, label: str) -> str:
    if not re.fullmatch(r"[0-9a-f]{40}", value):
        raise GuardError(f"{label} must be a full lowercase 40-character SHA")
    return value


def origin_slug(path: Path) -> str:
    remote = run("git", "remote", "get-url", "origin", cwd=path)
    match = re.search(r"github\.com(?:-[^/:]+)?[:/]([^/]+/[^/]+?)(?:\.git)?$", remote)
    if match is None:
        raise GuardError(f"origin is not a GitHub repository: {path}")
    return match.group(1)


def verify_repo(
    path: Path,
    expected: str,
    slug: str,
    *,
    source_clean: bool,
    allow_historical_local_app: bool = False,
) -> None:
    if not path.is_dir() or path.is_symlink():
        raise GuardError(f"checkout is missing or symlinked: {path}")
    if run("git", "rev-parse", "HEAD", cwd=path) != expected:
        raise GuardError(f"checkout HEAD differs from required {slug} SHA")
    if origin_slug(path) != slug:
        raise GuardError(f"checkout origin identity differs from {slug}")
    if source_clean:
        status = run(
            "git", "status", "--porcelain=v1", "--untracked-files=all", "--", ".",
            ":(exclude)acceptance/**",
            cwd=path,
        )
        if allow_historical_local_app and status == "?? local_app.rs":
            historical = path / HISTORICAL_H_FILE
            if historical.is_symlink() or not historical.is_file():
                raise GuardError("historical H local_app.rs is missing, symlinked, or not a regular file")
            digest = hashlib.sha256(historical.read_bytes()).hexdigest()
            if digest != HISTORICAL_H_FILE_SHA256:
                raise GuardError("historical H local_app.rs content changed")
            status = ""
        if status:
            raise GuardError(f"build source paths are dirty in {slug}:\n{status}")


def workspace_pin_rows(product_manifest: dict, url: str, expected: str, label: str) -> int:
    rows = [
        dependency
        for dependency in product_manifest.get("workspace", {}).get("dependencies", {}).values()
        if isinstance(dependency, dict) and dependency.get("git") == url
    ]
    if not rows or any(row.get("rev") != expected for row in rows):
        raise GuardError(f"Product direct {label} pins do not all match the requested SHA")
    return len(rows)


def verify_manifest_and_lock(shas: dict[str, str]) -> dict[str, int]:
    try:
        manifest = tomllib.loads((PRODUCT / "Cargo.toml").read_text(encoding="utf-8"))
        lock = tomllib.loads((PRODUCT / "Cargo.lock").read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise GuardError("Product Cargo manifest or lockfile is unreadable") from error

    counts: dict[str, int] = {}
    expected_counts = {"harness": 39, "sdk": 1, "mobile": 3}
    for label, url in URLS.items():
        counts[f"{label}_manifest"] = workspace_pin_rows(
            manifest, url, shas[label], label
        )
        if counts[f"{label}_manifest"] != expected_counts[label]:
            raise GuardError(
                f"Product direct {label} dependency row count is "
                f"{counts[f'{label}_manifest']}, expected {expected_counts[label]}"
            )

        prefix = f"git+{url}?"
        expected_source = f"git+{url}?rev={shas[label]}#{shas[label]}"
        rows = [
            package.get("source", "")
            for package in lock.get("package", [])
            if package.get("source", "").startswith(prefix)
        ]
        if not rows or any(row != expected_source for row in rows):
            raise GuardError(f"Cargo.lock has stale or noncanonical {label} source rows")
        counts[f"{label}_lock"] = len(rows)
    return counts


def verify_toolchain() -> dict[str, str]:
    if not PYTHON.is_file() or not os.access(PYTHON, os.X_OK):
        raise GuardError(f"pinned Python 3.12 runtime is unavailable: {PYTHON}")
    if not BUN.is_file() or not os.access(BUN, os.X_OK):
        raise GuardError(f"pinned Bun runtime is unavailable: {BUN}")
    if not CARGO.is_file() or not os.access(CARGO, os.X_OK):
        raise GuardError(f"pinned Cargo is unavailable: {CARGO}")
    bun_version = run(str(BUN), "--version", cwd=PRODUCT)
    cargo_version = run(str(CARGO), "--version", cwd=PRODUCT)
    rustc_version = run("rustc", "--version", cwd=PRODUCT)
    rustdoc_version = run("rustdoc", "--version", cwd=PRODUCT)
    if bun_version != "1.4.3":
        raise GuardError(f"expected Bun 1.4.3, found {bun_version}")
    if not all("1.94.0" in version for version in (cargo_version, rustc_version, rustdoc_version)):
        raise GuardError("Cargo, rustc, and rustdoc must all be version 1.94.0")
    return {
        "python": "3.12 runtime present",
        "bun": bun_version,
        "cargo": cargo_version,
        "rustc": rustc_version,
        "rustdoc": rustdoc_version,
    }


def verify_space() -> int:
    if TARGET.is_symlink() or TARGET.resolve(strict=False) != TARGET.absolute():
        raise GuardError("shared Cargo target directory is missing or resolves through a symlink")
    free = shutil.disk_usage(TARGET).free
    if free < MIN_FREE_BYTES:
        raise GuardError(f"disk guard: {free} free bytes; need at least {MIN_FREE_BYTES}")
    return free


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--product-sha", required=True)
    parser.add_argument("--harness-sha", required=True)
    parser.add_argument("--sdk-sha", required=True)
    parser.add_argument("--mobile-sha", required=True)
    parser.add_argument("--space", action="store_true", help="also enforce the 3 GiB free-space floor")
    args = parser.parse_args()

    shas = {
        "product": require_sha(args.product_sha, "Product SHA"),
        "harness": require_sha(args.harness_sha, "Harness SHA"),
        "sdk": require_sha(args.sdk_sha, "SDK SHA"),
        "mobile": require_sha(args.mobile_sha, "Mobile SHA"),
    }
    verify_repo(PRODUCT, shas["product"], "lingxi-coder/lingxi-app", source_clean=True)
    verify_repo(
        HARNESS,
        shas["harness"],
        "lingxi-coder/harness-runtime",
        source_clean=True,
        allow_historical_local_app=True,
    )
    verify_repo(SDK, shas["sdk"], "lingxi-coder/llm-client", source_clean=True)
    if not MOBILE.is_dir() or MOBILE.is_symlink():
        raise GuardError(f"Mobile checkout is missing or symlinked: {MOBILE}")
    if origin_slug(MOBILE) != "lingxi-coder/mobile-linux-runtime":
        raise GuardError("Mobile checkout origin identity differs from the canonical repository")
    run("git", "cat-file", "-e", f"{shas['mobile']}^{{commit}}", cwd=MOBILE)
    pins = verify_manifest_and_lock(shas)
    tools = verify_toolchain()
    free = verify_space() if args.space else None
    print(json.dumps({"status": "guarded", "pins": pins, "tools": tools, "free_bytes": free}, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except GuardError as error:
        print(f"r342 source guard: {error}", file=sys.stderr)
        raise SystemExit(2)
