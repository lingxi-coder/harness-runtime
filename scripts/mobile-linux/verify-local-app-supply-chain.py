#!/usr/bin/env python3
import argparse
import base64
import binascii
import hashlib
import json
import pathlib
import re
import subprocess
import sys
import tempfile
sys.dont_write_bytecode = True
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from sdk_source import effective_pins


EXPECTED_DEPENDENCIES = {
    # The UI kit. Ionic is what makes a generated app look native on BOTH
    # platforms from one source: `setupIonicReact({ mode })` selects the iOS or
    # the Material design language at runtime from the host's OS, which is a
    # thing the previous shadcn/Radix set could not do at all -- it is a web
    # design language, and the platform "adapter" that was supposed to bridge it
    # published fields (`stateLayer: "ripple"`) that had zero consumers.
    #
    # It must be imported from the `@ionic/react` barrel. The per-component
    # entry points under `@ionic/core/components` are the only tree-shakeable
    # path, but they dynamically import one another and rolldown rejects that
    # under the `iife` output format this build is pinned to.
    "@ionic/react": "9.0.4",
    # Native page transitions and the platform back gesture, via IonRouterOutlet.
    # It peers on react-router 6.x, which is why react-router is pinned to 6
    # rather than 7.
    "@ionic/react-router": "9.0.4",
    "@vitejs/plugin-react": "6.1.1",
    "react": "19.3.0",
    "react-dom": "19.3.0",
    "react-router": "6.30.6",
    "react-router-dom": "6.30.6",
    "vite": "8.3.0",
    "zod": "4.6.5",
    "zustand": "5.0.15",
}
EXPECTED_OVERRIDES = {"lightningcss": "1.33.0"}
EXPECTED_SCRIPTS = {
    "build": "vite build",
    "dev": "vite",
    "preview": "vite preview",
}
EXPECTED_ROLLDOWN_BINDINGS = {
    "@rolldown/binding-linux-arm64-musl": "1.2.9",
    "@rolldown/binding-linux-x64-musl": "1.2.9",
}
EXPECTED_LIGHTNINGCSS_BINDINGS = {
    "lightningcss-linux-arm64-musl": "1.33.0",
    "lightningcss-linux-x64-musl": "1.33.0",
}
EXPECTED_ROLLUP_BINDINGS = {
    "@rollup/rollup-linux-arm64-musl": "4.44.0",
    "@rollup/rollup-linux-x64-musl": "4.44.0",
}
EXPECTED_NATIVE_PACKAGE_BINARIES = {
    "@rolldown/binding-linux-arm64-musl": "rolldown-binding.linux-arm64-musl.node",
    "@rolldown/binding-linux-x64-musl": "rolldown-binding.linux-x64-musl.node",
    "@rollup/rollup-linux-arm64-musl": "rollup.linux-arm64-musl.node",
    "@rollup/rollup-linux-x64-musl": "rollup.linux-x64-musl.node",
    "lightningcss-linux-arm64-musl": "lightningcss.linux-arm64-musl.node",
    "lightningcss-linux-x64-musl": "lightningcss.linux-x64-musl.node",
}
EXPECTED_ROLLDOWN_VERSION = "1.2.9"
EXPECTED_LIGHTNINGCSS_VERSION = "1.33.0"
EXPECTED_WRITABLE_ROOTS = ["app", "components", "lib", "styles", "public"]
VITE_EXPECTED_WRITABLE_ROOTS = EXPECTED_WRITABLE_ROOTS + ["src"]
FORBIDDEN_ROUTE_FILES = {"route.js", "route.jsx", "route.ts", "route.tsx"}

# Package-manager policy for the *rootfs* APK closure. npm remains available
# for terminal users, while the host-owned local-app installer uses the pinned
# pnpm tarball. Only managers that are not part of the supported toolchain stay
# out of the rootfs closure.
FORBIDDEN_PACKAGE_NAMES = {"corepack", "yarn"}
FORBIDDEN_EXECUTABLES = {
    "/usr/bin/corepack",
    "/usr/bin/yarn",
}
APK_VERSION_RE = re.compile(r"[0-9][0-9A-Za-z._]*(?:_[a-z]+[0-9]*)?-r[0-9]+")
ALPINE_RELEASE_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+")
ALPINE_CDN = "https://dl-cdn.alpinelinux.org/alpine"
SOURCE_SUFFIXES = {
    ".css",
    ".htm",
    ".html",
    ".js",
    ".jsx",
    ".mjs",
    ".svg",
    ".ts",
    ".tsx",
    ".xml",
}
FORBIDDEN_SOURCE_PATTERNS = {
    "direct network access": re.compile(r"\b(fetch|XMLHttpRequest|WebSocket|EventSource)\s*\("),
    "dynamic code evaluation": re.compile(r"\b(eval|Function)\s*\("),
    # Third-party scripts only. A same-origin RELATIVE src (`/x.js`, `./x.js`)
    # is Vite's required entry form and loads nothing the app did not ship, so
    # it is allowed; anything with a scheme (`https:`, `data:`, `javascript:`),
    # a protocol-relative `//host`, a `..` escape, a bare unrooted path, or a
    # dynamic expression still fails.
    "external script": re.compile(
        r"<script\b[^>]*?\bsrc\s*=\s*(?![\"']?\.?/(?!/))",
        re.IGNORECASE,
    ),
    "package manager invocation": re.compile(r"\b(npm|npx|corepack|yarn|pnpm)\b\s+(install|add|exec|dlx)\b"),
    "server action": re.compile(r"^[\t ]*[\"']use server[\"'];?", re.MULTILINE),
}

RUNTIME_PROFILE_COMMON_DEPENDENCIES = {
    "@ionic/react": "9.0.4",
    "@ionic/react-router": "9.0.4",
    "@vitejs/plugin-react": "6.1.1",
    "react": "19.3.0",
    "react-dom": "19.3.0",
    "react-router": "6.30.6",
    "react-router-dom": "6.30.6",
    "vite": "8.3.0",
    "zod": "4.6.5",
    "zustand": "5.0.15",
}
RUNTIME_PROFILE_LOCK_PACKAGES = {
    "rolldown": "1.2.9",
    "@rolldown/binding-linux-arm64-musl": "1.2.9",
    "@rolldown/binding-linux-x64-musl": "1.2.9",
    "@rollup/rollup-linux-arm64-musl": "4.44.0",
    "@rollup/rollup-linux-x64-musl": "4.44.0",
    "lightningcss": "1.33.0",
    "lightningcss-linux-arm64-musl": "1.33.0",
    "lightningcss-linux-x64-musl": "1.33.0",
}
RUNTIME_PROFILE_LOCK_SHA256 = {
    "react-dom": "27675ad6bfbcb79deec7d2f4e06ae986ed63e7312696196bae377ddaacf0eada",
    "canvas-2d": "27675ad6bfbcb79deec7d2f4e06ae986ed63e7312696196bae377ddaacf0eada",
    "three-3d": "64cbae2ddb878a757cdf99c022887eec1e5a050859517abb6c51e4cf648d30e9",
    "phaser-2d": "d7361a1cd59a5a9ac183b967cde2952a3e9583d39e0c9ac4b94e76b0f86e97c2",
    "babylon-3d": "df787d493a39e7ee3765e1cf70302957176aa7bbbf180208eca7ba33f56a83d4"
}
RUNTIME_PROFILES = {
    "react-dom": {
        "extra_dependencies": {},
        "host_managed_helpers": [],
    },
    "canvas-2d": {
        "extra_dependencies": {},
        "host_managed_helpers": ["lib/frame-loop.js"],
    },
    "three-3d": {
        "extra_dependencies": {"three": "0.186.0"},
        "host_managed_helpers": ["lib/frame-loop.js"],
    },
    "phaser-2d": {
        "extra_dependencies": {"phaser": "4.2.1"},
        "host_managed_helpers": ["lib/frame-loop.js", "lib/phaser-runtime.js"],
    },
    "babylon-3d": {
        "extra_dependencies": {
            "@babylonjs/core": "9.27.1",
            "@babylonjs/havok": "1.3.14",
            "@babylonjs/loaders": "9.27.1",
        },
        "host_managed_helpers": ["lib/frame-loop.js", "lib/babylon-runtime.js"],
    },
}
RUNTIME_PROFILE_HOST_MANAGED_BASE = [
    ".lingxi",
    ".gitignore",
    "LINGXI.md",
    "index.html",
    "jsconfig.json",
    "vite.config.mjs",
    "package.json",
    "pnpm-lock.yaml",
    "pnpm-workspace.yaml",
    "lib/device-context.js",
    "lib/lingxi-bridge.js",
    "lib/platform-adapter.js",
    "lib/lingxi-provider.jsx",
]
RUNTIME_PROFILE_HOST_MANAGED_SUFFIX = [
    "styles/foundation.css",
    "node_modules",
]


def expected_native_packages_for(platform: str, family: str) -> dict[str, str]:
    if family == "rolldown":
        bindings = EXPECTED_ROLLDOWN_BINDINGS
        arm64 = "@rolldown/binding-linux-arm64-musl"
    elif family == "rollup":
        bindings = EXPECTED_ROLLUP_BINDINGS
        arm64 = "@rollup/rollup-linux-arm64-musl"
    elif family == "lightningcss":
        bindings = EXPECTED_LIGHTNINGCSS_BINDINGS
        arm64 = "lightningcss-linux-arm64-musl"
    else:
        fail(f"unknown native package family: {family}")
    if platform == "ios":
        return {arm64: bindings[arm64]}
    if platform == "android":
        return dict(bindings)
    fail(f"unknown runtime platform: {platform}")


def expected_native_binary_for(package_name: str) -> str:
    binary = EXPECTED_NATIVE_PACKAGE_BINARIES.get(package_name)
    if binary is None:
        fail(f"missing expected binary metadata for native package: {package_name}")
    return binary


def fail(message: str) -> None:
    print(message, file=sys.stderr)
    raise SystemExit(1)


def load_yaml_mapping(path: pathlib.Path) -> dict:
    """Parse the flat two-level settings mapping pnpm-workspace.yaml uses.

    PyYAML is not a dependency of this gate and adding one to a supply-chain
    verifier to read seven settings is a poor trade. The grammar accepted here
    is exactly what the template file uses: `key: value` at column 0, and
    `key:` followed by two-space-indented `name: value` pairs. Anything else
    fails loudly rather than being skipped, so a file that grows a construct
    this cannot represent cannot pass by being misread.
    """
    mapping: dict = {}
    current: dict | list | None = None
    for number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        if not raw.strip() or raw.lstrip().startswith("#"):
            continue
        if raw.startswith("  "):
            if current is None:
                fail(f"{path}:{number}: indented entry outside a mapping")
            nested = raw[2:]
            if isinstance(current, list):
                match = re.fullmatch(r"-\s+(['\"])([^'\"]+)\1", nested)
                if match is None:
                    fail(f"{path}:{number}: unsupported workspace package syntax")
                current.append(match.group(2))
                continue
            # A third level would be flattened into the second if it were
            # accepted here, which is exactly the silent misreading this parser
            # must not do: the caller would compare a mapping that never
            # existed in the file.
            if nested.startswith(" "):
                fail(f"{path}:{number}: unsupported nesting depth")
            key, separator, value = nested.partition(":")
            if not separator or not key.strip() or not value.strip():
                fail(f"{path}:{number}: unsupported nested syntax")
            current[key.strip()] = value.strip()
            continue
        # Any other leading whitespace -- a single space, or a tab, which YAML
        # forbids for indentation -- would otherwise be stripped and read as a
        # TOP-LEVEL key, turning a nested entry into a sibling of its parent.
        if raw[:1].isspace():
            fail(f"{path}:{number}: unsupported indentation")
        key, separator, value = raw.partition(":")
        if not separator:
            fail(f"{path}:{number}: unsupported syntax")
        if value.strip():
            mapping[key.strip()] = value.strip()
            current = None
        else:
            current = [] if key.strip() == "packages" else {}
            mapping[key.strip()] = current
    return mapping


def load_json(path: pathlib.Path) -> dict:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        fail(f"invalid JSON {path}: {exc}")
    if not isinstance(value, dict):
        fail(f"JSON root must be an object: {path}")
    return value


def valid_sha256(value: object) -> bool:
    return isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None


def valid_sha512_base64(value: object) -> bool:
    if not isinstance(value, str):
        return False
    try:
        return len(base64.b64decode(value, validate=True)) == 64
    except (ValueError, binascii.Error):
        return False


def validate_typescript_native_pin(repo: pathlib.Path, pins: dict) -> None:
    toolchain = pins.get("typescript_native")
    if not isinstance(toolchain, dict):
        fail("local-app runtime pins must carry typescript_native")
    version = toolchain.get("version")
    if version != "7.0.2":
        fail("native TypeScript toolchain must remain pinned to 7.0.2")
    if toolchain.get("install_root") != f"/opt/lingxi/toolchains/typescript/{version}":
        fail("native TypeScript install_root must be its fixed /opt toolchain path")
    if toolchain.get("license") != "Apache-2.0":
        fail("native TypeScript license pin must be Apache-2.0")

    packages = toolchain.get("packages")
    expected = {
        "aarch64": "@typescript/typescript-linux-arm64",
        "x86_64": "@typescript/typescript-linux-x64",
    }
    if not isinstance(packages, dict) or set(packages) != set(expected):
        fail("native TypeScript packages must cover exactly aarch64 and x86_64")
    for arch, name in expected.items():
        package = packages.get(arch)
        expected_url = (
            f"https://registry.npmjs.org/{name}/-/"
            f"{name.rsplit('/', 1)[1]}-{version}.tgz"
        )
        if not isinstance(package, dict) or package.get("name") != name:
            fail(f"native TypeScript package identity diverged for {arch}")
        if package.get("url") != expected_url:
            fail(f"native TypeScript package URL diverged for {arch}")
        if not valid_sha512_base64(package.get("sha512")):
            fail(f"native TypeScript package needs a SHA-512 integrity pin for {arch}")
        if not valid_sha256(package.get("tsc_sha256")):
            fail(f"native TypeScript tsc needs a SHA-256 pin for {arch}")

    # Shared TypeScript source and binary pins now have one SDK-owned authority.


def expected_apk_packages(pins: dict) -> dict:
    """The pinned primary APK set, read from the pins rather than duplicated.

    The pins file is the single source of version truth: everything else in the
    tree is checked *against* it. Holding a second copy of these versions here
    is what let the template move to Node 24.18.1 while this module still said
    22.23.0, which took the whole release gate red.
    """
    packages = pins.get("runtime_packages")
    if not isinstance(packages, dict) or not packages:
        fail("local-app runtime pins must list runtime_packages")
    for name, version in packages.items():
        if not isinstance(name, str) or not name:
            fail("runtime_packages keys must be package names")
        if not isinstance(version, str) or not APK_VERSION_RE.fullmatch(version):
            fail(f"runtime_packages must pin an exact APK version: {name}={version!r}")
        if name in FORBIDDEN_PACKAGE_NAMES:
            fail(f"runtime_packages must not install a forbidden package manager: {name}")
    return packages


def validate_alpine_pin(pins: dict) -> dict:
    """Structural checks on the Alpine pin itself.

    Nothing here asserts a specific release — advancing Alpine is a pins edit,
    not a code edit. What must hold is that the pin is internally coherent and
    points only at official repositories.
    """
    alpine = pins.get("alpine")
    if not isinstance(alpine, dict):
        fail("local-app runtime pins must carry an `alpine` record")
    version = alpine.get("version")
    if not isinstance(version, str) or not ALPINE_RELEASE_RE.fullmatch(version):
        fail("alpine.version must be an exact three-part Alpine release")
    branch = "v" + ".".join(version.split(".")[:2])
    if alpine.get("branch") != branch:
        fail(f"alpine.branch must be {branch} for Alpine {version}")
    expected_repositories = [f"{ALPINE_CDN}/{branch}/main", f"{ALPINE_CDN}/{branch}/community"]
    if alpine.get("repositories") != expected_repositories:
        fail("alpine.repositories must be the official main+community CDN URLs for the pinned branch")
    minirootfs = alpine.get("minirootfs")
    if not isinstance(minirootfs, dict) or not minirootfs:
        fail("alpine.minirootfs must pin the release tarball per architecture")
    for arch, record in minirootfs.items():
        if not isinstance(record, dict):
            fail(f"invalid minirootfs pin for {arch}")
        expected_url = (
            f"{ALPINE_CDN}/{branch}/releases/{arch}/alpine-minirootfs-{version}-{arch}.tar.gz"
        )
        if record.get("url") != expected_url:
            fail(f"minirootfs URL for {arch} must be {expected_url}")
        if not valid_sha256(record.get("sha256")):
            fail(f"minirootfs pin for {arch} needs a SHA-256")
    return alpine


def validate_apk_pins(pins: dict, release: bool, apk_dir: pathlib.Path | None) -> None:
    if pins.get("schema_version") != 2:
        fail("local-app runtime pins must use schema_version 2")
    alpine = validate_alpine_pin(pins)
    EXPECTED_APK_PACKAGES = expected_apk_packages(pins)
    if set(pins.get("forbidden_packages", [])) != FORBIDDEN_PACKAGE_NAMES:
        fail("forbidden package-manager package set diverged")
    if set(pins.get("forbidden_executables", [])) != FORBIDDEN_EXECUTABLES:
        fail("forbidden package-manager executable set diverged")

    by_abi = pins.get("apk_artifacts")
    if not isinstance(by_abi, dict) or set(by_abi) != {"arm64-v8a", "x86_64"}:
        fail("APK pins must cover exactly arm64-v8a and x86_64")
    for abi, abi_record in by_abi.items():
        if not isinstance(abi_record, dict):
            fail(f"invalid APK pin record for {abi}")
        artifacts = abi_record.get("artifacts")
        if not isinstance(artifacts, list):
            fail(f"APK pins missing artifacts for {abi}")
        primary_identities = set()
        all_identities = set()
        for artifact in artifacts:
            if not isinstance(artifact, dict):
                fail(f"invalid APK artifact for {abi}")
            name = artifact.get("name")
            version = artifact.get("version")
            role = artifact.get("role")
            if not isinstance(name, str) or not name or not isinstance(version, str) or not version:
                fail(f"incomplete APK identity for {abi}")
            if name in FORBIDDEN_PACKAGE_NAMES:
                fail(f"forbidden package-manager APK in closure for {abi}: {name}")
            identity = (name, version)
            if identity in all_identities:
                fail(f"duplicate APK identity for {abi}: {name}={version}")
            all_identities.add(identity)
            if role == "primary":
                if EXPECTED_APK_PACKAGES.get(name) != version:
                    fail(f"unexpected primary APK identity for {abi}: {name}={version}")
                primary_identities.add(name)
            elif role != "transitive":
                fail(f"APK artifact role must be primary or transitive for {abi}: {name}")
            # Pin the whole URL, not just its tail. A suffix match accepts any
            # host and any branch, which is precisely what a supply-chain pin
            # exists to prevent.
            section = artifact.get("repository")
            if section not in ("main", "community"):
                fail(f"APK artifact must record repository main|community for {abi}: {name}")
            alpine_arch = abi_record.get("alpine_arch")
            expected_url = (
                f"{ALPINE_CDN}/{alpine['branch']}/{section}/{alpine_arch}/{name}-{version}.apk"
            )
            if artifact.get("url") != expected_url:
                fail(f"APK URL for {abi}/{name} must be {expected_url}")
            if artifact.get("arch") != alpine_arch:
                fail(f"APK artifact arch must be {alpine_arch} for {abi}: {name}")
            availability = artifact.get("availability")
            digest = artifact.get("sha256")
            if availability == "available":
                if not valid_sha256(digest):
                    fail(f"available APK must have a SHA-256 pin for {abi}: {name}")
            elif availability == "unavailable":
                if digest is not None:
                    fail(f"unavailable APK must not contain a fabricated digest for {abi}: {name}")
            else:
                fail(f"invalid APK availability for {abi}: {name}")
        if primary_identities != set(EXPECTED_APK_PACKAGES):
            fail(f"APK primary pin set incomplete for {abi}")
        if abi_record.get("closure_status") != "complete":
            blocker = abi_record.get("blocker")
            if not isinstance(blocker, str) or not blocker.strip():
                fail(f"incomplete APK closure must include a blocker for {abi}")
            if release:
                fail(f"release blocked for {abi}: {blocker}")

        if release:
            if apk_dir is None:
                fail("--apk-dir is required for release verification")
            abi_dir = apk_dir / abi
            for artifact in artifacts:
                if artifact.get("availability") != "available":
                    fail(f"release APK is unavailable for {abi}: {artifact.get('name')}")
                filename = pathlib.PurePosixPath(artifact["url"]).name
                package_path = abi_dir / filename
                if not package_path.is_file() or package_path.is_symlink():
                    fail(f"release APK is missing or unsafe: {package_path}")
                actual = hashlib.sha256(package_path.read_bytes()).hexdigest()
                if actual != artifact["sha256"]:
                    fail(f"release APK SHA-256 mismatch: {package_path}")

    if pins.get("release_ready") is not all(
        record.get("closure_status") == "complete" for record in by_abi.values()
    ):
        fail("release_ready must reflect APK closure completeness")


def expected_node(pins: dict) -> str:
    """Node version the template must pin, read from the pins.

    Derived rather than duplicated: the Node version appears in the pins, the
    template package.json, the lockfile, and the Alpine `nodejs` APK, and a
    second hardcoded copy here is what let three of them advance while the
    fourth silently stayed behind.
    """
    runtime = pins.get("local_app_runtime")
    if not isinstance(runtime, dict):
        fail("local-app runtime pins must carry a local_app_runtime record")
    version = runtime.get("node")
    if not isinstance(version, str) or not re.fullmatch(r"\d+\.\d+\.\d+", version):
        fail("local_app_runtime.node must be an exact three-part Node version")
    source = pins.get("node_source", {})
    if (source.get("version") != version
            or source.get("url") not in {
                f"https://nodejs.org/dist/v{version}/node-v{version}.tar.xz",
                f"https://nodejs.org/dist/v{version}/node-v{version}.tar.gz",
            }
            or not re.fullmatch(r"[0-9a-f]{64}", str(source.get("sha256", "")))):
        fail("local_app_runtime.node must match the hashed official Node source pin")
    return version


def validate_sbom(repo: pathlib.Path, template: pathlib.Path) -> None:
    sbom = load_json(repo / "docs" / "mobile-linux" / "sbom" / "local-app-runtime.spdx.json")
    if sbom.get("spdxVersion") != "SPDX-2.3":
        fail("local-app runtime SBOM must use SPDX 2.3")
    packages = sbom.get("packages")
    if not isinstance(packages, list):
        fail("local-app runtime SBOM missing packages")
    if len(packages) != 1 or not isinstance(packages[0], dict):
        fail("local-app runtime SBOM must contain one pnpm lockfile package")
    lock_digest = hashlib.sha256((template / "pnpm-lock.yaml").read_bytes()).hexdigest()
    package = packages[0]
    if (
        package.get("name") != "lingxi-local-app-template"
        or package.get("versionInfo") != "pnpm-lock.yaml"
        or package.get("sourceInfo") != f"pnpm lockfile sha256: {lock_digest}"
        or {"algorithm": "SHA256", "checksumValue": lock_digest}
        not in package.get("checksums", [])
    ):
        fail("local-app runtime SBOM does not match pnpm-lock.yaml")


def validate_runtime_policy(repo: pathlib.Path) -> None:
    policy = load_json(repo / "docs" / "mobile-linux" / "local-app-runtime-policy.json")
    node = "/usr/bin/node"
    build_root = "/var/lingxi/local-app-build/{app_id}/{channel}/project"
    guest_build_state_root = f"{build_root}/.lingxi-build-state"
    guest_home_root = f"{guest_build_state_root}/home"
    guest_temp_root = f"{guest_build_state_root}/tmp"
    guest_xdg_cache_root = f"{guest_build_state_root}/xdg-cache"
    guest_xdg_config_root = f"{guest_build_state_root}/xdg-config"
    guest_xdg_data_root = f"{guest_build_state_root}/xdg-data"
    vite_binary = f"{build_root}/node_modules/vite/bin/vite.js"
    if policy.get("schema_version") != 1:
        fail("local-app runtime policy must use schema_version 1")
    if policy.get("node_executable") != node or policy.get("vite_executable") != vite_binary:
        fail("local-app runtime command paths diverged")
    if "next_executable" in policy:
        fail("local-app runtime policy must not retain a Next executable path")
    if "node_modules_mount" in policy:
        fail("local-app runtime policy must not guest-mount shared node_modules")
    if "scaffold" in policy:
        fail("local-app runtime policy must not pin create-vite scaffolding policy")
    dependency_snapshot = policy.get("dependency_snapshot")
    if dependency_snapshot != {
        "source": "embedded:runtime-profiles/react-dom/r4/pnpm-lock.yaml",
        "materialize_into": f"{build_root}/node_modules",
        "guest_mount": "forbidden",
        "selection_policy": "exact_lock_only",
        "install_command": "pnpm install --frozen-lockfile --ignore-scripts --no-runtime --prefer-offline",
    }:
        fail("local-app dependency snapshot policy diverged")
    build_mount = policy.get("build_mount")
    if build_mount != {
        "kind": "LocalAppBuild",
        "count": 1,
        "host_path_policy": "workspace_or_staging_or_store_root",
        "guest_path": build_root,
        "writable": True,
    }:
        fail("local-app build mount policy diverged")
    commands = policy.get("commands")
    old_space_argument = "--max-old-space-size={build_node_old_space_size_mib}"
    if not isinstance(commands, dict):
        fail("local-app runtime policy missing commands")
    if set(commands) != {"vite_static_build"}:
        fail("local-app runtime policy must expose only the Vite static build command")
    vite_build = commands.get("vite_static_build")
    if (
        not isinstance(vite_build, dict)
        or vite_build.get("argv")
        != [node, old_space_argument, vite_binary, "build", "--outDir", "dist", "--emptyOutDir"]
        or vite_build.get("cwd") != build_root
        or vite_build.get("output_dir") != "dist"
        or vite_build.get("environment")
        != {
            "NODE_ENV": "production",
            "HOME": guest_home_root,
            "TMPDIR": guest_temp_root,
            "TMP": guest_temp_root,
            "TEMP": guest_temp_root,
            "XDG_CACHE_HOME": guest_xdg_cache_root,
            "XDG_CONFIG_HOME": guest_xdg_config_root,
            "XDG_DATA_HOME": guest_xdg_data_root,
        }
        or vite_build.get("network_policy") != "disabled"
        or vite_build.get("memory_limit_policy") != "physical_memory_tier"
        or "memory_limit_bytes" in vite_build
        or vite_build.get("timeout_ms") != 30 * 60 * 1000
    ):
        fail("fixed Vite build command diverged")
    limits = policy.get("limits")
    expected_build_memory_tiers = [
        {
            "physical_memory_max_exclusive_bytes": 6 * 1024**3,
            "process_tree_memory_bytes": 2048 * 1024**2,
            "node_max_old_space_size_mib": 1536,
        },
        {
            "physical_memory_max_exclusive_bytes": 8 * 1024**3,
            "process_tree_memory_bytes": 3072 * 1024**2,
            "node_max_old_space_size_mib": 2304,
        },
        {
            "physical_memory_max_exclusive_bytes": None,
            "process_tree_memory_bytes": 4096 * 1024**2,
            "node_max_old_space_size_mib": 3072,
        },
    ]
    if (
        not isinstance(limits, dict)
        or limits.get("build_concurrency") != 1
        or limits.get("build_node_old_space_percent") != 75
        or limits.get("build_memory_tiers") != expected_build_memory_tiers
        or limits.get("runtime_process_tree_memory_bytes") != 800 * 1024 * 1024
        or "node_process_tree_memory_bytes" in limits
    ):
        fail("local-app build tiers or runtime memory limit diverged")
    if "package_manager_policy" in policy:
        fail("local-app runtime policy must not pin CLI scaffolding/package-manager policy")



# The retired create flow launched the `local-app-build` plugin workflow, so
# `skills/create-local-app/SKILL.md` used to carry a wrapped
# `Workflow({"name":"lingxi-local-app:local-app-build","args":{"operation":
# "create"...}})` example with an `authoring_spec`/`mcp_intent` payload, and
# this module pinned those exact tokens. `local-app-build.js` is DELETED and
# create now runs as EnterPlanMode -> ExitPlanMode (the user's plan approval IS
# the create confirmation) -> LocalAppPrepare -> implement -> LocalAppBuild ->
# LocalAppRuntime: the skill launches no plugin workflow and carries no such
# example. The token set and the function that required it were removed with
# the workflow; the replacement grounding lives in `validate_create_flow_contract`
# below, which pins what the rewritten skill actually says.


# `local-app-build.js` is DELETED, so the optimized build-workflow contract it
# pinned (`OPTIMIZED_BUILD_WORKFLOW_REQUIRED`/`_FORBIDDEN` and
# `validate_optimized_build_workflow_contract`) went with it. The create flow no
# longer launches a plugin workflow at all; MCP authoring and the on-demand
# use-test workflow are the only plugin workflows left, and their live checks
# are in `validate_create_skill` and `validate_agent_prompt_contracts`.


def validate_create_flow_contract(text: str) -> None:
    """Pin the plan-driven create contract that replaced the retired workflow.

    The create path no longer launches a plugin workflow. Its ordered shape is
    `EnterPlanMode` (read-only planning) -> `ExitPlanMode`, where the user's
    approval of the plan IS the create confirmation -> `LocalAppPrepare`
    (Host re-reads its own approval record and lands/stages from the approved
    plan) -> implement -> `LocalAppBuild` -> `LocalAppRuntime`. Each stage is
    pinned by its section header and by the sentence that carries its
    authority, and the retired workflow id and its deleted agents are asserted
    ABSENT so a reintroduced `local-app-build` launch or automatic QA/verifier
    chain fails here instead of silently unpinning create.
    """
    ordered_stages = {
        "## 1. Plan",
        "## 2. Prepare",
        "## 3. Implement per the plan",
        "## 4. Build",
        "## 5. Deliver",
    }
    missing_stages = sorted(stage for stage in ordered_stages if stage not in text)
    if missing_stages:
        fail(
            "create-local-app skill is missing the plan-driven create stages "
            "(plan -> approval -> prepare -> implement -> build -> deliver): "
            f"{missing_stages}"
        )
    flow_tokens = {
        "EnterPlanMode",
        "ExitPlanMode",
        # The plan approval IS the create confirmation; there is no second native
        # create sheet, which is the whole point of the deferred flow.
        "That approval — the Allow on the plan — IS the create confirmation.",
        "After the user approves the plan, call:",
        "LocalAppPrepare({",
        "You never scaffold yourself",
        # A successful build is the completion condition; create runs no
        # automatic verification, QA scoring or repair loop.
        "A successful `LocalAppBuild` is the completion condition.",
        "automatic verification stage after it",
        "no build workflow",
    }
    missing_flow = sorted(token for token in flow_tokens if token not in text)
    if missing_flow:
        fail(
            "create-local-app skill no longer states the plan-driven create "
            f"contract (plan -> approval -> LocalAppPrepare -> build -> runtime): {missing_flow}"
        )
    # The build workflow is retired and the create flow launches none: the skill
    # must not name the deleted workflow id, its deleted agents, or the Host
    # tools only the retired workflow used (`LocalAppScaffold`,
    # `LocalAppResolveTemplateSelection`, the QA-launch tools). A reintroduction
    # of the old flow would otherwise pass unnoticed.
    forbidden_tokens = {
        "local-app-build",
        "builder",
        "designer",
        "template-selector",
        "create-preparer",
        "LocalAppResolveTemplateSelection",
        "LocalAppScaffold",
        "LocalAppQaBegin",
        "LocalAppQaReadEvidence",
        "LocalAppQaFinalize",
    }
    reintroduced = sorted(token for token in forbidden_tokens if token in text)
    if reintroduced:
        fail(
            "create-local-app skill names the retired build workflow or its "
            "deleted agents/Host tools; the plan-driven flow launches no plugin "
            f"workflow and runs no automatic QA chain: {reintroduced}"
        )


def validate_create_skill(repo: pathlib.Path) -> None:
    skill_path = repo / "skills" / "create-local-app" / "SKILL.md"
    try:
        text = skill_path.read_text(encoding="utf-8")
    except OSError as exc:
        fail(f"missing create-local-app skill: {exc}")
    # `skills/create-local-app/SKILL.md` has a BYTE-IDENTICAL mirror under the
    # plugin tree (the copy a plugin-loaded skill actually reads). A one-sided
    # edit here is a silent divergence a text diff over the whole repo would
    # not surface unless someone thought to run it -- pin the comparison so
    # editing one copy without the other fails loudly instead of shipping.
    mirror_path = (
        repo
        / "crates"
        / "plugins"
        / "lingxi-local-app"
        / "skills"
        / "create-local-app"
        / "SKILL.md"
    )
    try:
        mirror_text = mirror_path.read_text(encoding="utf-8")
    except OSError as exc:
        fail(f"missing create-local-app plugin skill mirror: {exc}")
    if mirror_text != text:
        fail(
            f"create-local-app SKILL.md and its plugin mirror ({mirror_path}) "
            "have diverged -- both copies must stay byte-identical"
        )
    if not text.startswith("---\nname: create-local-app\ndescription: "):
        fail("create-local-app skill frontmatter is invalid")
    # Every token below is a substring of the CURRENT SKILL.md. The retired
    # create flow launched the `local-app-build` plugin workflow, so the old set
    # pinned that workflow's wrapped launch example, the quality tiers, the
    # rescore loop, the template-selection handshake and the add/no-outDir
    # artifact spellings -- all gone with the workflow. This set pins the
    # model-facing tool surface the plan-driven create actually names; the
    # ordered plan -> prepare -> build -> runtime contract and the
    # forbidden-token checks live in `validate_create_flow_contract` below.
    required_tokens = {
        "LocalAppCreate",
        "LocalAppRuntimeProfiles",
        "LocalAppTemplateCatalog",
        "LocalAppManifest",
        "LocalAppBuild",
        "LocalAppRuntime",
        "LocalAppLogs",
        "LocalAppInstallDeps",
        "LocalAppInspectUi",
        "LocalAppActOnUi",
        "LocalAppQueryData",
        "LocalAppMutateData",
        "LocalAppCheckpointRestore",
        "LocalAppCaptureUi",
        "LocalAppConfirmDependencyChange",
        "LocalAppUpdateDependencies",
        "window.lingxi.v2",
        # `LocalAppList`/`LocalAppGet` must not be used to rediscover an app the
        # workspace is already bound to; this is the skill's own sentence.
        "`LocalAppList` or `LocalAppGet` to rediscover it",
        "with `AskUserQuestion`",
        "Never declare host-owned record metadata",
        "`canvas` when the whole interface is one drawn surface",
        "background_schedule",
        "streamLlmChat",
        "onLlmStreamFrame",
        "getClipboardText",
        "setClipboardText",
        "shareContent",
        "synthesizeSpeech",
        "readFile",
        "writeFile",
        "getDeviceStatus",
        "triggerHaptics",
        "openDeepLink",
        "listCalendarEvents",
        "searchContacts",
        "getMedia",
    }
    missing = sorted(token for token in required_tokens if token not in text)
    if missing:
        fail(f"create-local-app skill is missing host contract tokens: {missing}")
    validate_create_flow_contract(text)

    # A pin here required every `local-app-build` launch example in this skill
    # to be written as a `Workflow({"name":"lingxi-local-app:local-app-build"...
    # })` CALL carrying `authoring_spec`/`mcp_intent`, plus at least one example
    # that omitted `mcp_intent`. The skill no longer launches a plugin workflow
    # (see `validate_create_flow_contract`), so the examples and the check are
    # gone; `create-local-app/SKILL.md` now carries a single fenced
    # `authoring-spec` block that `LocalAppPrepare` reads, which
    # `validate_create_flow_contract` pins instead.
    local_apps_host_path = (
        repo / "crates" / "harness-runtime" / "src" / "mobile" / "local_apps_host.rs"
    )
    try:
        local_apps_host = local_apps_host_path.read_text(encoding="utf-8")
    except OSError as exc:
        fail(f"missing local-apps host: {exc}")
    if "already bound to local app `{id}`" not in local_apps_host:
        fail("app-scoped LINGXI.md does not make its current app id authoritative")
    # There was a pin here requiring the create-time MCP interview text in the
    # guided `workspace/LINGXI.md` as well, on the premise that an unscaffolded
    # shell's only channel to the model is that contract, so the interview could
    # not live in SKILL.md alone. `f81c57261` moved the interview deliberately:
    # the guided contract is now a thin identity/no-write guard that immediately
    # enters `lingxi-local-app:create-local-app`, which is the channel. Its Rust
    # test `guided_shell_delegates_without_technical_or_mcp_prerequisites` pins
    # that delegation AND asserts `mcpSuggestions`/`LocalAppTemplateCatalog` are
    # absent from the guided contract -- the exact strings this pin demanded, so
    # the repository could not satisfy both and this script has failed since.
    #
    # There WAS a pin here requiring the retired build workflow's own contract:
    # its `local-app-build.js` source, the create-declined/stage/scaffold/
    # QA-launch invariants (`workflow_tokens`) and the optimized build-workflow
    # tokens (`validate_optimized_build_workflow_contract`). All of that is
    # deleted -- `local-app-build.js` no longer exists and create launches no
    # plugin workflow, so the main session owns plan -> prepare -> build ->
    # runtime and `validate_create_flow_contract` above pins those steps. Only
    # the still-live plugin workflows are checked here.
    workflow_dir = repo / "crates" / "plugins" / "lingxi-local-app" / "workflows"
    try:
        mcp_authoring = (workflow_dir / "local-app-mcp-authoring.js").read_text(
            encoding="utf-8"
        )
    except OSError as exc:
        fail(f"missing plugin-owned local-app workflow: {exc}")
    mcp_authoring_tokens = {
        "const WORKFLOW_ID = 'lingxi-local-app:local-app-mcp-authoring';",
        "HOST_CONTEXT_REQUIRED",
        "HOST_INVOCATION_CAPABILITY_REQUIRED",
        "LocalAppValidateMcpProposal",
        "LocalAppApproveMcpProposal",
        "LocalAppQaMcpCandidate",
        "LocalAppPromoteMcpCandidate",
        "mcp_authoring_required",
        "approval_required",
        "proposal_sha256",
        "approval_contract_sha256",
        "tool_surface_sha256",
    }
    missing_mcp_authoring = sorted(
        token for token in mcp_authoring_tokens if token not in mcp_authoring
    )
    if missing_mcp_authoring:
        fail(
            "plugin local-app-mcp-authoring workflow is missing contract tokens: "
            f"{missing_mcp_authoring}"
        )

    handoff_path = repo / "docs" / "local-apps" / "HANDOFF.md"
    try:
        handoff = handoff_path.read_text(encoding="utf-8")
    except OSError as exc:
        fail(f"missing local-app handoff: {exc}")
    # Keep the handoff tied to the scaffold that the host actually seeds. The
    # old Template v2 paragraph described an unavailable Tailwind/shadcn stack
    # and omitted the provider/bridge helpers that generated source must use.
    #
    # The previous set also required the headings "DOM workflow shape", "Canvas
    # workflow shape" and "Shared workflow core". Those named the retired
    # workflow-shaped create path; the handoff now describes the plan-driven
    # flow (EnterPlanMode -> ExitPlanMode -> LocalAppPrepare), so the three
    # headings and their tokens went away with the workflow rather than being
    # reworded. The remaining tokens all describe the scaffold the host still
    # seeds.
    handoff_tokens = {
        "runtime-profiles/react-dom/r4",
        "runtime-profiles/canvas-2d/r4",
        "@ionic/react",
        "LingXiBridgeProvider",
        "IonReactHashRouter",
        "lib/lingxi-provider.jsx",
        "queryCollection",
        "requestLlmChat",
        "expected_writable_collections",
    }
    missing_handoff = sorted(token for token in handoff_tokens if token not in handoff)
    if missing_handoff:
        fail(f"local-app handoff is missing scaffold/workflow contract tokens: {missing_handoff}")
    if "Template v2 bundles the JSX Vite/Tailwind foundation" in handoff:
        fail("local-app handoff still describes the retired Template v2 scaffold")


def validate_agent_prompt_contracts(repo: pathlib.Path) -> None:
    """Pin least-privilege roles and Host-bound optimized orchestration."""
    agents_dir = repo / "crates" / "plugins" / "lingxi-local-app" / "agents"
    workflows_dir = repo / "crates" / "plugins" / "lingxi-local-app" / "workflows"

    verifier = (agents_dir / "verifier.md").read_text(encoding="utf-8")
    verifier_frontmatter = verifier.split("---", 2)[1]
    for tool in ("LocalAppGet", "LocalAppQaReadEvidence", "LocalAppQaFinalize"):
        if f"  - {tool}\n" not in verifier_frontmatter:
            fail(f"verifier.md must grant {tool}")
    for tool in (
        "LocalAppInspectUi",
        "LocalAppCaptureUi",
        "LocalAppActOnUi",
        "LocalAppMutateData",
        "LocalAppBuild",
        "LocalAppPromoteMcpCandidate",
        "LocalAppResolveTemplateSelection",
    ):
        if f"  - {tool}\n" in verifier_frontmatter:
            fail(f"verifier.md must not grant {tool}")
    if "previous_result_id must name the tester's" not in verifier:
        fail("verifier.md no longer binds its candidate to the tester Host result")

    operator = (agents_dir / "operator.md").read_text(encoding="utf-8")
    # Two halves of one invariant: the handle is checked against the pass, and
    # the evidence list is returned WHOLE. The single token this used to look
    # for ("Return the Host qa_handle and every evidence ID without
    # truncation") matched nothing in operator.md at the commit that added
    # both (f81c57261), so this check had never passed; these are the file's
    # own sentences.
    for fragment in (
        "verify that its handle matches the pass",
        "deduplicate without truncating, and\nreturn that complete list as the structured result's `evidence_ids`",
    ):
        if fragment not in operator:
            fail(
                "operator.md no longer requires complete Host evidence "
                f"identities (missing {fragment!r})"
            )
    if "do not judge scenario\npass/fail" not in operator:
        fail("operator.md no longer separates operation from QA judgement")

    tester = (agents_dir / "tester.md").read_text(encoding="utf-8")
    if "For every quality level, call LocalAppQaFinalize in this same agent pass" not in tester:
        fail("tester.md no longer Finalizes in the same pass for thorough mode")
    if "previous_result_id names this result" not in tester:
        fail("tester.md no longer anchors the thorough verifier candidate")

    # `agents/builder.md` and `agents/designer.md` were the create workflow's
    # build and design roles and are DELETED (only mcp-designer/mcp-promoter/
    # operator/tester/verifier remain), so their frontmatter and prompt pins
    # went with them. The create flow no longer spawns either role.

    #
    # The QA-candidate/disposition invariants below used to be pinned on
    # `local-app-build.js`, also DELETED. The same mechanism is live in
    # `local-app-use-test.js` (create now runs no automatic QA chain of its
    # own), so the check is RE-POINTED there rather than dropped: the tester
    # must finalize from the Host candidate, the verifier must attest the tester
    # as its predecessor, success is decided by the Host result rather than a
    # model flag, and a resample may not reuse the previous Host qa_handle.
    use_test_js = (workflows_dir / "local-app-use-test.js").read_text(encoding="utf-8")
    for forbidden in (
        "localPolicyFindings",
        "checked_matrix",
        "webview_checked",
        "frames_captured",
        "frames_compared",
        ".slice(0, 40)",
        ".slice(0, 500)",
    ):
        if forbidden in use_test_js:
            fail(f"local-app-use-test.js still trusts or truncates model QA data: {forbidden}")
    for required in (
        "const testerDisposition = finalizeDisposition(tester, 'tester', qaHandle)",
        "verifier.result.previous_result_id !== tester.receipt.result_id",
        "result.scenario_judgements.every",
        "result.findings.every",
        "evidence resample reused the previous Host qa_handle",
    ):
        if required not in use_test_js:
            fail(f"local-app-use-test.js is missing Host QA invariant: {required}")

    for name, titles in (
        ("local-app-use-test.js", ("Operate", "Test", "Verify")),
        (
            "local-app-mcp-authoring.js",
            ("Evidence and proposal", "Validate and approve", "QA and promote"),
        ),
    ):
        source = (workflows_dir / name).read_text(encoding="utf-8")
        calls = re.findall(r"phase\('([^']*)'\)", source)
        if calls != list(titles):
            fail(
                f"{name} must call phase(...) once per meta.phases title in order "
                f"{list(titles)}; found {calls}"
            )


def validate_product_model_name_absence(repo: pathlib.Path) -> None:
    """Keep the task-only model name out of product routing and generation."""
    forbidden = "gpt-5.6" + "luna"
    roots = [
        repo / "crates" / "tools",
        repo / "skills",
    ]
    for root in roots:
        if not root.is_dir():
            fail(f"required product source root missing: {root}")
        for path in root.rglob("*"):
            if not path.is_file() or any(
                part in {".git", "target", "build", "node_modules"} for part in path.parts
            ):
                continue
            try:
                if forbidden in path.read_text(encoding="utf-8"):
                    fail(f"task-only model name leaked into product source: {path}")
            except (OSError, UnicodeDecodeError):
                continue


def runtime_profile_template_root(repo: pathlib.Path) -> pathlib.Path:
    return repo / "crates" / "local-apps" / "templates" / "runtime-profiles"


# The bytes this verifier attests are read from `runtime_profile_template_root`,
# but the bytes the product SHIPS are the ones `profile_file!` in
# crates/harness-runtime/src/mobile/local_app_runtime_profiles.rs pulls in with
# `include_bytes!` from a SECOND on-disk copy under the plugin tree. An
# attestation over a tree the binary does not compile is worth nothing the
# moment the two copies drift, so the two roots are compared byte for byte and
# the compiled file list is parsed out of the macro call sites rather than
# guessed.
COMPILED_PROFILE_MACRO_SOURCE = (
    "crates",
    "harness-runtime",
    "src",
    "mobile",
    "local_app_runtime_profiles.rs",
)
COMPILED_PROFILE_ROOT_LITERAL = "/../plugins/lingxi-local-app/assets/templates/"
# The five families embed more than 100 distinct files. The floor exists so a
# regex that silently stops matching cannot report "0 files compared, all clear"
# -- a zero-hit scan is not evidence.
MIN_COMPILED_PROFILE_FILES = 100
COMPILED_PROFILE_CALL = re.compile(
    r'profile_file!\(\s*"([^"]+)"\s*,\s*"([^"]+)"\s*\)'
)


def compiled_runtime_profile_template_root(repo: pathlib.Path) -> pathlib.Path:
    return repo / "crates" / "plugins" / "lingxi-local-app" / "assets" / "templates"


def compiled_runtime_profile_files(repo: pathlib.Path) -> list[tuple[str, str]]:
    """(family, path-under-r4) pairs `include_bytes!` compiles into the engine."""
    source_path = repo.joinpath(*COMPILED_PROFILE_MACRO_SOURCE)
    try:
        source = source_path.read_text(encoding="utf-8")
    except OSError as exc:
        fail(f"cannot read the compiled runtime-profile macro source {source_path}: {exc}")
    if COMPILED_PROFILE_ROOT_LITERAL not in source:
        fail(
            f"{source_path} no longer builds its include_bytes! paths from "
            f"'{COMPILED_PROFILE_ROOT_LITERAL}' -- this verifier's idea of which tree the "
            "product compiles is stale, fix compiled_runtime_profile_template_root()"
        )
    pairs = sorted(set(COMPILED_PROFILE_CALL.findall(source)))
    if len(pairs) < MIN_COMPILED_PROFILE_FILES:
        fail(
            f"only {len(pairs)} profile_file! call site(s) parsed out of {source_path}, expected at "
            f"least {MIN_COMPILED_PROFILE_FILES} -- refusing to report a clean comparison from an "
            "enumeration this small, the scan is probably broken"
        )
    return pairs


def compare_runtime_profile_trees(
    attested_root: pathlib.Path,
    compiled_root: pathlib.Path,
    entries: list[tuple[str, str]],
    selected_profile: str | None = None,
) -> int:
    """Compare the current family files enumerated by the Rust embedding macro.

    Shared widget files are checked separately below against their compiled tree.
    """
    compared = 0
    for family, relative in entries:
        if selected_profile is not None and family != selected_profile:
            continue
        attested = attested_root / family / "r4" / relative
        compiled = compiled_root / family / "r4" / relative
        try:
            compiled_bytes = compiled.read_bytes()
        except OSError as exc:
            fail(f"compiled runtime-profile file is unreadable: {compiled}: {exc}")
        try:
            attested_bytes = attested.read_bytes()
        except OSError as exc:
            fail(
                f"the runtime-profile tree this verifier attests is missing a file the engine "
                f"compiles in: {attested} (compiled from {compiled}): {exc}"
            )
        if attested_bytes != compiled_bytes:
            fail(
                f"runtime-profile template diverged from the bytes the engine compiles: "
                f"{attested} != {compiled} -- this verifier would otherwise attest a tree the "
                "product does not ship"
            )
        compared += 1
    if compared == 0:
        fail(
            "compared 0 runtime-profile template files against the compiled tree -- an empty "
            "comparison is not an all-clear"
        )
    return compared


def validate_runtime_profile_templates_match_compiled(
    repo: pathlib.Path,
    selected_profile: str | None,
) -> None:
    compare_runtime_profile_trees(
        runtime_profile_template_root(repo),
        compiled_runtime_profile_template_root(repo),
        compiled_runtime_profile_files(repo),
        selected_profile,
    )
    source_root = runtime_profile_template_root(repo)
    compiled_root = compiled_runtime_profile_template_root(repo)
    for family in ([selected_profile] if selected_profile else RUNTIME_PROFILES):
        for relative in ("package.json", "pnpm-lock.yaml", "index.html", "vite.config.mjs", "src/main.jsx", "src/widget.jsx"):
            expected = compiled_root / "shared/mcp-widget/r4" / relative
            actual = source_root / family / "r4/app/mcp-widget" / relative
            if not actual.is_file() or actual.read_bytes() != expected.read_bytes():
                fail(f"r4 MCP widget mirror differs from compiled template: {actual}")


def expected_runtime_profile_dependencies(profile_name: str) -> dict[str, str]:
    profile = RUNTIME_PROFILES.get(profile_name)
    if profile is None:
        fail(f"unknown runtime profile: {profile_name}")
    return dict(RUNTIME_PROFILE_COMMON_DEPENDENCIES | profile["extra_dependencies"])


def expected_runtime_profile_host_managed_paths(profile_name: str) -> list[str]:
    profile = RUNTIME_PROFILES.get(profile_name)
    if profile is None:
        fail(f"unknown runtime profile: {profile_name}")
    return [
        *RUNTIME_PROFILE_HOST_MANAGED_BASE,
        *profile["host_managed_helpers"],
        *RUNTIME_PROFILE_HOST_MANAGED_SUFFIX,
    ]


def lock_contains_package(lock_text: str, name: str, version: str) -> bool:
    return re.search(
        rf"^\s*['\"]?{re.escape(name)}@{re.escape(version)}['\"]?:\s*$",
        lock_text,
        re.MULTILINE,
    ) is not None


def validate_runtime_profile_lock(
    profile_name: str,
    template: pathlib.Path,
    pins: dict,
) -> None:
    package_json = load_json(template / "package.json")
    lock_path = template / "pnpm-lock.yaml"
    lock_text = lock_path.read_text(encoding="utf-8")
    expected_dependencies = expected_runtime_profile_dependencies(profile_name)
    if package_json.get("engines") != {"node": expected_node(pins)}:
        fail(f"{profile_name} package.json must pin Node exactly")
    if package_json.get("dependencies") != expected_dependencies:
        fail(f"{profile_name} package.json dependencies diverged from the runtime-profile contract")
    if package_json.get("scripts") != EXPECTED_SCRIPTS:
        fail(f"{profile_name} package.json must expose the standard Vite scripts only")
    if "overrides" in package_json:
        fail(f"{profile_name} package.json must not carry pnpm overrides")
    if hashlib.sha256(lock_path.read_bytes()).hexdigest() != RUNTIME_PROFILE_LOCK_SHA256[profile_name]:
        fail(f"{profile_name} pnpm-lock.yaml bytes diverged from the reviewed runtime-profile lock")

    workspace_settings = load_yaml_mapping(template / "pnpm-workspace.yaml")
    expected_workspace_settings = {
        "packages": [".", "app/mcp-widget"],
        "lockfile": "true",
        "nodeLinker": "hoisted",
        "packageImportMethod": "clone-or-copy",
        "verifyStoreIntegrity": "true",
        "strictStorePkgContentCheck": "true",
        "ignoreScripts": "true",
        "preferFrozenLockfile": "true",
        "overrides": {"lightningcss": "1.33.0"},
    }
    if workspace_settings != expected_workspace_settings:
        fail(f"{profile_name} pnpm-workspace.yaml diverged from the fixed runtime-profile settings")
    if not re.search(r"^lockfileVersion:\s*['\"]?9\.0['\"]?\s*$", lock_text, re.MULTILINE):
        fail(f"{profile_name} pnpm-lock.yaml must use lockfileVersion 9")
    if "importers:" not in lock_text or "packages:" not in lock_text or "snapshots:" not in lock_text:
        fail(f"{profile_name} pnpm-lock.yaml is missing importers/packages/snapshots")
    for name, version in expected_dependencies.items():
        pattern = rf"(?ms)^\s+['\"]?{re.escape(name)}['\"]?:\s*\n\s+specifier:\s*{re.escape(version)}\b"
        if not re.search(pattern, lock_text):
            fail(f"{profile_name} pnpm-lock importer did not pin {name}@{version}")
    for name, version in RUNTIME_PROFILE_LOCK_PACKAGES.items():
        if not lock_contains_package(lock_text, name, version):
            fail(f"{profile_name} pnpm-lock did not pin {name}@{version}")


def validate_base_seed_profile_relationships(repo: pathlib.Path, pins: dict) -> None:
    base_template = repo / pins["local_app_runtime"]["template"]
    if base_template != runtime_profile_template_root(repo) / "react-dom" / "r4":
        fail("bundled local-app dependency seed must point to runtime-profiles/react-dom/r4")
    base_lock_sha = hashlib.sha256((base_template / "pnpm-lock.yaml").read_bytes()).hexdigest()
    if pins["local_app_runtime"]["lockfile_sha256"] != base_lock_sha:
        fail("bundled local-app dependency seed lock SHA diverged from the base runtime profile")

    canvas_lock_sha = hashlib.sha256(
        (runtime_profile_template_root(repo) / "canvas-2d" / "r4" / "pnpm-lock.yaml").read_bytes()
    ).hexdigest()
    if canvas_lock_sha != base_lock_sha:
        fail("react_dom and canvas_2d must share the engine-free bundled seed lock")
    for profile_name in ("three-3d", "phaser-2d", "babylon-3d"):
        profile_lock_sha = hashlib.sha256(
            (runtime_profile_template_root(repo) / profile_name / "r4" / "pnpm-lock.yaml").read_bytes()
        ).hexdigest()
        if profile_lock_sha == base_lock_sha:
            fail(f"{profile_name} must not share the engine-free bundled seed lock")

    package_json = load_json(base_template / "package.json")
    dependencies = package_json.get("dependencies", {})
    forbidden_engine_packages = {"three", "phaser", "@babylonjs/core", "@babylonjs/loaders", "@babylonjs/havok"}
    present = sorted(package for package in forbidden_engine_packages if package in dependencies)
    if present:
        fail(f"bundled local-app dependency seed must remain engine-free, found {present}")
    lock_text = (base_template / "pnpm-lock.yaml").read_text(encoding="utf-8")
    forbidden_lock_entries = []
    for package in forbidden_engine_packages:
        if re.search(rf"^\s*['\"]?{re.escape(package)}@", lock_text, re.MULTILINE):
            forbidden_lock_entries.append(package)
    if forbidden_lock_entries:
        fail(
            "bundled local-app dependency seed lock must remain engine-free, found "
            f"{sorted(forbidden_lock_entries)}"
        )
    expected_runtime = {
        "template": "crates/local-apps/templates/runtime-profiles/react-dom/r4",
        "node": expected_node(pins),
        "react": EXPECTED_DEPENDENCIES["react"],
        "react_dom": EXPECTED_DEPENDENCIES["react-dom"],
        "vite": EXPECTED_DEPENDENCIES["vite"],
        "rolldown": EXPECTED_ROLLDOWN_VERSION,
        "rolldown_bindings": EXPECTED_ROLLDOWN_BINDINGS,
        "rollup": "4.44.0",
        "rollup_bindings": EXPECTED_ROLLUP_BINDINGS,
        "lightningcss": EXPECTED_LIGHTNINGCSS_VERSION,
        "lightningcss_bindings": EXPECTED_LIGHTNINGCSS_BINDINGS,
        "lockfile": "crates/local-apps/templates/runtime-profiles/react-dom/r4/pnpm-lock.yaml",
        "lockfile_sha256": base_lock_sha,
    }
    if pins.get("local_app_runtime") != expected_runtime:
        fail("bundled local-app dependency seed pins diverged from the engine-free base profile")


def scan_runtime_profile_sources(template: pathlib.Path) -> None:
    writable_roots = set(VITE_EXPECTED_WRITABLE_ROOTS)
    allowed_top_level = {
        *writable_roots,
        ".gitignore",
        ".lingxi",
        "index.html",
        "jsconfig.json",
        "vite.config.mjs",
        "package.json",
        "pnpm-lock.yaml",
        "pnpm-workspace.yaml",
        "node_modules",
        "dist",
    }
    for path in template.rglob("*"):
        relative = path.relative_to(template)
        if relative.parts[0] in {"node_modules", "dist"}:
            continue
        if path.is_symlink():
            fail(f"symbolic links are forbidden in runtime profile sources: {relative}")
        if relative.parts[0] not in allowed_top_level:
            fail(f"runtime profile path is outside the fixed workspace roots: {relative}")
        if not path.is_file() or path.suffix not in SOURCE_SUFFIXES:
            continue
        if relative.name in FORBIDDEN_ROUTE_FILES:
            fail(f"API routes are forbidden: {relative}")
        text = path.read_text(encoding="utf-8")
        for label, pattern in FORBIDDEN_SOURCE_PATTERNS.items():
            if pattern.search(text):
                fail(f"forbidden {label} in {relative}")


def validate_runtime_profile_source_policy(profile_name: str, template: pathlib.Path) -> None:
    source_policy = load_json(template / ".lingxi" / "source-policy.json")
    if source_policy.get("schema_version") != 1:
        fail(f"{profile_name} source policy schema_version must remain 1")
    if source_policy.get("agent_writable_roots") != VITE_EXPECTED_WRITABLE_ROOTS:
        fail(f"{profile_name} source policy writable roots diverged")
    expected_host_managed = expected_runtime_profile_host_managed_paths(profile_name)
    if source_policy.get("host_managed_paths") != expected_host_managed:
        fail(f"{profile_name} source policy host-managed paths diverged")
    if set(source_policy.get("forbidden_features", [])) != {
        "arbitrary_javascript_bridge_actions",
        "direct_network_calls",
        "eval",
        "external_scripts",
        "package_install",
        "server_actions",
        "symbolic_links",
    }:
        fail(f"{profile_name} source policy forbidden features diverged")
    for helper in expected_host_managed:
        if "/" in helper and helper not in {"node_modules"} and not (template / helper).is_file():
            fail(f"{profile_name} source policy references a missing managed helper: {helper}")
    lingxi_dir = template / ".lingxi"
    lingering = sorted(
        path.relative_to(lingxi_dir).as_posix()
        for path in lingxi_dir.rglob("*")
        if path.is_file() or path.is_symlink()
    )
    if lingering != ["source-policy.json"]:
        fail(f"{profile_name} runtime profile must keep only .lingxi/source-policy.json, found {lingering}")
    config = (template / "vite.config.mjs").read_text(encoding="utf-8")
    compressed_size_values = re.findall(
        r"^[\t ]*reportCompressedSize\s*:\s*(true|false)\s*,?[\t ]*(?://.*)?$",
        config,
        flags=re.MULTILINE,
    )
    if compressed_size_values != ["false"]:
        fail(f"{profile_name} fixed Vite build must disable compressed-size reporting")
    out_dir_values = re.findall(
        r'^[\t ]*outDir\s*:\s*["\']([^"\']+)["\']\s*,?[\t ]*(?://.*)?$',
        config,
        flags=re.MULTILINE,
    )
    if out_dir_values != ["dist"]:
        fail(f"{profile_name} fixed Vite build must use the official dist output directory")
    scan_runtime_profile_sources(template)


def validate_runtime_profile_sbom(
    repo: pathlib.Path,
    profile_name: str,
    template: pathlib.Path,
) -> None:
    generator = repo / "scripts" / "mobile-linux" / "generate-local-app-sbom.py"
    with tempfile.TemporaryDirectory(prefix=f"local-app-sbom-{profile_name}-") as temp_root:
        output = pathlib.Path(temp_root) / "runtime.spdx.json"
        subprocess.run(
            [sys.executable, str(generator), "--lock", str(template / "pnpm-lock.yaml"), "--output", str(output)],
            check=True,
        )
        sbom = load_json(output)
        if sbom.get("spdxVersion") != "SPDX-2.3":
            fail(f"{profile_name} generated SBOM must remain SPDX-2.3")
        packages = sbom.get("packages")
        if not isinstance(packages, list) or len(packages) != 1:
            fail(f"{profile_name} generated SBOM must describe exactly one lockfile package")
        lock_digest = hashlib.sha256((template / "pnpm-lock.yaml").read_bytes()).hexdigest()
        package = packages[0]
        if package.get("checksums") != [{"algorithm": "SHA256", "checksumValue": lock_digest}]:
            fail(f"{profile_name} generated SBOM checksum must match pnpm-lock.yaml")
        if not str(sbom.get("documentNamespace", "")).endswith(lock_digest):
            fail(f"{profile_name} generated SBOM namespace must end with the lock digest")


def validate_runtime_profiles(
    repo: pathlib.Path,
    pins: dict,
    selected_profile: str | None,
) -> None:
    root = runtime_profile_template_root(repo)
    # Prove these files ARE the files the engine compiles in before this
    # function attests anything about them -- the two trees are separate copies
    # on disk.
    #
    # SCOPE, precisely. This is NOT the first attestation in the run: `main()`
    # already ran validate_runtime_profile_lock("react-dom"),
    # validate_runtime_profile_source_policy, validate_base_seed_profile_relationships
    # and validate_sbom over the very same tree. The guarantee is weaker and
    # still sufficient: every path out of `main()` is fail-fast, so no OVERALL
    # pass can be printed over a tree that drifted from the compiled bytes.
    # Do not read this comment as "nothing is attested before the comparison".
    validate_runtime_profile_templates_match_compiled(repo, selected_profile)
    profile_names = [selected_profile] if selected_profile else sorted(RUNTIME_PROFILES)
    for profile_name in profile_names:
        if profile_name not in RUNTIME_PROFILES:
            fail(f"unknown runtime profile: {profile_name}")
        template = root / profile_name / "r4"
        if not template.is_dir():
            fail(f"runtime profile template is missing: {template}")
        validate_runtime_profile_lock(profile_name, template, pins)
        validate_runtime_profile_source_policy(profile_name, template)
        validate_runtime_profile_sbom(repo, profile_name, template)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", required=True)
    parser.add_argument("--sdk-root")
    parser.add_argument("--release", action="store_true")
    parser.add_argument("--apk-dir")
    parser.add_argument("--profile", choices=sorted(RUNTIME_PROFILES), help="validate one runtime profile only")
    parser.add_argument("--template", help="override a single runtime profile r4 directory")
    args = parser.parse_args()

    repo = pathlib.Path(args.repo_root).resolve()
    pins = effective_pins(repo, args.sdk_root)
    validate_typescript_native_pin(repo, pins)
    validate_apk_pins(
        pins,
        release=args.release,
        apk_dir=pathlib.Path(args.apk_dir).resolve() if args.apk_dir else None,
    )
    if args.template:
        if not args.profile:
            fail("--template requires --profile so the expected runtime profile contract is known")
        template = pathlib.Path(args.template).resolve()
        validate_runtime_profile_lock(args.profile, template, pins)
        validate_runtime_profile_source_policy(args.profile, template)
        validate_runtime_profile_sbom(repo, args.profile, template)
    else:
        template = repo / pins.get("local_app_runtime", {}).get("template", "")
        validate_runtime_profile_lock("react-dom", template, pins)
        validate_runtime_profile_source_policy("react-dom", template)
        validate_base_seed_profile_relationships(repo, pins)
        validate_sbom(repo, template)
        validate_runtime_profiles(repo, pins, args.profile)
    validate_runtime_policy(repo)
    validate_create_skill(repo)
    validate_agent_prompt_contracts(repo)
    validate_product_model_name_absence(repo)
    print("local-app runtime profile supply-chain pins verified")


if __name__ == "__main__":
    main()
