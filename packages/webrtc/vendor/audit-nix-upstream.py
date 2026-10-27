#!/usr/bin/env python3
"""Verify the nix iOS patch and audit its crates.io advisory baseline."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import urllib.request


ROOT = Path(__file__).resolve().parents[3]
VENDOR = Path(__file__).resolve().parent / "nix"
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT / "third_party"))
import registry_audit

VERSION = "0.31.3"
CHECKSUM = "cf20d2fde8ff38632c426f1165ed7436270b44f199fc55284c38276f9db47c3d"
REPOSITORY = "https://github.com/lingxi-coder/harness-runtime.git"
MAX_ARCHIVE_BYTES = 2 * 1024 * 1024

# Keep these contexts specific: an unrelated change to any source byte must
# fail verification, including another cfg guard elsewhere in either file.
PATCHES = {
    "src/sys/socket/mod.rs": [
        (b'    #[cfg(apple_targets)]\n    KextControl = libc::SYSPROTO_CONTROL,',
         b'    #[cfg(target_os = "macos")]\n    KextControl = libc::SYSPROTO_CONTROL,'),
    ],
    "src/sys/socket/sockopt.rs": [
        (b'#[cfg(apple_targets)]\n#[cfg(feature = "net")]\nsockopt_impl!(\n    /// Get the utun interface name.',
         b'#[cfg(target_os = "macos")]\n#[cfg(feature = "net")]\nsockopt_impl!(\n    /// Get the utun interface name.'),
        (b'#[cfg(apple_targets)]\n#[cfg(feature = "net")]\nstruct GetCString<T: AsMut<[u8]>>',
         b'#[cfg(target_os = "macos")]\n#[cfg(feature = "net")]\nstruct GetCString<T: AsMut<[u8]>>'),
        (b'#[cfg(apple_targets)]\n#[cfg(feature = "net")]\nimpl<T: AsMut<[u8]>> Get<CString> for GetCString<T>',
         b'#[cfg(target_os = "macos")]\n#[cfg(feature = "net")]\nimpl<T: AsMut<[u8]>> Get<CString> for GetCString<T>'),
    ],
}


def verify_identity(metadata, workspace):
    packages = [package for package in metadata["packages"] if package["name"] == "nix" and package["version"] == VERSION]
    if len(packages) != 1:
        raise ValueError(f"expected exactly one nix {VERSION} package in Cargo metadata")
    package = packages[0]
    if Path(package["manifest_path"]).resolve() != (VENDOR / "Cargo.toml").resolve():
        raise ValueError("Cargo metadata resolved nix outside the expected vendored checkout")
    manifest = tomllib.loads((workspace / "Cargo.toml").read_text(encoding="utf-8"))
    patch = manifest["patch"]["crates-io"]["nix"]
    if package["source"] is None:
        if workspace.resolve() != VENDOR.parent.parent.resolve() or patch != {"path": "vendor/nix"}:
            raise ValueError("nix path identity is not the embedded WebRTC workspace patch")
    else:
        runtime = manifest["workspace"]["dependencies"]["harness-runtime"]
        revision = patch.get("rev", "")
        if (patch.get("git") != REPOSITORY or not re.fullmatch(r"[0-9a-f]{40}", revision)
                or runtime.get("git") != REPOSITORY or runtime.get("rev") != revision):
            raise ValueError("nix patch must use the same pinned Harness Git source as the runtime")
        expected = f"git+{REPOSITORY}?rev={revision}#{revision}"
        if package["source"] != expected:
            raise ValueError("nix Git metadata source differs from the pinned Harness revision")


def archive_path(directory):
    filename = f"nix-{VERSION}.crate"
    cargo_home = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    cached = sorted((cargo_home / "registry/cache").glob(f"*/{filename}"))
    if cached:
        return cached[0]
    request = urllib.request.Request(
        f"https://static.crates.io/crates/nix/{filename}",
        headers={"User-Agent": "upstream-advisory-audit/1.0"},
    )
    with urllib.request.urlopen(request, timeout=60) as response:
        data = response.read(MAX_ARCHIVE_BYTES + 1)
    if len(data) > MAX_ARCHIVE_BYTES:
        raise ValueError("nix registry archive exceeds the expected size limit")
    path = directory / filename
    path.write_bytes(data)
    return path


def verify_source(archive):
    if archive.stat().st_size > MAX_ARCHIVE_BYTES or hashlib.sha256(archive.read_bytes()).hexdigest() != CHECKSUM:
        raise ValueError("nix source archive differs from the pinned crates.io checksum")
    with tarfile.open(archive, "r:gz") as tar:
        prefix = f"nix-{VERSION}/"
        keep = {"Cargo.toml", "LICENSE", "README.md", "build.rs"}
        members = {}
        for member in tar.getmembers():
            if not member.name.startswith(prefix):
                raise ValueError("unexpected nix archive member prefix")
            name = member.name[len(prefix):]
            if name.startswith("src/") or name in keep:
                if member.isdir():
                    continue
                if not member.isfile() or name in members:
                    raise ValueError(f"invalid nix archive member: {name}")
                members[name] = member
        if keep - members.keys() or not PATCHES.keys() <= members.keys():
            raise ValueError("nix archive is missing a required source file")
        actual = set()
        for path in VENDOR.rglob("*"):
            if path.is_symlink() or not (path.is_file() or path.is_dir()):
                raise ValueError(f"unsupported vendored nix entry: {path}")
            if path.is_file():
                actual.add(path.relative_to(VENDOR).as_posix())
        if actual != members.keys():
            raise ValueError(f"vendored nix file inventory differs: {sorted(actual ^ members.keys())}")
        for name, member in members.items():
            expected = tar.extractfile(member).read()
            for old, new in PATCHES.get(name, []):
                if expected.count(old) != 1:
                    raise ValueError(f"unexpected upstream nix patch context: {name}")
                expected = expected.replace(old, new, 1)
            if (VENDOR / name).read_bytes() != expected:
                raise ValueError(f"vendored nix differs from the reviewed upstream patch: {name}")
    print(f"Verified {len(members)} nix files and four reviewed macOS cfg guards", flush=True)


def audit(workspace, archive=None):
    metadata = subprocess.run(
        ["cargo", "metadata", "--locked", "--all-features", "--format-version=1"],
        cwd=workspace, check=True, text=True, capture_output=True,
    )
    verify_identity(json.loads(metadata.stdout), workspace)
    with tempfile.TemporaryDirectory(prefix="nix-upstream-audit-") as temporary:
        directory = Path(temporary)
        verify_source(archive if archive is not None else archive_path(directory))
        print(f"Auditing crates.io nix {VERSION}; actual workspace dependencies are audited separately", flush=True)
        registry_audit.audit([("nix", VERSION, CHECKSUM)], directory)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", type=Path, default=Path.cwd())
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    try:
        audit(args.workspace.resolve(), args.archive.resolve() if args.archive else None)
    except subprocess.CalledProcessError as error:
        if error.stderr:
            print(error.stderr, file=sys.stderr)
        print(f"nix upstream audit: command failed with exit code {error.returncode}", file=sys.stderr)
        return error.returncode if 0 < error.returncode < 126 else 1
    except (OSError, ValueError, KeyError, json.JSONDecodeError, tarfile.TarError) as error:
        parser.exit(1, f"nix upstream audit: {error}\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
