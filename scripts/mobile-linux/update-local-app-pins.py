#!/usr/bin/env python3
"""Fold a resolved APK closure into the local-app runtime pins, or check it.

The pins file is the single source of version truth for the on-device Alpine
toolchain, so the closure it records has to be *generated* from a real
resolution rather than hand-maintained. `--check` is the release-gate direction:
it re-runs the comparison without writing, so CI fails when a rebuild produces a
closure that differs from what is committed.
"""
from __future__ import annotations

import argparse
import json
import pathlib
import sys

# Product ABI name -> Alpine arch. The pins are keyed by the ABI the clients
# use, while apk speaks Alpine's arch names.
ABI_BY_ALPINE_ARCH = {"aarch64": "arm64-v8a", "x86_64": "x86_64"}

ARTIFACT_KEYS = (
    "name",
    "version",
    "role",
    "repository",
    "arch",
    "url",
    "sha256",
    "availability",
)


def fail(message: str) -> None:
    print(message, file=sys.stderr)
    raise SystemExit(1)


def normalized(artifacts: list[dict]) -> list[dict]:
    """Artifacts in a stable order with a stable key order.

    Both orders are fixed here so a rebuild that resolves the same closure
    produces a byte-identical pins section, and `--check` can compare directly.
    """
    ordered = sorted(artifacts, key=lambda item: (item["name"], item["version"]))
    return [{key: item[key] for key in ARTIFACT_KEYS} for item in ordered]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--pins", required=True)
    parser.add_argument("--arch", required=True, choices=sorted(ABI_BY_ALPINE_ARCH))
    parser.add_argument("--closure", required=True)
    parser.add_argument("--check", action="store_true")
    parser.add_argument(
        "--blocker",
        help=(
            "Record the closure as resolved but NOT release-ready, with this "
            "reason. Use when the artifacts are fully hashed but the offline "
            "install could not be verified on this host."
        ),
    )
    args = parser.parse_args()

    pins_path = pathlib.Path(args.pins)
    pins = json.loads(pins_path.read_text(encoding="utf-8"))
    closure = json.loads(pathlib.Path(args.closure).read_text(encoding="utf-8"))

    if closure.get("arch") != args.arch:
        fail(f"closure manifest is for {closure.get('arch')!r}, expected {args.arch!r}")

    artifacts = closure.get("artifacts")
    if not isinstance(artifacts, list) or not artifacts:
        fail("closure manifest carries no artifacts")
    for artifact in artifacts:
        missing = [key for key in ARTIFACT_KEYS if key not in artifact]
        if missing:
            fail(f"closure artifact {artifact.get('name')!r} is missing {missing}")

    abi = ABI_BY_ALPINE_ARCH[args.arch]
    record = {
        "alpine_arch": args.arch,
        "closure_status": "blocked" if args.blocker else "complete",
        "artifacts": normalized(artifacts),
    }
    if args.blocker:
        record["blocker"] = args.blocker

    existing = pins.setdefault("apk_artifacts", {}).get(abi)
    if args.check:
        if existing != record:
            fail(
                f"resolved closure for {abi} does not match the committed pins; "
                f"re-run build-local-app-rootfs.sh --arch {args.arch} --emit-pins"
            )
        print(f"closure for {abi} matches the pins ({len(artifacts)} artifacts)")
        return

    pins["apk_artifacts"][abi] = record
    # release_ready is derived, never asserted by hand. It requires every
    # supported ABI to be present AND complete -- "all recorded entries are
    # complete" would report ready after the very first architecture was built,
    # while the other one was still missing entirely.
    expected_abis = set(ABI_BY_ALPINE_ARCH.values())
    recorded = pins["apk_artifacts"]
    pins["release_ready"] = expected_abis.issubset(recorded) and all(
        recorded[name].get("closure_status") == "complete" for name in expected_abis
    )
    pins_path.write_text(
        json.dumps(pins, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(
        f"recorded {len(artifacts)} artifacts for {abi}; "
        f"release_ready={str(pins['release_ready']).lower()}"
    )


if __name__ == "__main__":
    main()
