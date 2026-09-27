#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
auth_dir="${repo_root}/docs/mobile-linux/authorization"
manifest_path="${auth_dir}/AUTHORIZATION_MANIFEST.json"
enabled="${LINGXI_MOBILE_LINUX_ENABLED:-0}"

if [[ "${enabled}" != "1" ]]; then
  echo "mobile-linux authorization gate skipped (LINGXI_MOBILE_LINUX_ENABLED!=1)"
  exit 0
fi

if [[ ! -f "${manifest_path}" || -L "${manifest_path}" ]]; then
  echo "missing authorization manifest: ${manifest_path}" >&2
  exit 1
fi

expected_manifest_sha="${LINGXI_MOBILE_LINUX_AUTHORIZATION_SHA256:-}"
if [[ ! "${expected_manifest_sha}" =~ ^[a-f0-9]{64}$ ]]; then
  echo "LINGXI_MOBILE_LINUX_AUTHORIZATION_SHA256 must pin the authorization manifest digest" >&2
  exit 1
fi

AUTH_DIR="${auth_dir}" MANIFEST_PATH="${manifest_path}" EXPECTED_MANIFEST_SHA="${expected_manifest_sha}" python3 - <<'PY'
import hashlib
import json
import os
import pathlib
import re
import sys

auth_dir = pathlib.Path(os.environ["AUTH_DIR"])
manifest_path = pathlib.Path(os.environ["MANIFEST_PATH"])
expected_manifest_sha = os.environ["EXPECTED_MANIFEST_SHA"]

actual_manifest_sha = hashlib.sha256(manifest_path.read_bytes()).hexdigest()
if actual_manifest_sha != expected_manifest_sha:
    print(
        f"authorization manifest hash mismatch: expected {expected_manifest_sha}, got {actual_manifest_sha}",
        file=sys.stderr,
    )
    sys.exit(1)

try:
    data = json.loads(manifest_path.read_text())
except Exception as exc:
    print(f"failed to parse authorization manifest: {exc}", file=sys.stderr)
    sys.exit(1)

if data.get("schema_version") != 1:
    print("authorization manifest schema_version must be 1", file=sys.stderr)
    sys.exit(1)
if data.get("status") != "approved":
    print("authorization manifest status must be approved", file=sys.stderr)
    sys.exit(1)

artifacts = data.get("artifacts")
if not isinstance(artifacts, list) or not artifacts:
    print("authorization manifest must contain non-empty artifacts[]", file=sys.stderr)
    sys.exit(1)

required_artifacts = {
    "proot": "proot-authorization.txt",
    "ish": "ish-authorization.txt",
    "alpine": "alpine-redistribution.txt",
}
required_ids = set(required_artifacts)
seen_ids = set()
sha_re = re.compile(r"^[a-f0-9]{64}$")

for entry in artifacts:
    if not isinstance(entry, dict):
        print("authorization manifest artifacts entries must be objects", file=sys.stderr)
        sys.exit(1)
    artifact_id = entry.get("id")
    rel_path = entry.get("path")
    sha256 = entry.get("sha256")
    if artifact_id in seen_ids:
        print(f"duplicate authorization artifact id: {artifact_id}", file=sys.stderr)
        sys.exit(1)
    if artifact_id not in required_artifacts:
        print(f"unexpected authorization artifact id: {artifact_id!r}", file=sys.stderr)
        sys.exit(1)
    seen_ids.add(artifact_id)
    if not isinstance(rel_path, str) or not rel_path:
        print(f"artifact {artifact_id!r} missing path", file=sys.stderr)
        sys.exit(1)
    if pathlib.PurePosixPath(rel_path).is_absolute() or ".." in pathlib.PurePosixPath(rel_path).parts:
        print(f"artifact {artifact_id!r} has invalid path {rel_path!r}", file=sys.stderr)
        sys.exit(1)
    if rel_path != required_artifacts[artifact_id]:
        print(
            f"artifact {artifact_id!r} must use path {required_artifacts[artifact_id]!r}",
            file=sys.stderr,
        )
        sys.exit(1)
    if not isinstance(sha256, str) or not sha_re.fullmatch(sha256):
        print(f"artifact {artifact_id!r} has invalid sha256", file=sys.stderr)
        sys.exit(1)

    file_path = auth_dir / rel_path
    if file_path.is_symlink() or not file_path.is_file():
        print(f"authorization artifact missing: {file_path}", file=sys.stderr)
        sys.exit(1)

    digest = hashlib.sha256(file_path.read_bytes()).hexdigest()
    if digest != sha256:
        print(
            f"authorization artifact hash mismatch for {file_path.name}: expected {sha256}, got {digest}",
            file=sys.stderr,
        )
        sys.exit(1)

missing = required_ids - seen_ids
if missing:
    print(f"authorization manifest missing required ids: {sorted(missing)}", file=sys.stderr)
    sys.exit(1)

print(f"authorization manifest verified: {manifest_path}")
PY
