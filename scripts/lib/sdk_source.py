#!/usr/bin/env python3
"""Resolve SDK resources from locked Cargo identity; explicit roots are development inputs only."""
import argparse
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib
sys.dont_write_bytecode = True

ROOT = Path(__file__).resolve().parents[2]
REPOSITORY = "https://github.com/lingxi-coder/mobile-linux-runtime.git"
PACKAGES = {"mobile-linux-api", "mobile-linux-core", "mobile-linux-android", "mobile-linux-ios", "mobile-linux-ffi", "platform-pty"}


def inspect_metadata(metadata, dependency):
    revision = dependency.get("rev", "")
    if dependency.get("git") != REPOSITORY or not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("SDK requires canonical Git URL and full commit SHA")
    source = f"git+{REPOSITORY}?rev={revision}#{revision}"
    packages = {}
    for p in metadata["packages"]:
        if p["name"] not in PACKAGES:
            continue
        if p.get("source") != source or p["name"] in packages:
            raise ValueError("SDK package has a different or duplicate Cargo identity")
        packages[p["name"]] = p
    if "mobile-linux-api" not in packages:
        raise ValueError("locked Cargo metadata does not contain mobile-linux-api")
    anchor = Path(packages["mobile-linux-api"]["manifest_path"]).resolve()
    root = anchor.parents[2]
    if anchor.relative_to(root).as_posix() != "crates/mobile-linux-api/Cargo.toml":
        raise ValueError("unexpected SDK source layout")
    for p in packages.values():
        Path(p["manifest_path"]).resolve().relative_to(root)
    for p in metadata["packages"]:
        for dep in p.get("dependencies", []):
            if dep["name"] not in PACKAGES:
                continue
            if dep.get("path"):
                if p.get("source") != source:
                    raise ValueError("host retains a local SDK dependency")
                Path(dep["path"]).resolve().relative_to(root)
            elif dep.get("source") not in (source, source.rsplit("#", 1)[0]):
                raise ValueError("inactive SDK edge has a different source")
    return {"root": str(root), "revision": revision, "source": source, "packages": sorted(packages)}


def resolve_sdk(explicit_root=None):
    if explicit_root:
        # Explicit development/test invocation, never a sibling/cache fallback.
        root = Path(explicit_root).resolve(strict=True)
        manifest = tomllib.loads((root / "crates/mobile-linux-api/Cargo.toml").read_text())
        if manifest["package"]["name"] != "mobile-linux-api":
            raise ValueError("explicit SDK root has the wrong package marker")
        return root
    dependency = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["dependencies"]["mobile-linux-api"]
    raw = subprocess.check_output(["cargo", "metadata", "--locked", "--format-version=1", "--all-features"], cwd=ROOT, text=True)
    return Path(inspect_metadata(json.loads(raw), dependency)["root"])


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--sdk-root", help="explicit development/test source; production omits this")
    args = parser.parse_args()
    print(resolve_sdk(args.sdk_root))
