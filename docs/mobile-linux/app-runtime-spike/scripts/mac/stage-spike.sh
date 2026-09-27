#!/usr/bin/env bash
# stage-spike.sh — Mac-side staging for the local-apps phase-0 app-runtime spike.
#
# ALL network fetching happens here on the Mac. The Android device is treated
# as fully offline; push-spike.sh only moves already-verified bytes over adb.
#
# Steps:
#   fetch   download external artifacts into the staging dir:
#             - Alpine minirootfs 3.21.3 aarch64 archive (pinned sha256)
#             - npm dependency tree for template/ (lockfile-pinned once
#               template/package-lock.json is committed)
#             - @next/swc-linux-arm64-musl tarball matching the resolved
#               next version
#             - nodejs apk dependency closure via `docker run alpine:3.21.3
#               apk fetch --recursive nodejs` (Docker only needed at pin time)
#   verify  hash every artifact against ../../spike-pins.json; placeholder
#           pins fail unless --allow-unpinned, and a candidate pin block is
#           written to <staging>/spike-pins.candidate.json for pasting back
#   pack    assemble the adb-pushable payload under <staging>/payload
#   all     fetch + verify + pack
#
# Usage:
#   stage-spike.sh --step <fetch|verify|pack|all> [--variant <play|direct>]
#                  [--staging DIR] [--allow-unpinned]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPIKE_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
REPO_ROOT="$(cd "${SPIKE_DIR}/../../.." && pwd)"
PINS="${SPIKE_DIR}/spike-pins.json"
TEMPLATE_DIR="${SPIKE_DIR}/template"

STEP=""
VARIANT="direct"
STAGING="${SPIKE_DIR}/build/staging"
ALLOW_UNPINNED=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --step) STEP="${2:-}"; shift 2 ;;
    --variant) VARIANT="${2:-}"; shift 2 ;;
    --staging) STAGING="${2:-}"; shift 2 ;;
    --allow-unpinned) ALLOW_UNPINNED=true; shift ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "${STEP}" in
  fetch|verify|pack|all) ;;
  *) echo "usage: $0 --step <fetch|verify|pack|all> [--variant <play|direct>] [--staging DIR] [--allow-unpinned]" >&2; exit 2 ;;
esac
case "${VARIANT}" in
  play|direct) ;;
  *) echo "invalid --variant '${VARIANT}' (expected play or direct)" >&2; exit 2 ;;
esac

APP_WORK="${STAGING}/app-work"
APKS_DIR="${STAGING}/apks"
SWC_DIR="${STAGING}/swc"
PAYLOAD="${STAGING}/payload"
mkdir -p "${STAGING}"

pins_get() {
  python3 - "${PINS}" "$1" <<'PY'
import json, sys
pins = json.load(open(sys.argv[1]))
node = pins
for part in sys.argv[2].split("."):
    node = node[part]
print(node)
PY
}

sha256_of() { shasum -a 256 "$1" | awk '{ print $1 }'; }

count_apks() {
  local n=0 f
  for f in "${APKS_DIR}"/*.apk; do
    [[ -e "${f}" ]] && n=$((n + 1))
  done
  echo "${n}"
}

resolved_next_version() {
  python3 - "${APP_WORK}/node_modules/next/package.json" <<'PY'
import json, sys
print(json.load(open(sys.argv[1]))["version"])
PY
}

step_fetch() {
  echo "== fetch: Alpine minirootfs archive"
  local rootfs_url rootfs_sha rootfs_file
  rootfs_url="$(pins_get rootfs.url)"
  rootfs_sha="$(pins_get rootfs.sha256)"
  rootfs_file="${STAGING}/$(basename "${rootfs_url}")"
  if [[ ! -f "${rootfs_file}" ]]; then
    curl -fL --retry 3 -o "${rootfs_file}" "${rootfs_url}"
  fi
  local actual
  actual="$(sha256_of "${rootfs_file}")"
  if [[ "${actual}" != "${rootfs_sha}" ]]; then
    echo "rootfs archive sha256 mismatch: expected ${rootfs_sha}, got ${actual}" >&2
    exit 1
  fi
  echo "   ok: ${rootfs_file}"

  echo "== fetch: npm dependency tree for template/"
  if [[ ! -d "${APP_WORK}/node_modules/next" ]]; then
    rm -rf "${APP_WORK}"
    mkdir -p "${APP_WORK}"
    cp "${TEMPLATE_DIR}/package.json" "${APP_WORK}/"
    cp "${TEMPLATE_DIR}/next.config.mjs" "${APP_WORK}/"
    cp -R "${TEMPLATE_DIR}/app" "${APP_WORK}/app"
    if [[ -f "${TEMPLATE_DIR}/package-lock.json" ]]; then
      cp "${TEMPLATE_DIR}/package-lock.json" "${APP_WORK}/"
      (cd "${APP_WORK}" && npm ci --ignore-scripts --no-audit --no-fund --loglevel=error)
    else
      echo "   NOTE: template/package-lock.json missing — running npm install (unpinned)." >&2
      echo "   Commit ${APP_WORK}/package-lock.json back to template/ to pin the tree." >&2
      (cd "${APP_WORK}" && npm install --ignore-scripts --no-audit --no-fund --loglevel=error)
    fi
  fi
  local next_version
  next_version="$(resolved_next_version)"
  echo "   ok: node_modules staged (next ${next_version})"

  echo "== fetch: @next/swc-linux-arm64-musl ${next_version}"
  mkdir -p "${SWC_DIR}"
  local swc_file="${SWC_DIR}/swc-linux-arm64-musl-${next_version}.tgz"
  if [[ ! -f "${swc_file}" ]]; then
    curl -fL --retry 3 -o "${swc_file}" \
      "https://registry.npmjs.org/@next/swc-linux-arm64-musl/-/swc-linux-arm64-musl-${next_version}.tgz"
  fi
  echo "   ok: ${swc_file}"

  echo "== fetch: nodejs apk closure (Alpine v3.21 aarch64)"
  mkdir -p "${APKS_DIR}"
  if compgen -G "${APKS_DIR}/*.apk" > /dev/null; then
    echo "   ok: $(count_apks) apk files already present (delete ${APKS_DIR} to refetch)"
  else
    echo "   running Docker resolver (needs the Docker daemon; pin-time only)…"
    docker run --rm --platform linux/arm64 -v "${APKS_DIR}:/out" alpine:3.21.3 \
      sh -c 'apk update -q && apk fetch --recursive --output /out nodejs'
    echo "   ok: fetched $(count_apks) apk files"
    echo "   (no-Docker fallback: once spike-pins.json apk_closure is filled in, curl each"
    echo "    file from $(pins_get node_runtime.url_template) instead)"
  fi
}

step_verify() {
  echo "== verify: hashing artifacts against ${PINS}"
  local allow_flag=0
  [[ "${ALLOW_UNPINNED}" == true ]] && allow_flag=1
  python3 - "${PINS}" "${STAGING}" "${allow_flag}" <<'PY'
import fnmatch
import hashlib
import json
import pathlib
import sys
import tarfile

pins_path, staging, allow_unpinned = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2]), sys.argv[3] == "1"
pins = json.loads(pins_path.read_text())
placeholder = lambda s: isinstance(s, str) and s.startswith("REPLACE_WITH_REAL")
sha256 = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
problems, unpinned = [], []

# 1. rootfs archive (always a real pin — inherited from mobile-linux-pins.json).
rootfs_file = staging / pins["rootfs"]["url"].rsplit("/", 1)[-1]
if not rootfs_file.is_file():
    problems.append(f"missing rootfs archive {rootfs_file} (run --step fetch)")
elif sha256(rootfs_file) != pins["rootfs"]["sha256"]:
    problems.append(f"rootfs sha256 mismatch for {rootfs_file}")

# 2. apk closure: forbidden patterns, then per-file pins.
apks = sorted((staging / "apks").glob("*.apk"))
if not apks:
    problems.append("no apk files staged (run --step fetch)")
forbidden = pins["node_runtime"]["forbidden_package_patterns"]
for apk in apks:
    for pattern in forbidden:
        if fnmatch.fnmatch(apk.name, pattern):
            problems.append(f"forbidden package staged: {apk.name} (npm/corepack must NOT reach the device)")
closure_actual = [{"file": a.name, "sha256": sha256(a)} for a in apks]
closure_pinned = pins["node_runtime"]["apk_closure"]
if any(placeholder(e.get("sha256", "")) for e in closure_pinned):
    unpinned.append("node_runtime.apk_closure")
else:
    want = {e["file"]: e["sha256"] for e in closure_pinned}
    got = {e["file"]: e["sha256"] for e in closure_actual}
    if want != got:
        problems.append(
            "apk closure differs from pins: "
            f"missing={sorted(set(want) - set(got))} extra={sorted(set(got) - set(want))} "
            f"changed={sorted(k for k in set(want) & set(got) if want[k] != got[k])}"
        )

# 3. next version + swc tarball.
next_pkg = staging / "app-work/node_modules/next/package.json"
if not next_pkg.is_file():
    problems.append("app-work/node_modules/next missing (run --step fetch)")
    next_version = None
else:
    next_version = json.loads(next_pkg.read_text())["version"]
    if not next_version.startswith("16."):
        problems.append(f"resolved next version {next_version} is not 16.x")
    pinned_next = pins["next_template"]["resolved_next_version"]
    if placeholder(pinned_next):
        unpinned.append("next_template.resolved_next_version")
    elif pinned_next != next_version:
        problems.append(f"resolved next {next_version} != pinned {pinned_next}")

swc_pin = pins["next_template"]["swc_native_binding"]
swc_file = None
if next_version:
    swc_file = staging / f"swc/swc-linux-arm64-musl-{next_version}.tgz"
    if not swc_file.is_file():
        problems.append(f"missing swc tarball {swc_file} (run --step fetch)")
        swc_file = None
if swc_file:
    swc_sha = sha256(swc_file)
    if placeholder(swc_pin["sha256"]):
        unpinned.append("next_template.swc_native_binding.sha256")
    elif swc_pin["sha256"] != swc_sha:
        problems.append(f"swc tarball sha256 mismatch: expected {swc_pin['sha256']}, got {swc_sha}")
    if not placeholder(swc_pin["version"]) and swc_pin["version"] != next_version:
        problems.append(f"swc pinned version {swc_pin['version']} != next {next_version}")
    with tarfile.open(swc_file) as tar:
        names = tar.getnames()
    if not any(n.endswith(".node") for n in names):
        problems.append("swc tarball contains no .node native binding")
    pkg_member = [n for n in names if n.endswith("package/package.json")]
    if pkg_member:
        with tarfile.open(swc_file) as tar:
            swc_meta = json.load(tar.extractfile(pkg_member[0]))
        if next_version and swc_meta.get("version") != next_version:
            problems.append(f"swc package version {swc_meta.get('version')} != next {next_version}")

# 4. lockfile pin status.
lock = pins_path.parent / "template/package-lock.json"
if not lock.is_file():
    unpinned.append("template/package-lock.json (commit it from <staging>/app-work/package-lock.json)")

candidate = {
    "resolved_next_version": next_version,
    "swc_native_binding": {
        "package": "@next/swc-linux-arm64-musl",
        "version": next_version,
        "sha256": sha256(swc_file) if swc_file else None,
    },
    "apk_closure": closure_actual,
}
candidate_path = staging / "spike-pins.candidate.json"
candidate_path.write_text(json.dumps(candidate, indent=2) + "\n")

for p in problems:
    print(f"FAIL: {p}", file=sys.stderr)
for u in unpinned:
    print(f"UNPINNED: {u}", file=sys.stderr)
print(f"candidate pin block written to {candidate_path}")
if problems:
    sys.exit(1)
if unpinned and not allow_unpinned:
    print("placeholder pins remain — paste the candidate block into spike-pins.json,", file=sys.stderr)
    print("or re-run with --allow-unpinned for a first exploratory run.", file=sys.stderr)
    sys.exit(1)
print("verify: ok")
PY
}

step_pack() {
  echo "== pack: assembling payload"
  local next_version
  next_version="$(resolved_next_version)"

  local proot_src="${REPO_ROOT}/clients/android/app/src/${VARIANT}/jniLibs/arm64-v8a"
  if [[ ! -f "${proot_src}/libproot.so" || ! -f "${proot_src}/libproot-loader.so" ]]; then
    echo "PRoot binaries missing under ${proot_src}." >&2
    echo "Build them first: clients/android/scripts/build-mobile-linux-native.sh --variant ${VARIANT}" >&2
    exit 1
  fi

  rm -rf "${PAYLOAD}"
  mkdir -p "${PAYLOAD}/bin" "${PAYLOAD}/apks" "${PAYLOAD}/guest" "${PAYLOAD}/device"

  # PRoot + unbundled loader (locally built from the pinned fork; ELF-checked).
  cp "${proot_src}/libproot.so" "${PAYLOAD}/bin/proot"
  cp "${proot_src}/libproot-loader.so" "${PAYLOAD}/bin/loader"
  python3 - "${PAYLOAD}/bin/proot" "${PAYLOAD}/bin/loader" <<'PY'
import pathlib, sys
for path in map(pathlib.Path, sys.argv[1:]):
    header = path.read_bytes()[:20]
    if header[:4] != b"\x7fELF" or int.from_bytes(header[18:20], "little") != 183:
        raise SystemExit(f"{path} is not an aarch64 ELF")
print("proot/loader ELF check: ok (aarch64)")
PY

  # Rootfs archive (bytes identical to the pinned upstream archive).
  cp "${STAGING}/$(basename "$(pins_get rootfs.url)")" "${PAYLOAD}/"

  # apk closure.
  cp "${APKS_DIR}"/*.apk "${PAYLOAD}/apks/"

  # App payload: graft the pinned musl swc binding, prune foreign-platform
  # bindings, then tar with symlinks intact (adb push would flatten them).
  echo "   grafting @next/swc-linux-arm64-musl ${next_version}"
  local swc_file="${SWC_DIR}/swc-linux-arm64-musl-${next_version}.tgz"
  local graft="${APP_WORK}/node_modules/@next/swc-linux-arm64-musl"
  rm -rf "${graft}" "${STAGING}/swc-unpack"
  mkdir -p "${graft}" "${STAGING}/swc-unpack"
  tar -xzf "${swc_file}" -C "${STAGING}/swc-unpack"
  cp -R "${STAGING}/swc-unpack/package/." "${graft}/"
  rm -rf "${STAGING}/swc-unpack"
  local foreign
  for foreign in "${APP_WORK}/node_modules/@next"/swc-*; do
    [[ -e "${foreign}" ]] || continue
    if [[ "$(basename "${foreign}")" != "swc-linux-arm64-musl" ]]; then
      rm -rf "${foreign}"
    fi
  done

  rm -rf "${APP_WORK}/.next" "${APP_WORK}/out"
  echo "   packing app-payload.tar.gz (gnutar format for busybox/toybox tar)"
  COPYFILE_DISABLE=1 tar --format gnutar \
    --exclude '.DS_Store' --exclude '*/.DS_Store' \
    -czf "${PAYLOAD}/app-payload.tar.gz" -C "${APP_WORK}" .

  cp "${SPIKE_DIR}/scripts/guest/measure-spike.sh" "${PAYLOAD}/guest/"
  cp "${SPIKE_DIR}/scripts/device/run-in-guest.sh" "${PAYLOAD}/device/"

  python3 - "${PAYLOAD}" <<'PY'
import hashlib, json, pathlib, sys, time
payload = pathlib.Path(sys.argv[1])
entries = []
for path in sorted(p for p in payload.rglob("*") if p.is_file()):
    entries.append({
        "path": str(path.relative_to(payload)),
        "size_bytes": path.stat().st_size,
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
    })
manifest = {"schema_version": 1, "kit": "app-runtime-spike", "created_epoch_s": int(time.time()), "files": entries}
(payload / "payload-manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(f"payload-manifest.json: {len(entries)} files")
PY

  echo "== pack: done"
  du -sh "${PAYLOAD}"
}

case "${STEP}" in
  fetch) step_fetch ;;
  verify) step_verify ;;
  pack) step_pack ;;
  all) step_fetch; step_verify; step_pack ;;
esac
