#!/usr/bin/env bash
# P4 — no crate under crates/ names a Local App identifier (Local App is an externally installed plugin). Scans tracked files only (stage new files before running); see check_product_neutral.py.
set -euo pipefail
cd "$(dirname "$0")/../.."
exec python3 "$(dirname "$0")/check_product_neutral.py" "$PWD"
