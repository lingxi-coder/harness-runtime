#!/usr/bin/env python3
import argparse
import datetime as dt
import hashlib
import json
import os
import pathlib
import stat
import subprocess
import sys
import tarfile
import tempfile
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Dict, List, Tuple

# Versions come from the pins so toolchain updates and validation stay aligned.
_PINS_PATH = (
    pathlib.Path(__file__).resolve().parents[2]
    / "docs"
    / "mobile-linux"
    / "local-app-runtime-pins.json"
)
_PINS = json.loads(_PINS_PATH.read_text(encoding="utf-8"))
_ALPINE = _PINS["alpine"]
_TYPESCRIPT_NATIVE = _PINS["typescript_native"]

# Packages the minirootfs already provides, plus everything the pins install.
# `openssh-client` is not a real Alpine package — the client split is shipped as
# `openssh-client-default`, so the old entry could never have matched.
FIXED_PRIMARY_PACKAGES = sorted(
    {"apk-tools", "busybox", *_PINS["runtime_packages"]}
)

FIXED_PACKAGE_VERSIONS = dict(_PINS["runtime_packages"])

# npm, npx and pip3 are now part of the shipped developer environment, so they
# are no longer contraband; only the alternative package managers are. Keeping
# them listed here would have made a correctly-built rootfs fail verification.
FORBIDDEN_PACKAGE_MANAGER_PATHS = [
    "/usr/bin/corepack",
    "/usr/bin/yarn",
]

# Read from the pins so this cannot drift from the closure policy. npm and
# py3-pip used to be listed here while also being required packages, which no
# correctly-built rootfs could ever satisfy.
FORBIDDEN_PACKAGE_NAMES = set(_PINS["forbidden_packages"])

REQUIRED_INTERACTIVE_PACKAGE_MANAGER_PATHS = [
    "/sbin/apk",
    "/etc/apk/repositories",
]

# Directories whose symlinks get the extra scrutiny below. A link here is what
# an agent actually executes, so it must be provably safe -- but it may not be
# banned outright: Alpine ships npm, npx and git's helper commands
# (git-receive-pack, git-upload-pack, git-upload-archive) as relative symlinks,
# so a blanket ban cannot describe any real Alpine rootfs.
BINARY_SYMLINK_SCRUTINY_PREFIXES = [
    "/bin/",
    "/sbin/",
    "/usr/bin/",
    "/usr/sbin/",
]

WORLD_WRITABLE_ALLOWED = {"/tmp", "/var/tmp"}
WRITABLE_ROOTS = {"/root", "/tmp", "/var/tmp", "/workspace"}
DEFAULT_SOURCE_DATE_EPOCH = 0
EXPECTED_ALPINE_RELEASE = _ALPINE["version"]


@dataclass
class PackageRecord:
    name: str
    version: str
    license: str
    architecture: str
    origin: str


def fail(message: str) -> "None":
    print(message, file=sys.stderr)
    raise SystemExit(1)


def root_rel(path: pathlib.Path, root: pathlib.Path) -> str:
    rel = path.relative_to(root).as_posix()
    return "/" if rel == "." else f"/{rel}"


def safe_member_path(name: str) -> pathlib.PurePosixPath:
    pure = pathlib.PurePosixPath(name)
    if pure.parts and pure.parts[0] == ".":
        pure = pathlib.PurePosixPath(*pure.parts[1:])
    if pure.is_absolute():
        fail(f"archive entry must not be absolute: {name}")
    if any(part in {"", ".", ".."} for part in pure.parts):
        fail(f"archive entry contains unsafe path components: {name}")
    return pure


def resolve_symlink_target(member_name: str, linkname: str) -> pathlib.PurePosixPath:
    if pathlib.PurePosixPath(linkname).is_absolute():
        fail(f"archive link target must be relative: {member_name} -> {linkname}")
    member_parent = pathlib.PurePosixPath(member_name).parent
    resolved = member_parent.joinpath(linkname)
    normalized_parts: List[str] = []
    for part in resolved.parts:
        if part in {"", "."}:
            continue
        if part == "..":
            if not normalized_parts:
                fail(f"archive link escapes root: {member_name} -> {linkname}")
            normalized_parts.pop()
            continue
        normalized_parts.append(part)
    if not normalized_parts:
        fail(f"archive link resolves to root: {member_name} -> {linkname}")
    return pathlib.PurePosixPath(*normalized_parts)


def validate_binary_symlink(
    root: pathlib.Path,
    rel: str,
    target: str,
    resolved: pathlib.PurePosixPath,
) -> None:
    """Extra checks for a symlink living in an executable directory.

    `resolve_symlink_target` has already rejected absolute targets and any that
    climb out of the rootfs. What remains is the property that only matters for
    something the guest will execute: it must actually resolve to a real file
    inside this rootfs. npm, npx and git's helper commands are legitimate
    relative symlinks, so they are allowed -- a dangling one is not, because it
    is a command that exists in PATH and fails at exec time.
    """
    if not any(rel.startswith(prefix) for prefix in BINARY_SYMLINK_SCRUTINY_PREFIXES):
        return

    # Resolve the LINK, not its lexically-normalised target. The two agree only
    # when every component of the target is a real directory; once the target
    # traverses a symlink followed by `..` they diverge, and attesting to the
    # lexical result vouches for a different file than the one the guest execs.
    real = pathlib.Path(os.path.realpath(root / rel.lstrip("/")))
    root_real = pathlib.Path(os.path.realpath(root))
    try:
        guest_rel = "/" + real.relative_to(root_real).as_posix()
    except ValueError:
        fail(f"symlink in a binary directory escapes the rootfs: {rel} -> {target}")
    if not real.exists():
        fail(f"symlink in a binary directory is dangling: {rel} -> {target}")
    if real.is_dir():
        fail(f"symlink in a binary directory must resolve to a file: {rel} -> {target}")
    # The blanket ban this replaced made it impossible for anything on PATH to
    # live in a guest-writable directory. Keep that property: an executable the
    # guest can rewrite is not covered by the immutable-file inventory, so a
    # /usr/bin entry resolving into one is uninventoried by construction.
    for writable in WRITABLE_ROOTS:
        if guest_rel == writable or guest_rel.startswith(f"{writable}/"):
            fail(
                f"symlink in a binary directory resolves into a writable root: "
                f"{rel} -> {guest_rel}"
            )


def normalize_hardlink_target(member_name: str, linkname: str) -> pathlib.PurePosixPath:
    if pathlib.PurePosixPath(linkname).is_absolute():
        fail(f"archive hardlink target must not be absolute: {member_name} -> {linkname}")
    return safe_member_path(linkname)


def read_sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(1024 * 1024)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def read_sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def stable_json_dumps(data: object) -> str:
    return json.dumps(data, indent=2, sort_keys=False) + "\n"


def canonical_json_bytes(data: object) -> bytes:
    return json.dumps(
        data,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
    ).encode("utf-8")


def source_date_epoch(value: int | None) -> int:
    if value is not None:
        return value
    env_value = os.environ.get("SOURCE_DATE_EPOCH")
    if env_value is None or env_value == "":
        return DEFAULT_SOURCE_DATE_EPOCH
    try:
        return int(env_value)
    except ValueError as exc:
        fail(f"SOURCE_DATE_EPOCH must be an integer: {exc}")


def format_created_timestamp(epoch: int) -> str:
    return dt.datetime.fromtimestamp(epoch, tz=dt.timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z")


def normalized_tar_mode(mode: int, *, is_dir: bool, is_symlink: bool) -> int:
    if is_symlink:
        return 0o777
    permission_bits = stat.S_IMODE(mode)
    if is_dir:
        if permission_bits & stat.S_IWOTH:
            return 0o1777 if permission_bits & stat.S_ISVTX else 0o777
        return 0o755
    if permission_bits & (stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH):
        return 0o755
    return 0o644


def parse_apk_installed(installed_path: pathlib.Path) -> List[PackageRecord]:
    if not installed_path.is_file():
        fail(f"missing APK installed database: {installed_path}")
    packages: List[PackageRecord] = []
    fields: Dict[str, str] = {}
    for line in installed_path.read_text(encoding="utf-8").splitlines():
        if not line:
            if fields:
                packages.append(
                    PackageRecord(
                        name=fields.get("P", ""),
                        version=fields.get("V", ""),
                        license=fields.get("L", "NOASSERTION") or "NOASSERTION",
                        architecture=fields.get("A", "unknown") or "unknown",
                        origin=fields.get("o", fields.get("P", "")) or fields.get("P", ""),
                    )
                )
                fields = {}
            continue
        if ":" not in line:
            continue
        key, value = line.split(":", 1)
        fields[key] = value
    if fields:
        packages.append(
            PackageRecord(
                name=fields.get("P", ""),
                version=fields.get("V", ""),
                license=fields.get("L", "NOASSERTION") or "NOASSERTION",
                architecture=fields.get("A", "unknown") or "unknown",
                origin=fields.get("o", fields.get("P", "")) or fields.get("P", ""),
            )
        )
    if not packages:
        fail(f"no packages found in APK installed database: {installed_path}")
    names = {package.name for package in packages}
    missing = sorted(set(FIXED_PRIMARY_PACKAGES) - names)
    if missing:
        fail(f"fixed primary packages missing from APK installed database: {missing}")
    forbidden = sorted(names & FORBIDDEN_PACKAGE_NAMES)
    if forbidden:
        fail(f"forbidden package-manager packages present in rootfs: {forbidden}")
    versions = {package.name: package.version for package in packages}
    mismatches = {
        name: {"expected": expected, "actual": versions.get(name)}
        for name, expected in FIXED_PACKAGE_VERSIONS.items()
        if versions.get(name) != expected
    }
    if mismatches:
        fail(f"fixed runtime package versions diverged: {mismatches}")
    return sorted(packages, key=lambda package: package.name)


def validate_typescript_native(root: pathlib.Path) -> PackageRecord:
    install_root = _TYPESCRIPT_NATIVE.get("install_root")
    version = _TYPESCRIPT_NATIVE.get("version")
    license_id = _TYPESCRIPT_NATIVE.get("license")
    packages = _TYPESCRIPT_NATIVE.get("packages")
    if (
        install_root != f"/opt/lingxi/toolchains/typescript/{version}"
        or not isinstance(version, str)
        or not isinstance(license_id, str)
        or not isinstance(packages, dict)
    ):
        fail("invalid native TypeScript toolchain pin")

    toolchain_root = root / install_root.lstrip("/")
    metadata_path = toolchain_root / "package.json"
    tsc_path = toolchain_root / "tsc"
    required_files = [
        metadata_path,
        tsc_path,
        toolchain_root / "LICENSE",
        toolchain_root / "NOTICE.txt",
        toolchain_root / "lib.d.ts",
    ]
    for required in required_files:
        if not required.is_file() or required.is_symlink():
            fail(f"native TypeScript toolchain file missing or unsafe: {root_rel(required, root)}")
    if not is_elf(tsc_path) or not os.access(tsc_path, os.X_OK):
        fail("native TypeScript tsc must be a real executable ELF")

    try:
        metadata = json.loads(metadata_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        fail(f"invalid native TypeScript package metadata: {exc}")
    matching = [
        (arch, package)
        for arch, package in packages.items()
        if isinstance(package, dict) and package.get("name") == metadata.get("name")
    ]
    if len(matching) != 1:
        fail("native TypeScript package does not match exactly one pinned architecture")
    architecture, package = matching[0]
    if elf_architecture(tsc_path) != architecture or elf_architecture(root / "bin/busybox") != architecture:
        fail("native TypeScript architecture diverged from its pin or guest")
    if metadata.get("version") != version or metadata.get("license") != license_id:
        fail("native TypeScript package metadata diverged from its version/license pin")
    expected_tsc_sha256 = package.get("tsc_sha256")
    if os.environ.get("ROOTFS_TOOL_TESTING") == "1":
        expected_tsc_sha256 = os.environ.get("ROOTFS_TOOL_TEST_TSC_SHA256")
    if read_sha256(tsc_path) != expected_tsc_sha256:
        fail("native TypeScript tsc diverged from its executable pin")

    return PackageRecord(
        name="typescript-native",
        version=version,
        license=license_id,
        architecture=architecture,
        origin=package["name"],
    )


def validate_source_toolchains(root: pathlib.Path) -> List[PackageRecord]:
    node_pin = _PINS["node_source"]
    metadata_path = root / "opt/lingxi/toolchains/node/provenance.json"
    binary = root / "usr/bin/node"
    if metadata_path.is_symlink() or not metadata_path.is_file():
        fail("source Node provenance is missing or unsafe")
    try:
        metadata = json.loads(metadata_path.read_text())
    except (OSError, json.JSONDecodeError) as exc:
        fail(f"invalid source Node provenance: {exc}")
    if any(metadata.get(key) != value for key, value in node_pin.items()):
        fail("source Node provenance diverged from pins")
    if binary.is_symlink() or not is_elf(binary) or not os.access(binary, os.X_OK):
        fail("source Node must be a real executable ELF")
    if metadata.get("binary_sha256") != read_sha256(binary):
        fail("source Node binary diverged from build provenance")
    architecture = metadata.get("architecture")
    if architecture not in ("aarch64", "x86_64"):
        fail("invalid source Node architecture")
    if elf_architecture(binary) != architecture or elf_architecture(root / "bin/busybox") != architecture:
        fail("source Node architecture diverged from provenance or guest")
    native_pnpm = root / "usr/lib/node_modules/pnpm/pnpm"
    if native_pnpm.is_symlink() or not os.access(native_pnpm, os.X_OK) or elf_architecture(native_pnpm) != architecture:
        fail("native pnpm architecture diverged from guest")
    records = [PackageRecord("node-source", node_pin["version"], node_pin["license"], architecture, node_pin["url"])]
    for name in ("npm", "pnpm"):
        manifest_path = root / "usr/lib/node_modules" / name / "package.json"
        if manifest_path.is_symlink() or not manifest_path.is_file():
            fail(f"{name} manifest is missing or unsafe")
        manifest = json.loads(manifest_path.read_text())
        if manifest.get("name") != name or manifest.get("version") != _PINS[name]["version"]:
            fail(f"{name} package metadata diverged from pins")
        records.append(PackageRecord(name, manifest["version"], manifest["license"], architecture, _PINS[name]["url"]))
    return records


def elf_architecture(path: pathlib.Path) -> str | None:
    """Read the ELF64 machine field; package metadata alone cannot prove ISA."""
    try:
        with path.open("rb") as handle:
            header = handle.read(20)
    except OSError:
        return None
    if len(header) != 20 or header[:4] != b"\x7fELF" or header[4] != 2 or header[5] not in (1, 2):
        return None
    machine = int.from_bytes(header[18:20], "little" if header[5] == 1 else "big")
    return {183: "aarch64", 62: "x86_64"}.get(machine)


def is_elf(path: pathlib.Path) -> bool:
    try:
        with path.open("rb") as handle:
            return handle.read(4) == b"\x7fELF"
    except OSError as exc:
        fail(f"failed to read file signature {path}: {exc}")


def classify_elf(path_rel: str) -> str:
    basename = pathlib.PurePosixPath(path_rel).name
    if ".so" in basename:
        return "shared-library"
    if path_rel in {"/bin/busybox", "/bin/sh", "/usr/bin/python3"}:
        return "interpreter"
    return "elf"


def validate_rootfs_tree(root: pathlib.Path) -> List[PackageRecord]:
    if not root.is_dir():
        fail(f"rootfs directory not found: {root}")

    release_path = root / "etc" / "alpine-release"
    if not release_path.is_file() or release_path.is_symlink():
        fail("rootfs must contain a real /etc/alpine-release file")
    actual_release = release_path.read_text(encoding="utf-8").strip()
    if actual_release != EXPECTED_ALPINE_RELEASE:
        fail(
            "Alpine release diverged: "
            f"expected {EXPECTED_ALPINE_RELEASE}, got {actual_release or '<empty>'}"
        )

    busybox = root / "bin" / "busybox"
    sh_path = root / "bin" / "sh"
    if not busybox.is_file() or busybox.is_symlink():
        fail("rootfs must contain a real /bin/busybox file")
    if not sh_path.exists():
        fail("rootfs must contain /bin/sh")
    # /bin/sh must BE busybox, by either link kind.
    #
    # Symlinks are what Alpine ships (busybox-binsh) and what we keep, because
    # fakefsify does not preserve hardlinks: it materialises each one as a full
    # copy, so hardlinking ~305 applets to a 919KB busybox added ~280MB to the
    # shipped rootfs for no benefit. Safety comes from validate_binary_symlink
    # above -- relative, inside the rootfs, non-dangling.
    if sh_path.is_symlink():
        resolved = resolve_symlink_target("bin/sh", os.readlink(sh_path))
        if (root / resolved).resolve() != busybox.resolve():
            fail("/bin/sh must resolve to /bin/busybox")
    elif os.stat(busybox).st_ino != os.stat(sh_path).st_ino:
        fail("/bin/sh must be a hardlink or a symlink to /bin/busybox")

    for forbidden in FORBIDDEN_PACKAGE_MANAGER_PATHS:
        candidate = root / forbidden.lstrip("/")
        if candidate.exists() or candidate.is_symlink():
            fail(f"forbidden package-manager artifact present in rootfs: {forbidden}")
    for required in REQUIRED_INTERACTIVE_PACKAGE_MANAGER_PATHS:
        candidate = root / required.lstrip("/")
        if not candidate.exists() or candidate.is_symlink():
            fail(f"interactive package-manager artifact missing or symlinked: {required}")

    for current_root, dirnames, filenames in os.walk(root, topdown=True, followlinks=False):
        current_dir = pathlib.Path(current_root)
        dirnames.sort()
        filenames.sort()

        for dirname in list(dirnames):
            path = current_dir / dirname
            rel = root_rel(path, root)
            mode = os.lstat(path).st_mode
            if stat.S_ISLNK(mode):
                target = os.readlink(path)
                resolved = resolve_symlink_target(rel.lstrip("/"), target)
                validate_binary_symlink(root, rel, target, resolved)
                dirnames.remove(dirname)
                continue
            if stat.S_ISSOCK(mode) or stat.S_ISCHR(mode) or stat.S_ISBLK(mode) or stat.S_ISFIFO(mode):
                fail(f"forbidden special file in rootfs: {rel}")
            if mode & stat.S_ISUID or mode & stat.S_ISGID:
                fail(f"suid/sgid bits are forbidden in rootfs directories: {rel}")
            if mode & stat.S_IWOTH and rel not in WORLD_WRITABLE_ALLOWED:
                fail(f"world-writable directory is forbidden outside scratch paths: {rel}")

        for filename in filenames:
            path = current_dir / filename
            rel = root_rel(path, root)
            mode = os.lstat(path).st_mode
            if stat.S_ISLNK(mode):
                target = os.readlink(path)
                resolved = resolve_symlink_target(rel.lstrip("/"), target)
                validate_binary_symlink(root, rel, target, resolved)
                continue
            if not stat.S_ISREG(mode):
                fail(f"non-regular rootfs file is forbidden: {rel}")
            if mode & stat.S_ISUID or mode & stat.S_ISGID:
                fail(f"suid/sgid bits are forbidden in rootfs files: {rel}")
            if mode & stat.S_IWOTH and rel not in WORLD_WRITABLE_ALLOWED:
                fail(f"world-writable file is forbidden outside scratch paths: {rel}")

    packages = parse_apk_installed(root / "lib" / "apk" / "db" / "installed")
    packages.append(validate_typescript_native(root))
    packages.extend(validate_source_toolchains(root))
    return sorted(packages, key=lambda package: package.name)


def collect_allowlist(root: pathlib.Path) -> List[dict]:
    entries: List[dict] = []
    seen: set[str] = set()
    for current_root, _, filenames in os.walk(root, topdown=True, followlinks=False):
        current_dir = pathlib.Path(current_root)
        filenames.sort()
        for filename in filenames:
            path = current_dir / filename
            rel = root_rel(path, root)
            if path.is_symlink():
                continue
            mode = os.stat(path).st_mode
            if not stat.S_ISREG(mode):
                continue
            if not is_elf(path):
                continue
            if not ((mode & stat.S_IXUSR) or ".so" in pathlib.PurePosixPath(rel).name):
                continue
            if rel in seen:
                continue
            seen.add(rel)
            entries.append(
                {
                    "path": rel,
                    "sha256": read_sha256(path),
                    "kind": classify_elf(rel),
                    "size_bytes": path.stat().st_size,
                }
            )
    required = {
        "/bin/busybox",
        "/bin/sh",
        "/sbin/apk",
        "/usr/bin/git",
        "/usr/bin/node",
        "/usr/bin/ssh",
        "/usr/bin/python3",
        f"{_TYPESCRIPT_NATIVE['install_root']}/tsc",
    }
    present = {entry["path"] for entry in entries}
    missing = sorted(required - present)
    if missing:
        fail(f"required ELF/interpreter paths missing from rootfs allowlist: {missing}")
    return sorted(entries, key=lambda entry: entry["path"])


def is_writable_inventory_path(path: str) -> bool:
    return any(path == root or path.startswith(root + "/") for root in WRITABLE_ROOTS)


def immutable_entry(path: pathlib.Path, root: pathlib.Path) -> dict:
    rel = root_rel(path, root)
    st = os.lstat(path)
    if stat.S_ISLNK(st.st_mode):
        target = os.readlink(path)
        try:
            payload = target.encode("utf-8")
        except UnicodeEncodeError:
            fail(f"rootfs symlink target must be UTF-8: {rel}")
        return {
            "path": rel,
            "sha256": read_sha256_bytes(payload),
            "kind": "symlink",
            "size_bytes": len(payload),
        }
    if not stat.S_ISREG(st.st_mode):
        fail(f"immutable inventory supports only regular files and symlinks: {rel}")
    return {
        "path": rel,
        "sha256": read_sha256(path),
        "kind": "regular-file",
        "size_bytes": st.st_size,
    }


def collect_immutable_files(root: pathlib.Path) -> List[dict]:
    entries: List[dict] = []
    for current_root, dirnames, filenames in os.walk(root, topdown=True, followlinks=False):
        current_dir = pathlib.Path(current_root)
        dirnames.sort()
        filenames.sort()

        for dirname in list(dirnames):
            path = current_dir / dirname
            rel = root_rel(path, root)
            if is_writable_inventory_path(rel):
                dirnames.remove(dirname)
                continue
            if path.is_symlink():
                entries.append(immutable_entry(path, root))
                dirnames.remove(dirname)

        for filename in filenames:
            path = current_dir / filename
            rel = root_rel(path, root)
            if not is_writable_inventory_path(rel):
                entries.append(immutable_entry(path, root))

    return sorted(entries, key=lambda entry: entry["path"])


def generate_manifest(args: argparse.Namespace) -> None:
    root = pathlib.Path(args.root).resolve()
    packages = validate_rootfs_tree(root)
    allowlist = collect_allowlist(root)
    immutable_files = collect_immutable_files(root)
    manifest = {
        "schema_version": 2,
        "runtime": args.runtime,
        "platform": args.platform,
        "abi": args.abi,
        "rootfs_version": args.rootfs_version,
        "content_sha256": hashlib.sha256(canonical_json_bytes(immutable_files)).hexdigest(),
        "sbom_filename": "rootfs.spdx.json",
        "source_pins_filename": "mobile-linux-pins.json",
        "archive": {
            "filename": args.archive_filename,
            "sha256": args.archive_sha256,
            "size_bytes": args.archive_size,
        },
        "packages": [
            {
                "name": package.name,
                "version": package.version,
                "license": package.license,
                "architecture": package.architecture,
                "origin": package.origin,
            }
            for package in packages
        ],
        "executable_allowlist": allowlist,
        "immutable_files": immutable_files,
        "writable_paths": sorted(WRITABLE_ROOTS),
    }
    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(stable_json_dumps(manifest), encoding="utf-8")


def generate_spdx(args: argparse.Namespace) -> None:
    root = pathlib.Path(args.root).resolve()
    packages = validate_rootfs_tree(root)
    created = format_created_timestamp(source_date_epoch(args.source_date_epoch))
    package_payload = [
        {
            "name": package.name,
            "version": package.version,
            "license": package.license,
            "architecture": package.architecture,
            "origin": package.origin,
        }
        for package in packages
    ]
    namespace_hash = hashlib.sha256(canonical_json_bytes(package_payload)).hexdigest()
    document = {
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": args.name,
        "documentNamespace": f"https://crates/mobile-linux/spdx/{args.name}/{namespace_hash}",
        "creationInfo": {
            "created": created,
            "creators": ["Tool: scripts/mobile-linux/rootfs_tool.py"],
        },
        "packages": [
            {
                "name": package.name,
                "SPDXID": f"SPDXRef-Package-{package.name}",
                "versionInfo": package.version,
                "downloadLocation": "NOASSERTION",
                "filesAnalyzed": False,
                "licenseConcluded": package.license,
                "licenseDeclared": package.license,
                "supplier": "NOASSERTION",
                "originator": package.origin or "NOASSERTION",
            }
            for package in packages
        ],
    }
    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(stable_json_dumps(document), encoding="utf-8")


def generate_lock(args: argparse.Namespace) -> None:
    root = pathlib.Path(args.root).resolve()
    packages = validate_rootfs_tree(root)
    lock = {
        "schema_version": 1,
        "alpine": {
            "version": _ALPINE["version"],
            "branch": _ALPINE["branch"],
            "repositories": list(_ALPINE["repositories"]),
        },
        "policy": {
            "archive_format": "tar.gz",
            "busybox_applet_strategy": "symlink",
            "apk_disabled": False,
            "interactive_package_install_allowed": True,
            "forbidden_package_manager_paths": FORBIDDEN_PACKAGE_MANAGER_PATHS,
            "fixed_primary_packages": FIXED_PRIMARY_PACKAGES,
            "fixed_package_versions": FIXED_PACKAGE_VERSIONS,
        },
        "resolved_packages": [
            {
                "name": package.name,
                "version": package.version,
                "license": package.license,
                "architecture": package.architecture,
                "origin": package.origin,
            }
            for package in packages
        ],
    }
    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(stable_json_dumps(lock), encoding="utf-8")


def snapshot_allowlist(args: argparse.Namespace) -> None:
    manifest = json.loads(pathlib.Path(args.manifest).read_text(encoding="utf-8"))
    if not isinstance(manifest.get("executable_allowlist"), list):
        fail("manifest missing executable_allowlist")
    snapshot = {
        "schema_version": 1,
        "entries": manifest["executable_allowlist"],
    }
    output = pathlib.Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(stable_json_dumps(snapshot), encoding="utf-8")


def validate_lock(args: argparse.Namespace) -> None:
    lock = json.loads(pathlib.Path(args.lock).read_text(encoding="utf-8"))
    manifest = json.loads(pathlib.Path(args.manifest).read_text(encoding="utf-8"))
    if manifest.get("schema_version") != 2:
        fail("rootfs manifest schema_version must be 2")
    if lock.get("schema_version") != 1:
        fail("rootfs lock schema_version must be 1")
    alpine = lock.get("alpine")
    if not isinstance(alpine, dict):
        fail("rootfs lock missing alpine block")
    if (
        alpine.get("version") != _ALPINE["version"]
        or alpine.get("branch") != _ALPINE["branch"]
    ):
        fail(
            f"rootfs lock must pin Alpine {_ALPINE['version']} / {_ALPINE['branch']}"
        )
    policy = lock.get("policy")
    if not isinstance(policy, dict):
        fail("rootfs lock missing policy block")
    if policy.get("archive_format") != "tar.gz":
        fail("rootfs lock must pin tar.gz archive format")
    if policy.get("busybox_applet_strategy") != "symlink":
        fail("rootfs lock must record symlink BusyBox applets")
    if policy.get("apk_disabled") is not False:
        fail("rootfs lock must require apk_disabled=false")
    if policy.get("interactive_package_install_allowed") is not True:
        fail("rootfs lock must allow interactive package installation")
    if policy.get("fixed_primary_packages") != FIXED_PRIMARY_PACKAGES:
        fail("rootfs lock fixed_primary_packages diverged")
    if policy.get("fixed_package_versions") != FIXED_PACKAGE_VERSIONS:
        fail("rootfs lock fixed_package_versions diverged")
    if policy.get("forbidden_package_manager_paths") != FORBIDDEN_PACKAGE_MANAGER_PATHS:
        fail("rootfs lock forbidden_package_manager_paths diverged")
    resolved = lock.get("resolved_packages")
    if not isinstance(resolved, list) or not resolved:
        fail("rootfs lock must contain resolved_packages[]")
    identity_fields = ("name", "version", "license", "architecture", "origin")
    resolved_identities: set[Tuple[str, str, str, str, str]] = set()
    resolved_names: set[str] = set()
    for entry in resolved:
        if not isinstance(entry, dict):
            fail("rootfs lock resolved_packages entries must be objects")
        required_fields = set(identity_fields)
        if set(entry) != required_fields or any(
            not isinstance(entry.get(field), str) or not entry[field]
            for field in required_fields
        ):
            fail("rootfs lock resolved_packages entries must contain complete package identity")
        if entry["name"] in resolved_names:
            fail(f"rootfs lock contains duplicate package: {entry['name']}")
        resolved_names.add(entry["name"])
        resolved_identities.add(tuple(entry[field] for field in identity_fields))

    manifest_packages = manifest.get("packages")
    if not isinstance(manifest_packages, list) or not manifest_packages:
        fail("rootfs manifest must contain packages[]")
    manifest_identities: set[Tuple[str, str, str, str, str]] = set()
    manifest_names: set[str] = set()
    for entry in manifest_packages:
        if (
            not isinstance(entry, dict)
            or set(entry) != set(identity_fields)
            or any(
                not isinstance(entry.get(field), str) or not entry[field]
                for field in identity_fields
            )
        ):
            fail("rootfs manifest package entries must contain complete package identity")
        if entry["name"] in manifest_names:
            fail(f"rootfs manifest contains duplicate package: {entry['name']}")
        manifest_names.add(entry["name"])
        manifest_identities.add(tuple(entry[field] for field in identity_fields))

    if manifest_identities != resolved_identities:
        missing = sorted(manifest_identities - resolved_identities)
        extra = sorted(resolved_identities - manifest_identities)
        fail(
            "rootfs lock package identities diverged from manifest "
            f"(missing={missing}, extra={extra})"
        )
    print(f"rootfs build lock verified: {args.lock}")


@contextmanager
def open_tar_archive(archive: pathlib.Path):
    decoded_archive = None
    try:
        if archive.name.endswith(".tar.zst") or archive.suffix == ".zst":
            decoded_archive = tempfile.TemporaryFile()
            try:
                result = subprocess.run(
                    ["zstd", "-dc", "--", str(archive)],
                    stdout=decoded_archive,
                    stderr=subprocess.PIPE,
                    check=False,
                )
            except FileNotFoundError:
                fail("zstd is required to verify .tar.zst archives")
            if result.returncode != 0:
                detail = result.stderr.decode("utf-8", errors="replace").strip()
                fail(f"failed to decompress rootfs archive: {detail or result.returncode}")
            decoded_archive.flush()
            decoded_archive.seek(0)

        with tarfile.open(
            fileobj=decoded_archive,
            name=None if decoded_archive is not None else archive,
            mode="r:" if decoded_archive is not None else "r:*",
        ) as tar:
            yield tar
    finally:
        if decoded_archive is not None:
            decoded_archive.close()


def verify_archive(args: argparse.Namespace) -> None:
    archive = pathlib.Path(args.archive)
    if not archive.is_file() or archive.is_symlink():
        fail(f"archive not found or unsafe: {archive}")
    seen_paths: set[str] = set()
    hardlinks: List[Tuple[str, str]] = []
    symlinks: List[Tuple[str, str]] = []
    with open_tar_archive(archive) as tar:
        members = tar.getmembers()
        for member in members:
            path = safe_member_path(member.name).as_posix()
            if path in seen_paths:
                fail(f"duplicate archive entry: {path}")
            seen_paths.add(path)
            rel = f"/{path}"
            if member.isdev():
                fail(f"device entries are forbidden in rootfs archive: {rel}")
            if member.isfifo():
                fail(f"FIFO entries are forbidden in rootfs archive: {rel}")
            if member.mode & stat.S_ISUID or member.mode & stat.S_ISGID:
                fail(f"suid/sgid entries are forbidden in rootfs archive: {rel}")
            if rel in FORBIDDEN_PACKAGE_MANAGER_PATHS:
                fail(f"forbidden package-manager artifact present in archive: {rel}")
            if member.issym():
                if member.linkname.startswith("/"):
                    fail(f"archive symlink target must be relative: {rel} -> {member.linkname}")
                symlinks.append((path, resolve_symlink_target(path, member.linkname).as_posix()))
            elif member.islnk():
                normalized_target = normalize_hardlink_target(path, member.linkname).as_posix()
                hardlinks.append((path, normalized_target))
            elif member.isdir():
                if member.mode & stat.S_IWOTH and rel not in WORLD_WRITABLE_ALLOWED:
                    fail(f"world-writable directory is forbidden outside scratch paths: {rel}")
            elif member.isreg():
                if member.mode & stat.S_IWOTH and rel not in WORLD_WRITABLE_ALLOWED:
                    fail(f"world-writable file is forbidden outside scratch paths: {rel}")
            else:
                fail(f"unsupported archive entry type for {rel}")

    if "bin/busybox" not in seen_paths:
        fail("archive missing /bin/busybox")
    if "bin/sh" not in seen_paths:
        fail("archive missing /bin/sh")
    if "sbin/apk" not in seen_paths:
        fail("archive missing /sbin/apk")
    if "etc/apk/repositories" not in seen_paths:
        fail("archive missing /etc/apk/repositories")

    # A symlink in a binary directory that points at nothing is a broken command
    # on device, so resolve every one against the archive's own member set.
    for source, resolved in symlinks:
        if f"/{source}" .startswith(tuple(BINARY_SYMLINK_SCRUTINY_PREFIXES)) and resolved not in seen_paths:
            fail(f"archive symlink is dangling: /{source} -> {resolved}")

    # /bin/sh may reach busybox as either link kind; see validate_rootfs_tree.
    hardlink_pairs = {frozenset((path, link)) for path, link in hardlinks}
    symlink_targets = dict(symlinks)
    if (
        frozenset(("bin/sh", "bin/busybox")) not in hardlink_pairs
        and symlink_targets.get("bin/sh") != "bin/busybox"
    ):
        fail("/bin/sh must be recorded as a hardlink or symlink to /bin/busybox")

    print(f"archive verified: {archive}")


def build_archive(args: argparse.Namespace) -> None:
    root = pathlib.Path(args.root).resolve()
    output = pathlib.Path(args.output)
    validate_rootfs_tree(root)
    output.parent.mkdir(parents=True, exist_ok=True)
    epoch = source_date_epoch(args.source_date_epoch)

    inode_first_path: Dict[Tuple[int, int], str] = {}
    with tarfile.open(output, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for current_root, dirnames, filenames in os.walk(root, topdown=True, followlinks=False):
            current_dir = pathlib.Path(current_root)
            dirnames.sort()
            filenames.sort()
            dir_rel = root_rel(current_dir, root)
            if dir_rel != "/":
                dir_name = dir_rel.lstrip("/")
                dir_stat = os.lstat(current_dir)
                info = tarfile.TarInfo(dir_name)
                info.type = tarfile.DIRTYPE
                info.mode = normalized_tar_mode(dir_stat.st_mode, is_dir=True, is_symlink=False)
                info.uid = 0
                info.gid = 0
                info.uname = "root"
                info.gname = "root"
                info.mtime = epoch
                tar.addfile(info)

            for dirname in dirnames:
                path = current_dir / dirname
                if path.is_symlink():
                    rel = root_rel(path, root).lstrip("/")
                    target = os.readlink(path)
                    info = tarfile.TarInfo(rel)
                    info.type = tarfile.SYMTYPE
                    info.linkname = target
                    info.mode = normalized_tar_mode(os.lstat(path).st_mode, is_dir=False, is_symlink=True)
                    info.uid = 0
                    info.gid = 0
                    info.uname = "root"
                    info.gname = "root"
                    info.mtime = epoch
                    tar.addfile(info)

            for filename in filenames:
                path = current_dir / filename
                rel = root_rel(path, root).lstrip("/")
                st = os.lstat(path)
                if stat.S_ISLNK(st.st_mode):
                    info = tarfile.TarInfo(rel)
                    info.type = tarfile.SYMTYPE
                    info.linkname = os.readlink(path)
                    info.mode = normalized_tar_mode(st.st_mode, is_dir=False, is_symlink=True)
                    info.uid = 0
                    info.gid = 0
                    info.uname = "root"
                    info.gname = "root"
                    info.mtime = epoch
                    tar.addfile(info)
                    continue

                inode_key = (st.st_dev, st.st_ino)
                if inode_key in inode_first_path:
                    info = tarfile.TarInfo(rel)
                    info.type = tarfile.LNKTYPE
                    info.linkname = inode_first_path[inode_key]
                    info.mode = normalized_tar_mode(st.st_mode, is_dir=False, is_symlink=False)
                    info.uid = 0
                    info.gid = 0
                    info.uname = "root"
                    info.gname = "root"
                    info.mtime = epoch
                    tar.addfile(info)
                    continue

                inode_first_path[inode_key] = rel
                info = tarfile.TarInfo(rel)
                info.size = st.st_size
                info.mode = normalized_tar_mode(st.st_mode, is_dir=False, is_symlink=False)
                info.uid = 0
                info.gid = 0
                info.uname = "root"
                info.gname = "root"
                info.mtime = epoch
                with path.open("rb") as handle:
                    tar.addfile(info, handle)

    print(f"deterministic tar archive built: {output}")


def verify_release_archive(args: argparse.Namespace) -> None:
    pins = json.loads(pathlib.Path(args.pins).read_text(encoding="utf-8"))
    archives = pins.get("rootfs", {}).get("release_archives")
    record = archives.get(args.abi) if isinstance(archives, dict) else None
    expected = record.get("sha256") if isinstance(record, dict) else None
    # `rootfs.archives` pins the official minirootfs, whose bytes the shipped
    # archive can never match: the release archive is the deterministic,
    # package-augmented rebuild. Refuse a rebuild that carries no committed
    # digest of its own rather than trust the hash its own evidence directory
    # issues for itself.
    if (
        not isinstance(expected, str)
        or len(expected) != 64
        or any(character not in "0123456789abcdef" for character in expected)
    ):
        fail(
            f"no committed release rootfs digest for {args.abi}: add "
            f"rootfs.release_archives.{args.abi}.sha256 to mobile-linux-pins.json"
        )
    archive = pathlib.Path(args.archive)
    if not archive.is_file() or archive.is_symlink():
        fail(f"release rootfs archive not found or unsafe: {archive}")
    digest = read_sha256(archive)
    if digest != expected:
        fail(
            f"release rootfs SHA-256 mismatch for {args.abi}: "
            f"expected {expected}, got {digest}"
        )
    print(f"release rootfs archive pin verified: {archive}")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)

    verify_tree_cmd = sub.add_parser("verify-tree")
    verify_tree_cmd.add_argument("--root", required=True)
    verify_tree_cmd.set_defaults(func=lambda args: (validate_rootfs_tree(pathlib.Path(args.root)), print(f"rootfs tree verified: {args.root}")))

    verify_archive_cmd = sub.add_parser("verify-archive")
    verify_archive_cmd.add_argument("--archive", required=True)
    verify_archive_cmd.set_defaults(func=verify_archive)

    verify_release_cmd = sub.add_parser("verify-release-archive")
    verify_release_cmd.add_argument("--pins", required=True)
    verify_release_cmd.add_argument("--abi", required=True)
    verify_release_cmd.add_argument("--archive", required=True)
    verify_release_cmd.set_defaults(func=verify_release_archive)

    build_archive_cmd = sub.add_parser("build-archive")
    build_archive_cmd.add_argument("--root", required=True)
    build_archive_cmd.add_argument("--output", required=True)
    build_archive_cmd.add_argument("--source-date-epoch", type=int)
    build_archive_cmd.set_defaults(func=build_archive)

    manifest_cmd = sub.add_parser("generate-manifest")
    manifest_cmd.add_argument("--root", required=True)
    manifest_cmd.add_argument("--runtime", required=True)
    manifest_cmd.add_argument("--platform", required=True)
    manifest_cmd.add_argument("--abi", required=True)
    manifest_cmd.add_argument("--rootfs-version", required=True)
    manifest_cmd.add_argument("--archive-filename", required=True)
    manifest_cmd.add_argument("--archive-sha256", required=True)
    manifest_cmd.add_argument("--archive-size", required=True, type=int)
    manifest_cmd.add_argument("--output", required=True)
    manifest_cmd.set_defaults(func=generate_manifest)

    spdx_cmd = sub.add_parser("generate-spdx")
    spdx_cmd.add_argument("--root", required=True)
    spdx_cmd.add_argument("--name", required=True)
    spdx_cmd.add_argument("--output", required=True)
    spdx_cmd.add_argument("--source-date-epoch", type=int)
    spdx_cmd.set_defaults(func=generate_spdx)

    lock_cmd = sub.add_parser("generate-lock")
    lock_cmd.add_argument("--root", required=True)
    lock_cmd.add_argument("--output", required=True)
    lock_cmd.set_defaults(func=generate_lock)

    snapshot_cmd = sub.add_parser("snapshot-allowlist")
    snapshot_cmd.add_argument("--manifest", required=True)
    snapshot_cmd.add_argument("--output", required=True)
    snapshot_cmd.set_defaults(func=snapshot_allowlist)

    validate_lock_cmd = sub.add_parser("validate-lock")
    validate_lock_cmd.add_argument("--lock", required=True)
    validate_lock_cmd.add_argument("--manifest", required=True)
    validate_lock_cmd.set_defaults(func=validate_lock)

    return parser


def main() -> None:
    parser = build_parser()
    args = parser.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
