#!/usr/bin/env bash
set -euo pipefail
exec python3 "$(dirname "$0")/../lib/sdk_delegate.py" "package-rootfs-release.sh" "$@"
