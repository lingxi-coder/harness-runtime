#!/usr/bin/env python3
"""Where the Local App repository's checkout is, for the gates and scripts that read its files.

The Local App crates and the plugin tree live in their own repository and reach this workspace through Cargo
(`[workspace.dependencies]` in Cargo.toml). The checkout is wherever Cargo resolved them to: a git checkout under
CARGO_HOME, or a local clone that a `[patch]` points at. Ask Cargo, so a gate reads the revision this workspace
builds against and not whatever happens to sit in a neighbouring directory.

`LOCAL_APP_ROOT` overrides the answer (a CI step that already knows it, a local experiment).

    python3 scripts/lib/local_app_checkout.py        # prints the root
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

ANCHOR_PACKAGE = "local-app-plugin"  # crates/local-app-plugin/Cargo.toml: two levels below the checkout root


def local_app_root(repo: Path) -> Path:
    override = os.environ.get("LOCAL_APP_ROOT")
    if override:
        return Path(override).resolve()
    manifest = repo / "Cargo.toml"
    last_error = ""
    # Offline first (CI and local runs with a warm cache); a networked resolve only if that fails.
    for extra in (["--offline"], []):
        result = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--locked", *extra, "--manifest-path", str(manifest)],
            capture_output=True,
            text=True,
        )
        if result.returncode == 0:
            for package in json.loads(result.stdout)["packages"]:
                if package["name"] == ANCHOR_PACKAGE:
                    return Path(package["manifest_path"]).resolve().parent.parent.parent
            raise SystemExit(f"{ANCHOR_PACKAGE} is not in this workspace's dependency graph")
        last_error = result.stderr
    raise SystemExit(f"cannot resolve the Local App checkout: cargo metadata failed:\n{last_error}")


if __name__ == "__main__":
    print(local_app_root(Path(__file__).resolve().parents[2]))
