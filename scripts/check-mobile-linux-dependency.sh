#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
python3 scripts/mobile-linux/test_sdk_source.py
python3 scripts/mobile-linux/sdk_source.py
