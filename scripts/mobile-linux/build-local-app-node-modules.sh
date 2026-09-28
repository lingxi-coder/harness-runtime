#!/usr/bin/env bash
set -euo pipefail
exec python3 "$(dirname "$0")/build-local-app-node-modules.py" "$@"
