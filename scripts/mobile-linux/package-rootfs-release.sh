#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tool="${script_dir}/rootfs_tool.py"

if [[ $# -ne 7 ]]; then
  echo "usage: $0 <rootfs-dir> <runtime> <platform> <abi> <rootfs-version> <archive-output> <evidence-dir>" >&2
  exit 1
fi

rootfs_dir="$1"
runtime="$2"
platform="$3"
abi="$4"
rootfs_version="$5"
archive_output="$6"
evidence_dir="$7"

if [[ ! -d "${rootfs_dir}" ]]; then
  echo "rootfs directory not found: ${rootfs_dir}" >&2
  exit 1
fi

if ! command -v python3 >/dev/null 2>&1; then
  echo "python3 is required" >&2
  exit 1
fi

if ! command -v gzip >/dev/null 2>&1 || ! command -v tar >/dev/null 2>&1; then
  echo "gzip and tar are required to package a rootfs release" >&2
  exit 1
fi

mkdir -p "${evidence_dir}"
mkdir -p "$(dirname "${archive_output}")"

python3 "${tool}" verify-tree --root "${rootfs_dir}"

archive_filename="$(basename "${archive_output}")"
archive_parent="$(cd "$(dirname "${archive_output}")" && pwd)"
rm -f "${archive_output}"

tmp_tar_seed="$(mktemp "${archive_parent}/.${archive_filename}.XXXXXX")"
tmp_tar="${tmp_tar_seed}.tar"
mv "${tmp_tar_seed}" "${tmp_tar}"
cleanup() {
  [[ -f "${tmp_tar}" ]] && rm -f "${tmp_tar}"
}
trap cleanup EXIT

python3 "${tool}" build-archive \
  --root "${rootfs_dir}" \
  --output "${tmp_tar}" \
  --source-date-epoch "${SOURCE_DATE_EPOCH:-0}"
python3 "${tool}" verify-archive --archive "${tmp_tar}"
gzip -n -9 -c "${tmp_tar}" > "${archive_output}"
cleanup
trap - EXIT

archive_sha="$(python3 - <<'PY' "${archive_output}"
import hashlib, pathlib, sys
path = pathlib.Path(sys.argv[1])
print(hashlib.sha256(path.read_bytes()).hexdigest())
PY
)"
archive_size="$(python3 - <<'PY' "${archive_output}"
import pathlib, sys
print(pathlib.Path(sys.argv[1]).stat().st_size)
PY
)"

manifest_path="${evidence_dir}/rootfs-manifest.json"
lock_path="${evidence_dir}/rootfs-build.lock.json"
spdx_path="${evidence_dir}/rootfs.spdx.json"
allowlist_path="${evidence_dir}/executable-allowlist.json"

python3 "${tool}" generate-lock \
  --root "${rootfs_dir}" \
  --output "${lock_path}"

python3 "${tool}" generate-manifest \
  --root "${rootfs_dir}" \
  --runtime "${runtime}" \
  --platform "${platform}" \
  --abi "${abi}" \
  --rootfs-version "${rootfs_version}" \
  --archive-filename "${archive_filename}" \
  --archive-sha256 "${archive_sha}" \
  --archive-size "${archive_size}" \
  --output "${manifest_path}"

python3 "${tool}" generate-spdx \
  --root "${rootfs_dir}" \
  --name "${runtime}-${platform}-${abi}-${rootfs_version}" \
  --source-date-epoch "${SOURCE_DATE_EPOCH:-0}" \
  --output "${spdx_path}"

python3 "${tool}" snapshot-allowlist \
  --manifest "${manifest_path}" \
  --output "${allowlist_path}"

python3 "${tool}" validate-lock \
  --lock "${lock_path}" \
  --manifest "${manifest_path}"

echo "rootfs release packaged:"
echo "  archive: ${archive_output}"
echo "  manifest: ${manifest_path}"
echo "  lock: ${lock_path}"
echo "  spdx: ${spdx_path}"
echo "  allowlist: ${allowlist_path}"
