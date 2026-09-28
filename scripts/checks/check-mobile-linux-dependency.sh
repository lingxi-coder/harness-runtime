#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."
python3 scripts/tests/test_sdk_source.py
python3 scripts/lib/sdk_source.py
