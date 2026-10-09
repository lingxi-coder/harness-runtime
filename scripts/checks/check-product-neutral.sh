#!/usr/bin/env bash
# P4 — the engine crates that must stay product-neutral (core, mcp, tasks, permission) name no Local App
# identifier. Scans tracked files only (stage new files before running); see check_product_neutral.py.
set -euo pipefail
cd "$(dirname "$0")/../.."
exec python3 "$(dirname "$0")/check_product_neutral.py" "$PWD"
