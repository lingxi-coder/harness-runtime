#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
enabled="${LINGXI_MOBILE_LINUX_ENABLED:-0}"
sbom_dir="${MOBILE_LINUX_EVIDENCE_DIR:-}"
auth_manifest="${repo_root}/docs/mobile-linux/authorization/AUTHORIZATION_MANIFEST.json"
rootfs_manifest="${sbom_dir}/rootfs-manifest.json"
rootfs_lock="${sbom_dir}/rootfs-build.lock.json"
tool_path="${repo_root}/scripts/mobile-linux/rootfs_tool.py"

if [[ "${enabled}" != "1" ]]; then
  echo "mobile-linux SBOM/license gate skipped (LINGXI_MOBILE_LINUX_ENABLED!=1)"
  exit 0
fi

if [[ -z "${sbom_dir}" || -z "${MOBILE_LINUX_ROOTFS_ARCHIVE:-}" ]]; then
  echo "enabled release requires MOBILE_LINUX_EVIDENCE_DIR and MOBILE_LINUX_ROOTFS_ARCHIVE" >&2
  exit 1
fi
python3 "${script_dir}/sdk_delegate.py" verify-evidence.py "$@" \
  --evidence-dir "${sbom_dir}" --archive "${MOBILE_LINUX_ROOTFS_ARCHIVE}"

for required in \
  "${sbom_dir}/rootfs.spdx.json" \
  "${sbom_dir}/licenses.json" \
  "${sbom_dir}/executable-allowlist.json" \
  "${auth_manifest}" \
  "${rootfs_manifest}" \
  "${rootfs_lock}"
do
  if [[ ! -f "${required}" || -L "${required}" ]]; then
    echo "missing required mobile-linux release evidence: ${required}" >&2
    exit 1
  fi
done

SBOM_DIR="${sbom_dir}" ROOTFS_MANIFEST="${rootfs_manifest}" python3 - <<'PY'
import json
import os
import pathlib
import sys

sbom_dir = pathlib.Path(os.environ["SBOM_DIR"])
rootfs_path = pathlib.Path(os.environ["ROOTFS_MANIFEST"])

def load(path):
    try:
        return json.loads(path.read_text())
    except Exception as exc:
        print(f"invalid JSON evidence {path}: {exc}", file=sys.stderr)
        sys.exit(1)

rootfs = load(rootfs_path)
spdx = load(sbom_dir / "rootfs.spdx.json")
licenses = load(sbom_dir / "licenses.json")
allowlist_snapshot = load(sbom_dir / "executable-allowlist.json")

if not str(spdx.get("spdxVersion", "")).startswith("SPDX-2."):
    print("rootfs.spdx.json must be an SPDX 2.x document", file=sys.stderr)
    sys.exit(1)
spdx_packages = spdx.get("packages")
if not isinstance(spdx_packages, list) or not spdx_packages:
    print("rootfs.spdx.json must contain packages[]", file=sys.stderr)
    sys.exit(1)
spdx_names = {
    package.get("name")
    for package in spdx_packages
    if isinstance(package, dict) and isinstance(package.get("name"), str)
}
manifest_package_names = {
    package.get("name")
    for package in rootfs.get("packages", [])
    if isinstance(package, dict)
}
missing_from_sbom = manifest_package_names - spdx_names
if missing_from_sbom:
    print(f"rootfs packages missing from SPDX SBOM: {sorted(missing_from_sbom)}", file=sys.stderr)
    sys.exit(1)

if licenses.get("schema_version") != 1 or licenses.get("status") != "approved":
    print("licenses.json must use schema_version 1 and status approved", file=sys.stderr)
    sys.exit(1)
components = licenses.get("components")
if not isinstance(components, list) or not components:
    print("licenses.json must contain components[]", file=sys.stderr)
    sys.exit(1)
component_ids = set()
for component in components:
    if (
        not isinstance(component, dict)
        or not isinstance(component.get("id"), str)
        or not isinstance(component.get("license"), str)
        or not component["license"].strip()
    ):
        print("each license component needs non-empty id and license", file=sys.stderr)
        sys.exit(1)
    component_ids.add(component["id"])
missing_components = {"proot", "ish", "alpine-rootfs", "typescript-native"} - component_ids
if missing_components:
    print(f"license inventory missing components: {sorted(missing_components)}", file=sys.stderr)
    sys.exit(1)

if (
    not isinstance(allowlist_snapshot, dict)
    or allowlist_snapshot.get("schema_version") != 1
    or not isinstance(allowlist_snapshot.get("entries"), list)
):
    print("executable-allowlist.json must contain schema_version 1 and entries[]", file=sys.stderr)
    sys.exit(1)

def normalized(entries):
    return sorted(
        (
            entry.get("path"),
            entry.get("sha256"),
            entry.get("kind"),
            entry.get("size_bytes"),
        )
        for entry in entries
        if isinstance(entry, dict)
    )

manifest_entries = rootfs.get("executable_allowlist")
snapshot_entries = allowlist_snapshot["entries"]
if (
    not isinstance(manifest_entries, list)
    or len(normalized(manifest_entries)) != len(manifest_entries)
    or len(normalized(snapshot_entries)) != len(snapshot_entries)
    or normalized(manifest_entries) != normalized(snapshot_entries)
):
    print("executable allowlist snapshot does not match rootfs manifest", file=sys.stderr)
    sys.exit(1)

print("mobile-linux SBOM/license evidence verified")
PY

python3 "${tool_path}" "$@" validate-lock --lock "${rootfs_lock}" --manifest "${rootfs_manifest}"
