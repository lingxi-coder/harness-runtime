#!/usr/bin/env bash
set -euo pipefail
exec python3 "$(dirname "$0")/sdk_delegate.py" "build-local-app-rootfs.sh" "$@"
