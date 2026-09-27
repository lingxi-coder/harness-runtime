#!/usr/bin/env bash
# push-spike.sh — push the staged payload to a real arm64 device over adb and
# perform the fully OFFLINE on-device install (rootfs extraction, local-file
# apk install of the node runtime, app template placement).
#
# The device never touches the network: every byte was fetched and verified on
# the Mac by stage-spike.sh. This script only runs adb push / adb shell.
#
# Usage:
#   push-spike.sh [--serial SERIAL] [--payload DIR]
#                 [--device-dir /data/local/tmp/lingxi-appdev-spike]
#                 [--skip-rootfs] [--skip-apk-install] [--skip-space-check]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPIKE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PINS="${SPIKE_DIR}/spike-pins.json"

SERIAL=""
PAYLOAD="${SPIKE_DIR}/build/staging/payload"
DEVICE_DIR="/data/local/tmp/lingxi-appdev-spike"
SKIP_ROOTFS=false
SKIP_APK_INSTALL=false
SKIP_SPACE_CHECK=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --serial) SERIAL="${2:-}"; shift 2 ;;
    --payload) PAYLOAD="${2:-}"; shift 2 ;;
    --device-dir) DEVICE_DIR="${2:-}"; shift 2 ;;
    --skip-rootfs) SKIP_ROOTFS=true; shift ;;
    --skip-apk-install) SKIP_APK_INSTALL=true; shift ;;
    --skip-space-check) SKIP_SPACE_CHECK=true; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ADB=(adb)
[[ -n "${SERIAL}" ]] && ADB=(adb -s "${SERIAL}")
run_adb() { "${ADB[@]}" "$@"; }
# adb shell output arrives CRLF-terminated; strip \r before comparing.
adb_shell_value() { run_adb shell "$@" | tr -d '\r'; }

[[ -f "${PAYLOAD}/payload-manifest.json" ]] || {
  echo "payload not staged at ${PAYLOAD} (run stage-spike.sh --step pack first)" >&2
  exit 1
}

ROOTFS_URL="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["rootfs"]["url"])' "${PINS}")"
ROOTFS_SHA="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["rootfs"]["sha256"])' "${PINS}")"
ROOTFS_FILE="$(basename "${ROOTFS_URL}")"

echo "== preflight"
STATE="$(adb_shell_value echo device-ok || true)"
[[ "${STATE}" == "device-ok" ]] || { echo "no usable adb device (adb devices; check USB debugging)" >&2; exit 1; }
ABI="$(adb_shell_value getprop ro.product.cpu.abi)"
if [[ "${ABI}" != "arm64-v8a" ]]; then
  echo "device ABI is '${ABI}', expected arm64-v8a — this spike targets real arm64 hardware only" >&2
  exit 1
fi
for tool in tar gzip sha256sum; do
  if ! run_adb shell "command -v ${tool}" > /dev/null 2>&1; then
    echo "device toybox is missing '${tool}' (Android 10+ expected)" >&2
    exit 1
  fi
done
if [[ "${SKIP_SPACE_CHECK}" != true ]]; then
  AVAIL_KB="$(adb_shell_value "df -k /data/local/tmp | tail -1" | awk '{ print $4 }')"
  if [[ ! "${AVAIL_KB}" =~ ^[0-9]+$ || "${AVAIL_KB}" -lt 3145728 ]]; then
    echo "need >= 3 GiB free under /data/local/tmp (have '${AVAIL_KB:-unknown}' kB); --skip-space-check to override" >&2
    exit 1
  fi
fi
echo "   device abi=${ABI} free_kb=${AVAIL_KB:-skipped}"

echo "== staging directories on device"
run_adb shell "mkdir -p '${DEVICE_DIR}/bin' '${DEVICE_DIR}/proot-tmp'"

if [[ "${SKIP_ROOTFS}" != true ]]; then
  echo "== rootfs: push + device-side digest check + extract"
  run_adb shell "rm -rf '${DEVICE_DIR}/rootfs'"
  run_adb push "${PAYLOAD}/${ROOTFS_FILE}" "${DEVICE_DIR}/${ROOTFS_FILE}"
  DEVICE_SHA="$(adb_shell_value "sha256sum '${DEVICE_DIR}/${ROOTFS_FILE}'" | awk '{ print $1 }')"
  if [[ "${DEVICE_SHA}" != "${ROOTFS_SHA}" ]]; then
    echo "rootfs digest mismatch after push: expected ${ROOTFS_SHA}, got ${DEVICE_SHA}" >&2
    exit 1
  fi
  run_adb shell "mkdir -p '${DEVICE_DIR}/rootfs' && cd '${DEVICE_DIR}/rootfs' && tar -xzf '../${ROOTFS_FILE}'"
  BUSYBOX_OK="$(adb_shell_value "test -f '${DEVICE_DIR}/rootfs/bin/busybox' && echo yes || echo no")"
  [[ "${BUSYBOX_OK}" == "yes" ]] || { echo "rootfs extraction failed (bin/busybox missing)" >&2; exit 1; }
  echo "   rootfs extracted"
else
  echo "== rootfs: skipped (--skip-rootfs)"
fi

echo "== nodejs apk closure -> rootfs /spike/apks"
run_adb shell "rm -rf '${DEVICE_DIR}/rootfs/spike/apks' && mkdir -p '${DEVICE_DIR}/rootfs/spike'"
run_adb push "${PAYLOAD}/apks" "${DEVICE_DIR}/rootfs/spike/"

echo "== app template -> rootfs /root/hello-next"
run_adb push "${PAYLOAD}/app-payload.tar.gz" "${DEVICE_DIR}/app-payload.tar.gz"
run_adb shell "rm -rf '${DEVICE_DIR}/rootfs/root/hello-next' && mkdir -p '${DEVICE_DIR}/rootfs/root/hello-next' && tar -xzf '${DEVICE_DIR}/app-payload.tar.gz' -C '${DEVICE_DIR}/rootfs/root/hello-next'"

echo "== guest measure script -> rootfs /opt/spike"
run_adb shell "mkdir -p '${DEVICE_DIR}/rootfs/opt/spike'"
run_adb push "${PAYLOAD}/guest/measure-spike.sh" "${DEVICE_DIR}/rootfs/opt/spike/measure-spike.sh"

echo "== proot + loader + wrapper"
run_adb push "${PAYLOAD}/bin/proot" "${DEVICE_DIR}/bin/proot"
run_adb push "${PAYLOAD}/bin/loader" "${DEVICE_DIR}/bin/loader"
run_adb push "${PAYLOAD}/device/run-in-guest.sh" "${DEVICE_DIR}/run-in-guest.sh"
run_adb shell "chmod 755 '${DEVICE_DIR}/bin/proot' '${DEVICE_DIR}/bin/loader' '${DEVICE_DIR}/run-in-guest.sh'"

echo "== sanity: PRoot boots the guest"
ALPINE="$(adb_shell_value "'${DEVICE_DIR}/run-in-guest.sh' 'cat /etc/alpine-release'")"
echo "   guest alpine-release: ${ALPINE}"
[[ "${ALPINE}" == 3.21.* ]] || { echo "unexpected alpine-release '${ALPINE}'" >&2; exit 1; }

if [[ "${SKIP_APK_INSTALL}" != true ]]; then
  echo "== offline node runtime install (local .apk files only, --no-network)"
  run_adb shell "'${DEVICE_DIR}/run-in-guest.sh' 'apk add --no-network --no-cache /spike/apks/*.apk'"
  NODE_V="$(adb_shell_value "'${DEVICE_DIR}/run-in-guest.sh' 'node --version'")"
  echo "   guest node: ${NODE_V}"
  [[ "${NODE_V}" == v* ]] || { echo "node did not run in the guest" >&2; exit 1; }
  if run_adb shell "'${DEVICE_DIR}/run-in-guest.sh' 'command -v npm'" > /dev/null 2>&1; then
    echo "npm is present in the guest — forbidden; re-check the apk closure" >&2
    exit 1
  fi
else
  echo "== offline apk install: skipped (--skip-apk-install)"
fi

echo
echo "push complete. Next:"
echo "  1. Baseline host metrics:   scripts/mac/record-host-metrics.sh --label before"
echo "  2. Run the measurements:    adb shell ${DEVICE_DIR}/run-in-guest.sh 'sh /opt/spike/measure-spike.sh'"
echo "  3. After-run host metrics:  scripts/mac/record-host-metrics.sh --label after"
echo "  4. Pull results:            adb shell cat ${DEVICE_DIR}/rootfs/root/spike-results.json"
