#!/usr/bin/env bash
set -euo pipefail
exec python3 "$(dirname "$0")/../lib/sdk_delegate.py" "check-rootfs-manifest.sh" "$@"
