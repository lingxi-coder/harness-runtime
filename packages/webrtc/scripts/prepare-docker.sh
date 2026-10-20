#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
# Reproducible generated input; never copy a user's Cargo home or credentials.
cargo vendor --locked --manifest-path "$root/Cargo.toml" "$root/.docker-vendor" >/dev/null
