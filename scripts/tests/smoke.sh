#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
enabled="${LINGXI_MOBILE_LINUX_ENABLED:-0}"

"${script_dir}/../local-apps/check-authorizations.sh"
if [[ "${enabled}" == "1" ]]; then
  "${script_dir}/../local-apps/check-rootfs-manifest.sh" \
    "${MOBILE_LINUX_EVIDENCE_DIR:?enabled release requires external evidence}/rootfs-manifest.json" "$@"
else
  sdk_root="$(python3 "${script_dir}/../lib/sdk_source.py" "$@")"
  bash "${sdk_root}/scripts/check-resource-contracts.sh"
fi
"${script_dir}/../local-apps/check-sbom-and-licenses.sh" "$@"
if [[ "${enabled}" == "1" ]]; then
  apk_dir="${LINGXI_LOCAL_APP_APK_DIR:-}"
  if [[ -z "${apk_dir}" ]]; then
    echo "LINGXI_LOCAL_APP_APK_DIR is required for an enabled local-app runtime release" >&2
    exit 1
  fi
  python3 "${script_dir}/../local-apps/verify-local-app-supply-chain.py" \
    --repo-root "${repo_root}" "$@" \
    --release \
    --apk-dir "${apk_dir}"
else
  python3 "${script_dir}/../local-apps/verify-local-app-supply-chain.py" \
    --repo-root "${repo_root}" "$@"
fi

echo "mobile-linux guardrail smoke checks passed"
