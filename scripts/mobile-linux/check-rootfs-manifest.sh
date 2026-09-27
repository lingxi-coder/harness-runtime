#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/../.." && pwd)"
schema_path="${repo_root}/docs/mobile-linux/rootfs/rootfs-manifest.schema.json"
sample_path="${repo_root}/docs/mobile-linux/rootfs/rootfs-manifest.sample.json"
target_path="${1:-${sample_path}}"
tool_path="${repo_root}/scripts/mobile-linux/rootfs_tool.py"
pins_path="${repo_root}/docs/mobile-linux/local-app-runtime-pins.json"
enabled="${LINGXI_MOBILE_LINUX_ENABLED:-0}"

if [[ ! -f "${schema_path}" ]]; then
  echo "missing schema: ${schema_path}" >&2
  exit 1
fi

if [[ ! -f "${target_path}" ]]; then
  echo "missing manifest: ${target_path}" >&2
  exit 1
fi

SCHEMA_PATH="${schema_path}" TARGET_PATH="${target_path}" PINS_PATH="${pins_path}" python3 - <<'PY'
import json
import os
import pathlib
import re
import sys

schema_path = pathlib.Path(os.environ["SCHEMA_PATH"])
target_path = pathlib.Path(os.environ["TARGET_PATH"])
pins_path = pathlib.Path(os.environ["PINS_PATH"])

try:
    schema = json.loads(schema_path.read_text())
    data = json.loads(target_path.read_text())
    # Package identities and the package-manager policy are read from the pins
    # rather than restated here; a second copy is what let this gate assert
    # Node 22.23.0 after the product had moved to 24.18.1.
    _pins = json.loads(pins_path.read_text())
except Exception as exc:
    print(f"failed to parse rootfs manifest input: {exc}", file=sys.stderr)
    sys.exit(1)

required = {
    "schema_version",
    "runtime",
    "platform",
    "abi",
    "rootfs_version",
    "content_sha256",
    "sbom_filename",
    "source_pins_filename",
    "archive",
    "packages",
    "executable_allowlist",
    "immutable_files",
    "writable_paths",
}
missing = required - data.keys()
if missing:
    print(f"missing required manifest keys: {sorted(missing)}", file=sys.stderr)
    sys.exit(1)
unknown = data.keys() - required
if unknown:
    print(f"unknown manifest keys: {sorted(unknown)}", file=sys.stderr)
    sys.exit(1)

if data["schema_version"] != 2:
    print("schema_version must be 2", file=sys.stderr)
    sys.exit(1)
if data["runtime"] not in {"android-proot", "ios-ish"}:
    print("runtime must be android-proot or ios-ish", file=sys.stderr)
    sys.exit(1)
if data["platform"] not in {"android", "ios"}:
    print("platform must be android or ios", file=sys.stderr)
    sys.exit(1)
if data["abi"] not in {"arm64", "x86_64"}:
    print("abi must be arm64 or x86_64", file=sys.stderr)
    sys.exit(1)
expected_platform = {"android-proot": "android", "ios-ish": "ios"}[data["runtime"]]
if data["platform"] != expected_platform:
    print("runtime and platform do not match", file=sys.stderr)
    sys.exit(1)
if data["platform"] == "ios" and data["abi"] != "arm64":
    print("iOS rootfs must use arm64", file=sys.stderr)
    sys.exit(1)
if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9._-]+)?", data["rootfs_version"]):
    print("rootfs_version must look like semver", file=sys.stderr)
    sys.exit(1)
if not re.fullmatch(r"[a-f0-9]{64}", str(data["content_sha256"])):
    print("content_sha256 must be 64 lowercase hex chars", file=sys.stderr)
    sys.exit(1)
if data["sbom_filename"] != "rootfs.spdx.json":
    print("sbom_filename must be rootfs.spdx.json", file=sys.stderr)
    sys.exit(1)
if data["source_pins_filename"] != "mobile-linux-pins.json":
    print("source_pins_filename must be mobile-linux-pins.json", file=sys.stderr)
    sys.exit(1)

archive = data["archive"]
if not isinstance(archive, dict):
    print("archive must be an object", file=sys.stderr)
    sys.exit(1)
if set(archive) != {"filename", "sha256", "size_bytes"}:
    print("archive must contain only filename, sha256, and size_bytes", file=sys.stderr)
    sys.exit(1)
if (
    not isinstance(archive.get("filename"), str)
    or not archive["filename"]
    or "/" in archive["filename"]
    or archive["filename"] in {".", ".."}
):
    print("archive.filename must be one safe path component", file=sys.stderr)
    sys.exit(1)
if not re.fullmatch(r"[a-f0-9]{64}", str(archive.get("sha256", ""))):
    print("archive.sha256 must be 64 lowercase hex chars", file=sys.stderr)
    sys.exit(1)
if not isinstance(archive.get("size_bytes"), int) or archive["size_bytes"] <= 0:
    print("archive.size_bytes must be a positive integer", file=sys.stderr)
    sys.exit(1)

packages = data["packages"]
if not isinstance(packages, list) or not packages:
    print("packages must be a non-empty list", file=sys.stderr)
    sys.exit(1)
package_names = set()
for package in packages:
    if (
        not isinstance(package, dict)
        or set(package) != {"name", "version", "license", "architecture", "origin"}
        or any(
            not isinstance(package.get(field), str) or not package[field]
            for field in {"name", "version", "license", "architecture", "origin"}
        )
    ):
        print("each package must include complete package identity", file=sys.stderr)
        sys.exit(1)
    if package["name"] in package_names:
        print(f"duplicate package: {package['name']}", file=sys.stderr)
        sys.exit(1)
    package_names.add(package["name"])

# `openssh-client` was never a real Alpine package (the client split ships as
# `openssh-client-default`), so this set could not have matched a real rootfs.
required_packages = {"apk-tools", "busybox", "typescript-native", *_pins["runtime_packages"]}
missing_packages = required_packages - package_names
if missing_packages:
    print(f"fixed toolset packages missing: {sorted(missing_packages)}", file=sys.stderr)
    sys.exit(1)
# npm and py3-pip are shipped packages now; only the alternative managers
# stay out. Read the policy from the pins so it cannot drift.
forbidden_packages = set(_pins["forbidden_packages"]) & package_names
if forbidden_packages:
    print(f"forbidden package-manager packages present: {sorted(forbidden_packages)}", file=sys.stderr)
    sys.exit(1)
package_versions = {package["name"]: package["version"] for package in packages}
fixed_versions = dict(_pins["runtime_packages"])
fixed_versions["typescript-native"] = _pins["typescript_native"]["version"]
version_drift = {
    name: {"expected": expected, "actual": package_versions.get(name)}
    for name, expected in fixed_versions.items()
    if package_versions.get(name) != expected
}
if version_drift:
    print(f"fixed runtime package versions diverged: {version_drift}", file=sys.stderr)
    sys.exit(1)

allowlist = data["executable_allowlist"]
if not isinstance(allowlist, list) or not allowlist:
    print("executable_allowlist must be a non-empty list", file=sys.stderr)
    sys.exit(1)
allowlist_paths = set()
for entry in allowlist:
    if not isinstance(entry, dict):
        print("allowlist entries must be objects", file=sys.stderr)
        sys.exit(1)
    if set(entry) - {"path", "sha256", "kind", "size_bytes"}:
        print("allowlist entry contains unknown keys", file=sys.stderr)
        sys.exit(1)
    path = entry.get("path")
    sha = entry.get("sha256")
    kind = entry.get("kind")
    parts = path.split("/") if isinstance(path, str) else []
    if (
        not isinstance(path, str)
        or not path.startswith("/")
        or path == "/"
        or any(part in {"", ".", ".."} for part in parts[1:])
    ):
        print(f"invalid allowlist path: {path!r}", file=sys.stderr)
        sys.exit(1)
    if path in allowlist_paths:
        print(f"duplicate allowlist path: {path}", file=sys.stderr)
        sys.exit(1)
    allowlist_paths.add(path)
    if not re.fullmatch(r"[a-f0-9]{64}", str(sha or "")):
        print(f"invalid sha256 for allowlist entry: {path!r}", file=sys.stderr)
        sys.exit(1)
    if kind not in {"elf", "shared-library", "interpreter"}:
        print(f"invalid allowlist kind for {path!r}: {kind!r}", file=sys.stderr)
        sys.exit(1)
    if "size_bytes" in entry and (
        not isinstance(entry["size_bytes"], int) or entry["size_bytes"] < 0
    ):
        print(f"invalid size_bytes for allowlist entry: {path!r}", file=sys.stderr)
        sys.exit(1)

required_allowlist_paths = {
    "/bin/busybox",
    "/bin/sh",
    "/usr/bin/git",
    "/usr/bin/node",
    "/usr/bin/ssh",
    "/usr/bin/python3",
}
missing_allowlist = sorted(required_allowlist_paths - allowlist_paths)
if missing_allowlist:
    print(f"required allowlist paths missing: {missing_allowlist}", file=sys.stderr)
    sys.exit(1)

immutable_files = data["immutable_files"]
if not isinstance(immutable_files, list) or not immutable_files:
    print("immutable_files must be a non-empty list", file=sys.stderr)
    sys.exit(1)
immutable_by_path = {}
for entry in immutable_files:
    if not isinstance(entry, dict) or set(entry) != {
        "path",
        "sha256",
        "kind",
        "size_bytes",
    }:
        print("immutable file entries must contain path, sha256, kind, and size_bytes", file=sys.stderr)
        sys.exit(1)
    path = entry.get("path")
    parts = path.split("/") if isinstance(path, str) else []
    if (
        not isinstance(path, str)
        or not path.startswith("/")
        or path == "/"
        or any(part in {"", ".", ".."} for part in parts[1:])
    ):
        print(f"invalid immutable file path: {path!r}", file=sys.stderr)
        sys.exit(1)
    if path in immutable_by_path:
        print(f"duplicate immutable file path: {path}", file=sys.stderr)
        sys.exit(1)
    if any(path == root or path.startswith(root + "/") for root in {
        "/root",
        "/tmp",
        "/var/tmp",
        "/workspace",
    }):
        print(f"writable path is forbidden in immutable inventory: {path}", file=sys.stderr)
        sys.exit(1)
    if not re.fullmatch(r"[a-f0-9]{64}", str(entry.get("sha256", ""))):
        print(f"invalid immutable file sha256: {path}", file=sys.stderr)
        sys.exit(1)
    if entry.get("kind") not in {"regular-file", "symlink"}:
        print(f"invalid immutable file kind: {path}", file=sys.stderr)
        sys.exit(1)
    if not isinstance(entry.get("size_bytes"), int) or entry["size_bytes"] < 0:
        print(f"invalid immutable file size: {path}", file=sys.stderr)
        sys.exit(1)
    immutable_by_path[path] = entry

for executable in allowlist:
    immutable = immutable_by_path.get(executable["path"])
    if (
        immutable is None
        or immutable["kind"] != "regular-file"
        or immutable["sha256"] != executable["sha256"]
        or (
            "size_bytes" in executable
            and immutable["size_bytes"] != executable["size_bytes"]
        )
    ):
        print(
            f"executable allowlist entry is not bound to immutable inventory: {executable['path']}",
            file=sys.stderr,
        )
        sys.exit(1)

writable_paths = data["writable_paths"]
if not isinstance(writable_paths, list) or not writable_paths:
    print("writable_paths must be a non-empty list", file=sys.stderr)
    sys.exit(1)

allowed_writable_roots = {"/root", "/tmp", "/var/tmp", "/workspace"}
if len(writable_paths) != len(set(writable_paths)):
    print("writable_paths contains duplicates", file=sys.stderr)
    sys.exit(1)
if set(writable_paths) != allowed_writable_roots:
    print(
        "writable_paths must be exactly /root, /tmp, /var/tmp, and /workspace",
        file=sys.stderr,
    )
    sys.exit(1)
for executable in allowlist_paths:
    if any(executable == root or executable.startswith(root + "/") for root in allowed_writable_roots):
        print(f"executable path is writable and therefore forbidden: {executable}", file=sys.stderr)
        sys.exit(1)

schema_required = set(schema.get("required", []))
if required != schema_required:
    print("schema file and validator required keys diverged", file=sys.stderr)
    sys.exit(1)

print(f"rootfs manifest validated: {target_path}")
PY

if [[ "${enabled}" == "1" && "${target_path}" != "${sample_path}" ]]; then
  archive_path="$(python3 - <<'PY' "${target_path}"
import json
import pathlib
import sys
manifest = json.loads(pathlib.Path(sys.argv[1]).read_text())
print(pathlib.Path(sys.argv[1]).parent / manifest["archive"]["filename"])
PY
)"
  lock_path="$(dirname "${target_path}")/rootfs-build.lock.json"
  if [[ ! -f "${archive_path}" || -L "${archive_path}" ]]; then
    echo "missing current rootfs archive: ${archive_path}" >&2
    exit 1
  fi
  if [[ ! -f "${lock_path}" || -L "${lock_path}" ]]; then
    echo "missing current rootfs lock file: ${lock_path}" >&2
    exit 1
  fi
  python3 "${tool_path}" verify-archive --archive "${archive_path}"
  python3 "${tool_path}" validate-lock --lock "${lock_path}" --manifest "${target_path}"
fi
