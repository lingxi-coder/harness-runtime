#!/usr/bin/env bash
# M8-P14 — dependency-graph CI gate.
#
# Enforces the composition-root architecture (design §8.1): library crates make
# no shipping choices, so dependency edges must flow strictly "downhill". Reads
# declared workspace deps via `cargo metadata --locked --no-deps` (offline-safe — the
# full resolve isn't needed; only *direct* workspace edges are checked) and
# fails CI if any crate depends in a forbidden direction.
#
# Rules (per crate class, by directory):
#   tool      (tools/<x>)        ✗ tool ✗ platform ✗ skill ✗ command ✗ app
#   platform  (platforms/<x>)    ✗ tool ✗ skill ✗ command ✗ app   (sibling platform + engine OK)
#   skill     (skills/<x>)       ✗ tool ✗ platform ✗ command ✗ app
#   command   (commands/<x>)     ✗ tool ✗ platform ✗ app ✗ sibling-command
#   engine    (root-level)       ✗ tool ✗ platform ✗ skill ✗ command ✗ app ✗ monolith
#   GLOBAL                       ✗ any non-leaf crate depending on an app/example leaf
#   shared core                   ✗ domain, telemetry, platform implementation crates
#
# Exemptions: the legacy `tools` monolith aggregator + test-fixture crates.
#
# Usage: scripts/checks/check-deps.sh [--list]
set -euo pipefail

cd "$(dirname "$0")/../.."
HERE="$(dirname "$0")"

# `--offline` first (CI/local with a warm cache); fall back to a networked
# metadata call only if the offline lockfile is incomplete.
if META="$(cargo metadata --locked --format-version=1 --no-deps --offline 2>/dev/null)"; then
    :
else
    META="$(cargo metadata --locked --format-version=1 --no-deps)"
fi

printf '%s' "$META" | python3 "$HERE/check_deps.py" "${1:-}"
