#!/usr/bin/env bash
#
# Resolve the pinned engine-free local-app seed dependencies INSIDE the
# local-app Alpine rootfs and emit the resulting `node_modules` tree.
#
# This is the producer stage that `validate-local-app-build-assets.sh` has been
# asking for: its failure message says "Set LINGXI_LOCAL_APP_NODE_MODULES and
# rebuild", but nothing in the repository ever produced that tree, so the iOS
# build wired itself into `--rootfs-only` mode and every device resolved the
# same 169 packages over the network on first `create_local_app`.
#
# Why a container and not the host: the tree ships to a device where Vite runs
# under musl on the guest's Node, so `@rolldown/binding-linux-<arch>-musl`,
# `@rollup/rollup-linux-<arch>-musl`, and `lightningcss-linux-<arch>-musl`
# are the bindings that must be resolved. A macOS `pnpm install` resolves the
# darwin bindings instead and `stage-local-app-runtime.py` rejects it -- late,
# and with a message about bindings rather than about the wrong builder.
#
# The rootfs tarball is the builder image, so pnpm, Node and their versions come
# from the artifact that actually ships rather than from a second pinning.
#
# Usage:
#   build-local-app-node-modules.sh --arch <aarch64|x86_64> \
#       [--rootfs <rootfs.tar.gz>] [--output <dir>]
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PINS="${REPO_ROOT}/docs/mobile-linux/local-app-runtime-pins.json"

ARCH=""
ROOTFS=""
OUTPUT=""
CONTAINER_RUNTIME="${CONTAINER_RUNTIME:-}"

usage() { sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch) ARCH="${2:-}"; shift 2 ;;
    --rootfs) ROOTFS="${2:-}"; shift 2 ;;
    --output|--output-dir) OUTPUT="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "${ARCH}" in
  aarch64|x86_64) ;;
  *) echo "--arch must be aarch64 or x86_64" >&2; exit 2 ;;
esac
[[ -n "${ROOTFS}" ]] || ROOTFS="${LINGXI_RUNTIME_BUILD_ROOT:-${REPO_ROOT}/build}/local-app-rootfs/${ARCH}/rootfs.tar.gz"
[[ -n "${OUTPUT}" ]] || OUTPUT="${LINGXI_RUNTIME_BUILD_ROOT:-${REPO_ROOT}/build}/local-app-node-modules/${ARCH}"
[[ "${OUTPUT}" == /* ]] || OUTPUT="${REPO_ROOT}/${OUTPUT}"
[[ -f "${PINS}" ]] || { echo "missing pins: ${PINS}" >&2; exit 1; }
if [[ ! -f "${ROOTFS}" ]]; then
  echo "missing local-app rootfs: ${ROOTFS}" >&2
  echo "Run scripts/mobile-linux/build-local-app-rootfs.sh --arch ${ARCH} first." >&2
  exit 1
fi

if [[ -z "${CONTAINER_RUNTIME}" ]]; then
  if command -v podman >/dev/null 2>&1; then CONTAINER_RUNTIME=podman
  elif command -v docker >/dev/null 2>&1; then CONTAINER_RUNTIME=docker
  else echo "neither podman nor docker is available" >&2; exit 1; fi
fi

TEMPLATE="${REPO_ROOT}/$(python3 -c '
import json, pathlib, sys
pins = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(pins["local_app_runtime"]["template"])
' "${PINS}")"
[[ -d "${TEMPLATE}" ]] || { echo "missing template: ${TEMPLATE}" >&2; exit 1; }

# The pins carry the lockfile digest the staged tree will be keyed by. Verifying
# it here means a template edit that forgot to refresh the pins fails at the
# producer rather than shipping a seed the engine will silently decline.
EXPECTED_LOCK_SHA="$(python3 -c '
import json, pathlib, sys
pins = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(pins["local_app_runtime"]["lockfile_sha256"])
' "${PINS}")"
ACTUAL_LOCK_SHA="$(shasum -a 256 "${TEMPLATE}/pnpm-lock.yaml" | awk '{print $1}')"
if [[ "${ACTUAL_LOCK_SHA}" != "${EXPECTED_LOCK_SHA}" ]]; then
  echo "template pnpm-lock.yaml does not match local_app_runtime.lockfile_sha256" >&2
  echo "  pins:     ${EXPECTED_LOCK_SHA}" >&2
  echo "  template: ${ACTUAL_LOCK_SHA}" >&2
  exit 1
fi

IMAGE="lingxi-local-app-rootfs:${ARCH}"
case "${ARCH}" in
  aarch64) IMAGE_ARCH="arm64" ;;
  x86_64) IMAGE_ARCH="amd64" ;;
esac

echo "[node_modules:${ARCH}] importing ${ROOTFS} as ${IMAGE}"
# Re-import every run: a stale image from an older rootfs would resolve against
# an older Node/pnpm while reporting success for the current one.
"${CONTAINER_RUNTIME}" rmi -f "${IMAGE}" >/dev/null 2>&1 || true
"${CONTAINER_RUNTIME}" import --arch "${IMAGE_ARCH}" --os linux "${ROOTFS}" "${IMAGE}" >/dev/null

WORK="$(mktemp -d "${TMPDIR:-/tmp}/lingxi-local-app-deps.XXXXXX")"
cleanup() { chmod -R u+w "${WORK}" 2>/dev/null || true; rm -rf "${WORK}"; }
trap cleanup EXIT
mkdir -p "${WORK}/project" "${WORK}/store" "${WORK}/state"
# Only the dependency inputs: resolving must not depend on app sources, and a
# stray file here would land in the shipped tree's provenance.
for file in package.json pnpm-lock.yaml pnpm-workspace.yaml; do
  cp "${TEMPLATE}/${file}" "${WORK}/project/${file}"
done
# pnpm 12 frozen workspace resolution also checks the widget importer.
if [[ -f "${TEMPLATE}/app/mcp-widget/package.json" ]]; then
  mkdir -p "${WORK}/project/app/mcp-widget"
  cp "${TEMPLATE}/app/mcp-widget/package.json" "${WORK}/project/app/mcp-widget/package.json"
fi

echo "[node_modules:${ARCH}] resolving the pinned lockfile inside the rootfs"
# The flags are the engine's own (local_apps_host.rs run_dependency_install), so
# the shipped tree is byte-identical in shape to one a device would build for
# itself -- including `--ignore-scripts`, which is why no postinstall runs here.
"${CONTAINER_RUNTIME}" run --rm \
  -v "${WORK}/project:/project" \
  -v "${WORK}/store:/var/lingxi/local-app-dependency-store" \
  -v "${WORK}/state:/state" \
  -e CI=1 \
  -e HOME=/state/home \
  -e TMPDIR=/state/tmp -e TMP=/state/tmp -e TEMP=/state/tmp \
  -e XDG_CACHE_HOME=/state/xdg-cache \
  -e XDG_CONFIG_HOME=/state/xdg-config \
  -e XDG_DATA_HOME=/state/xdg-data \
  -e PNPM_HOME=/state/pnpm-home \
  -e COREPACK_HOME=/state/corepack \
  -w /project \
  "${IMAGE}" \
  sh -c 'test "$(uname -m)" = "'"${ARCH}"'" || {
           echo "builder is $(uname -m), expected '"${ARCH}"'" >&2; exit 1; }
         exec /usr/bin/pnpm install --frozen-lockfile --ignore-scripts --no-runtime \
           --prefer-offline --store-dir /var/lingxi/local-app-dependency-store \
           --reporter=append-only'

VITE="${WORK}/project/node_modules/vite/bin/vite.js"
[[ -f "${VITE}" ]] || { echo "[node_modules:${ARCH}] resolve produced no vite entry point" >&2; exit 1; }

# Guard the reason this runs in a container at all. `stage-local-app-runtime.py`
# checks the same thing, but it reports "one real native binding" without saying
# the tree was built on the wrong operating system.
# Three families, all owned by the fixed Vite 8 toolchain. The bundled seed is
# deliberately engine-free: Three/Phaser/Babylon stay out of the base lock so
# React/Canvas apps can adopt it offline without carrying an unused renderer.
MUSL_BINDINGS=(
  "node_modules/@rolldown/binding-linux-${IMAGE_ARCH/amd64/x64}-musl"
  "node_modules/@rollup/rollup-linux-${IMAGE_ARCH/amd64/x64}-musl"
  "node_modules/lightningcss-linux-${IMAGE_ARCH/amd64/x64}-musl"
)
for binding in "${MUSL_BINDINGS[@]}"; do
  if [[ ! -d "${WORK}/project/${binding}" ]]; then
    echo "[node_modules:${ARCH}] resolved tree is missing ${binding}" >&2
    echo "       The tree was built for the wrong platform; rebuild inside the rootfs." >&2
    exit 1
  fi
done
for forbidden in node_modules/three node_modules/phaser node_modules/@babylonjs; do
  if [[ -e "${WORK}/project/${forbidden}" ]]; then
    echo "[node_modules:${ARCH}] resolved tree unexpectedly contains ${forbidden}" >&2
    echo "       The bundled seed must remain engine-free; use a runtime-profile-specific lock instead." >&2
    exit 1
  fi
done

mkdir -p "$(dirname "${OUTPUT}")"
chmod -R u+w "${OUTPUT}" 2>/dev/null || true
rm -rf "${OUTPUT}"
mkdir -p "${OUTPUT}"
# -a keeps the `.bin` shims as shims. Dereferencing them would double every
# binary they point at and break `vite` resolution on the device.
cp -a "${WORK}/project/node_modules" "${OUTPUT}/node_modules"
printf '%s\n' "${ACTUAL_LOCK_SHA}" > "${OUTPUT}/pnpm-lock.sha256"

echo "[node_modules:${ARCH}] lockfile sha256 ${ACTUAL_LOCK_SHA}"
du -sh "${OUTPUT}/node_modules" | awk '{print "[node_modules] tree size "$1}'
find "${OUTPUT}/node_modules" -type f | wc -l | awk '{print "[node_modules] files "$1}'
find "${OUTPUT}/node_modules" -type l | wc -l | awk '{print "[node_modules] shims "$1}'
echo "[node_modules:${ARCH}] wrote ${OUTPUT}/node_modules"
