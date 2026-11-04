#!/usr/bin/env python3
"""Keep the independently tested fork on the same shared dependency versions."""

from collections import defaultdict
from pathlib import Path
import sys
import tomllib

fork = Path(__file__).resolve().parent
root = fork.parents[1]


def packages(path):
    result = defaultdict(set)
    with path.open("rb") as stream:
        for package in tomllib.load(stream)["package"]:
            result[package["name"]].add(
                (package["version"], package.get("source"), package.get("checksum"))
            )
    return result


production = packages(root / "Cargo.lock")
tested = packages(fork / "Cargo.lock")
required = {"xcap", "image", "zbus", "zbus_macros", "zbus_utils", "zruntime", "tokio"}
errors = []
for name in sorted(required):
    if name not in production or name not in tested:
        errors.append(f"{name} missing from one of the lockfiles")

for name in sorted(production.keys() & tested.keys()):
    extra = tested[name] - production[name]
    # The upstream image/default feature is resolved by the standalone lock;
    # it is not enabled by the production dependency or these tests.
    if name == "quick-error":
        extra = {identity for identity in extra if identity[0] != "2.0.1"}
    if extra:
        errors.append(f"{name}: standalone has {extra}, production has {production[name]}")

if errors:
    print("xcap test lock differs from the Harness lock:", file=sys.stderr)
    for error in errors:
        print(f"  {error}", file=sys.stderr)
    sys.exit(1)
print("xcap shared dependency versions and checksums match the Harness lock")
