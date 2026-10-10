#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
enabled="${LINGXI_MOBILE_LINUX_ENABLED:-0}"

"${script_dir}/../mobile-linux/check-authorizations.sh"
if [[ "${enabled}" == "1" ]]; then
  "${script_dir}/../mobile-linux/check-rootfs-manifest.sh" \
    "${MOBILE_LINUX_EVIDENCE_DIR:?enabled release requires external evidence}/rootfs-manifest.json" "$@"
else
  sdk_root="$(python3 "${script_dir}/../lib/sdk_source.py" "$@")"
  bash "${sdk_root}/scripts/checks/check-resource-contracts.sh"
fi
"${script_dir}/../mobile-linux/check-sbom-and-licenses.sh" "$@"
echo "mobile-linux guardrail smoke checks passed"
