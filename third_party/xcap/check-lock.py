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
optional_image_quick_error = (
    "2.0.1",
    "registry+https://github.com/rust-lang/crates.io-index",
    "a993555f31e5a609f617c12db6250dedcac1b0a85076912c436e6fc9b2c8e6a3",
)
errors = []
for name in sorted(required):
    if name not in production or name not in tested:
        errors.append(f"{name} missing from one of the lockfiles")

for name in sorted(tested):
    if name not in production and name != "quick-error":
        continue
    extra = tested[name] - production.get(name, set())
    # The upstream image/default feature is resolved by the standalone lock;
    # it is not enabled by the production dependency or these tests.
    if name == "quick-error":
        extra.discard(optional_image_quick_error)
    if extra:
        errors.append(f"{name}: standalone has {extra}, production has {production.get(name, set())}")

if errors:
    print("xcap test lock differs from the Harness lock:", file=sys.stderr)
    for error in errors:
        print(f"  {error}", file=sys.stderr)
    sys.exit(1)
print("xcap shared dependency versions and checksums match the Harness lock")
