#!/usr/bin/env python3
"""Verify the scoped vendored xcap files against their recorded provenance."""

import argparse
import hashlib
import json
from pathlib import Path
import tarfile
import tomllib

ARCHIVE_SHA256 = "d8c280bc7ebbb756eb06816efc2b16d63f85d7ef6c10dcc09bb612503cd78964"
RETAINED_FILES = {
    "LICENSE",
    "src/lib.rs",
    "src/error.rs",
    "src/monitor.rs",
    "src/window.rs",
    "src/video_recorder.rs",
    "src/macos/mod.rs",
    "src/macos/capture.rs",
    "src/macos/impl_monitor.rs",
    "src/macos/impl_window.rs",
    "src/macos/impl_video_recorder.rs",
    "src/linux/capture.rs",
    "src/linux/impl_monitor.rs",
    "src/linux/impl_video_recorder.rs",
    "src/linux/impl_window.rs",
    "src/linux/mod.rs",
    "src/linux/utils.rs",
    "src/linux/wayland_capture.rs",
    "src/linux/wayland_video_recorder.rs",
    "src/linux/xorg_capture.rs",
    "src/linux/xorg_video_recorder.rs",
    "src/windows/capture.rs",
    "src/windows/impl_monitor.rs",
    "src/windows/impl_video_recorder.rs",
    "src/windows/impl_window.rs",
    "src/windows/mod.rs",
    "src/windows/utils.rs",
}


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def verify(root, archive=None):
    identity = json.loads((root / "UPSTREAM.json").read_text(encoding="utf-8"))
    if (identity["name"], identity["version"], identity["license"]) != ("xcap", "0.5.2", "Apache-2.0"):
        raise ValueError("unexpected xcap source identity")
    if identity["artifact_sha256"] != ARCHIVE_SHA256:
        raise ValueError("unexpected xcap source archive checksum")
    files = identity["files"]
    if set(files) != RETAINED_FILES:
        raise ValueError("upstream hash inventory differs from the retained source contract")
    # Cargo auto-discovers build.rs, so reject added Rust files anywhere in the
    # package, not just inside src/.
    actual = {str(path.relative_to(root)) for path in root.rglob("*.rs")}
    actual.add("LICENSE")
    if actual != set(files):
        raise ValueError(f"vendored file inventory differs: {sorted(actual ^ set(files))}")
    for name, expected in files.items():
        path = root / name
        if path.is_symlink() or not path.is_file() or sha256(path.read_bytes()) != expected:
            raise ValueError(f"vendored file differs from xcap 0.5.2: {name}")
    if archive is not None:
        if sha256(archive.read_bytes()) != identity["artifact_sha256"]:
            raise ValueError("xcap source archive checksum differs from the published lock")
        with tarfile.open(archive, "r:gz") as source:
            for name in files:
                member = source.getmember(f"xcap-0.5.2/{name}")
                if not member.isfile():
                    raise ValueError(f"archive member is not an ordinary file: {name}")
                data = source.extractfile(member).read()
                if data != (root / name).read_bytes():
                    raise ValueError(f"vendored file differs from the source archive: {name}")
            upstream_manifest = tomllib.loads(source.extractfile("xcap-0.5.2/Cargo.toml.orig").read().decode("utf-8"))
        vendor_manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
        package = {
            "name": "xcap", "version": "0.5.2", "edition": "2021",
            "description": "macOS-only source subset of xcap 0.5.2",
            "license": "Apache-2.0", "repository": "https://github.com/nashaofu/xcap",
        }
        macos = 'cfg(target_os = "macos")'
        expected_manifest = {
            "package": package,
            "features": upstream_manifest["features"],
            "dependencies": upstream_manifest["dependencies"],
            "target": {macos: upstream_manifest["target"][macos]},
            "workspace": {},
        }
        if vendor_manifest != expected_manifest:
            raise ValueError("vendored manifest differs from the reviewed macOS-only upstream subset")
    print(f"Verified {len(files)} unmodified xcap 0.5.2 source/license files")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path)
    args = parser.parse_args()
    try:
        verify(Path(__file__).resolve().parent, args.archive)
    except (OSError, ValueError, KeyError, tarfile.TarError) as error:
        parser.exit(1, f"xcap provenance: {error}\n")
