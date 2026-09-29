#!/usr/bin/env bash
# Wrapper so check-all.sh discovers this gate (it globs scripts/check-*.sh).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."
python3 scripts/checks/check_absolute_symlinks.py
