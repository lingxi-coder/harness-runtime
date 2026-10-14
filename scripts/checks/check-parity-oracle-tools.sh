#!/usr/bin/env bash
# Test the local oracle tooling without requiring a proprietary binary or network.
set -euo pipefail
cd "$(dirname "$0")/../.."
export PYTHONDONTWRITEBYTECODE=1
python3 scripts/tests/test_prompt_drift_scan.py
python3 scripts/tests/test_compact_live_oracle.py
