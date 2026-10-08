#!/usr/bin/env bash
# The SDK's release-archive verification, driven through this repository's delegates: a rootfs digest that is
# not committed or does not match must be rejected, and the SDK's real arm64 candidate inventory must validate.
# (The Local App runtime-seed supply-chain suite lives with the Local App: its `scripts/tests/`.)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SDK_ARGS=()
if [[ "${1:-}" == "--sdk-root" ]]; then SDK_ARGS=(--sdk-root "$2"); shift 2; fi
SDK_ROOT="$(python3 "${SCRIPT_DIR}/../lib/sdk_source.py" "${SDK_ARGS[@]}")"
TEMP_ROOT="$(mktemp -d)"
trap 'rm -rf "${TEMP_ROOT}"' EXIT

# A negative test must fail for the reason it names, not merely fail.
#
# `if cmd; then echo "expected ..."; exit 1; fi` cannot tell "rejected the bad
# input" from "crashed before it ever looked at the input": `fail()` raises
# SystemExit(1) and an uncaught Python exception also exits 1, while `if`
# suspends `set -e` around both. That is not hypothetical -- a NameError
# introduced while editing the verifier made every one of these cases report
# success, and this suite printed "tests passed".
#
# Exit code 1 AND no traceback is the discriminator: a usage error exits 2, a
# crash prints a traceback, and only a real rejection is a silent exit 1.
expect_rejection() {
  local label="$1"
  shift
  local output status
  set +e
  output="$("$@" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -eq 0 ]]; then
    echo "expected ${label}, but the command succeeded" >&2
    printf '%s\n' "${output}" >&2
    exit 1
  fi
  if printf '%s' "${output}" | grep -q "Traceback (most recent call last)"; then
    echo "expected ${label}, but the command CRASHED instead of rejecting" >&2
    printf '%s\n' "${output}" >&2
    exit 1
  fi
  if [[ "${status}" -ne 1 ]]; then
    echo "expected ${label} to exit 1, got ${status}" >&2
    printf '%s\n' "${output}" >&2
    exit 1
  fi
}
ROOTFS_PINS="${TEMP_ROOT}/rootfs-pins"
mkdir -p "${ROOTFS_PINS}"
printf 'package-augmented rootfs fixture\n' > "${ROOTFS_PINS}/rootfs.tar.gz"
python3 - "${ROOTFS_PINS}" <<'PY'
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
digest = hashlib.sha256((root / "rootfs.tar.gz").read_bytes()).hexdigest()
(root / "source-only-pins.json").write_text(
    json.dumps({"rootfs": {"archives": {"arm64-v8a": {"sha256": "e" * 64}}}}),
    encoding="utf-8",
)
(root / "wrong-pins.json").write_text(
    json.dumps({"rootfs": {"release_archives": {"arm64-v8a": {"sha256": "0" * 64}}}}),
    encoding="utf-8",
)
(root / "pins.json").write_text(
    json.dumps({"rootfs": {"release_archives": {"arm64-v8a": {"sha256": digest}}}}),
    encoding="utf-8",
)
PY
expect_rejection "an uncommitted release rootfs digest to fail validation" \
  python3 "${SCRIPT_DIR}/../local-apps/rootfs_tool.py" --sdk-root "${SDK_ROOT}" verify-release-archive \
  --pins "${ROOTFS_PINS}/source-only-pins.json" --abi arm64-v8a \
  --archive "${ROOTFS_PINS}/rootfs.tar.gz"
expect_rejection "a release rootfs digest mismatch to fail validation" \
  python3 "${SCRIPT_DIR}/../local-apps/rootfs_tool.py" --sdk-root "${SDK_ROOT}" verify-release-archive \
  --pins "${ROOTFS_PINS}/wrong-pins.json" --abi arm64-v8a \
  --archive "${ROOTFS_PINS}/rootfs.tar.gz"
python3 "${SCRIPT_DIR}/../local-apps/rootfs_tool.py" --sdk-root "${SDK_ROOT}" verify-release-archive \
  --pins "${ROOTFS_PINS}/pins.json" --abi arm64-v8a \
  --archive "${ROOTFS_PINS}/rootfs.tar.gz"

# Real SDK candidate inventory replaces the inherited unanchored-source-pin
# assertion. Release pipelines additionally require actual archive bytes; the
# synthetic mismatch/missing-pin rejection probes above remain mandatory.
EVIDENCE="${SDK_ROOT}/docs/mobile-linux/releases/3.24.2/arm64-v8a"
bash "${SDK_ROOT}/scripts/rootfs/check-rootfs-manifest.sh" "${EVIDENCE}/rootfs-manifest.json"
python3 "${SDK_ROOT}/scripts/rootfs/verify-evidence.py" --evidence-dir "${EVIDENCE}"

echo "rootfs release evidence tests passed (actual SDK arm64 candidate inventory verified; release archive acceptance is separate)"
