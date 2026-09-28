#!/usr/bin/env python3
import argparse
import hashlib
import importlib.util
import json
import os
import pathlib
import shutil
import stat
import tempfile
import sys

# The runtime source can be an immutable Cargo Git checkout.
sys.dont_write_bytecode = True

_VERIFY_SOURCE = pathlib.Path(__file__).with_name("verify-local-app-supply-chain.py")
_VERIFY_SPEC = importlib.util.spec_from_file_location("local_app_supply_chain", _VERIFY_SOURCE)
if _VERIFY_SPEC is None or _VERIFY_SPEC.loader is None:
    raise ImportError(f"cannot load {_VERIFY_SOURCE}")
_VERIFY = importlib.util.module_from_spec(_VERIFY_SPEC)
_VERIFY_SPEC.loader.exec_module(_VERIFY)

EXPECTED_DEPENDENCIES = _VERIFY.EXPECTED_DEPENDENCIES
EXPECTED_LIGHTNINGCSS_VERSION = _VERIFY.EXPECTED_LIGHTNINGCSS_VERSION
EXPECTED_ROLLDOWN_VERSION = _VERIFY.EXPECTED_ROLLDOWN_VERSION
expected_native_binary_for = _VERIFY.expected_native_binary_for
expected_native_packages_for = _VERIFY.expected_native_packages_for
fail = _VERIFY.fail
load_json = _VERIFY.load_json
validate_apk_pins = _VERIFY.validate_apk_pins
validate_base_seed_profile_relationships = _VERIFY.validate_base_seed_profile_relationships
validate_runtime_policy = _VERIFY.validate_runtime_policy
validate_sbom = _VERIFY.validate_sbom
validate_runtime_profile_lock = _VERIFY.validate_runtime_profile_lock
validate_runtime_profile_source_policy = _VERIFY.validate_runtime_profile_source_policy


FORBIDDEN_TOP_LEVEL_PACKAGES = {"corepack", "nodejs-npm", "npm", "pnpm", "yarn"}


def sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def validate_symlinks(root: pathlib.Path) -> None:
    for path in root.rglob("*"):
        if not path.is_symlink():
            continue
        try:
            path.resolve(strict=True).relative_to(root.resolve())
        except (OSError, ValueError) as exc:
            fail(f"node_modules symlink escapes or is broken: {path}: {exc}")

def validate_node_modules(
    root: pathlib.Path,
    platform: str,
) -> None:
    if not root.is_dir() or root.is_symlink():
        fail(f"node_modules input is missing or unsafe: {root}")
    validate_symlinks(root)
    present = {path.name for path in root.iterdir() if path.is_dir()}
    forbidden = present & FORBIDDEN_TOP_LEVEL_PACKAGES
    if forbidden:
        fail(f"forbidden package managers in node_modules: {sorted(forbidden)}")
    for name, version in EXPECTED_DEPENDENCIES.items():
        package = load_json(root / name / "package.json")
        if package.get("version") != version:
            fail(f"runtime node_modules did not resolve {name}@{version}")
    if (root / "next").exists():
        fail("runtime node_modules must not retain the Next.js package")
    if (root / "@next").exists():
        fail("runtime node_modules must not retain @next SWC packages")
    if (root / "three").exists():
        fail("runtime node_modules must not retain the Three.js package in the engine-free seed")
    if (root / "phaser").exists():
        fail("runtime node_modules must not retain the Phaser package in the engine-free seed")
    if (root / "@babylonjs").exists():
        fail("runtime node_modules must not retain Babylon packages in the engine-free seed")
    # Tailwind left the pinned set with the Ionic move. Its Oxide bindings were
    # the ONLY thing validating that scope, so without this a stale seed still
    # carrying `@tailwindcss/oxide-*/*.node` stages clean and `copytree` ships
    # unpinned native code to every device — the same shape as the `@next` guard
    # above, which exists for exactly that reason.
    if (root / "@tailwindcss").exists():
        fail("runtime node_modules must not retain @tailwindcss packages")
    vite_binary = root / "vite" / "bin" / "vite.js"
    if not vite_binary.is_file() or vite_binary.is_symlink():
        fail("runtime node_modules is missing the fixed Vite CLI")
    rolldown = load_json(root / "rolldown" / "package.json")
    if rolldown.get("version") != EXPECTED_ROLLDOWN_VERSION:
        fail(f"runtime node_modules did not resolve rolldown@{EXPECTED_ROLLDOWN_VERSION}")
    lightningcss = load_json(root / "lightningcss" / "package.json")
    if lightningcss.get("version") != EXPECTED_LIGHTNINGCSS_VERSION:
        fail(f"runtime node_modules did not resolve lightningcss@{EXPECTED_LIGHTNINGCSS_VERSION}")
    # No `rollup` package check here, on purpose. The Rolldown move took the
    # rollup JS package out of the graph entirely — all five seed lockfiles
    # carry zero `rollup@` entries and two `rolldown@` ones — so asking for
    # `rollup/package.json` could only ever raise `load_json`'s "invalid JSON
    # … No such file or directory", failing every stage with a message that
    # names the wrong problem.
    #
    # 4.44.0 is still pinned, on the artifacts that actually ship: the
    # `@rollup/rollup-linux-*-musl` native bindings are checked by exact
    # version, and their single real `.node` binary by name, in the
    # `allowed_rollup_bindings` loop below. Those bindings ARE in the
    # lockfiles; the package that used to pull them in is not.
    allowed_rolldown_bindings = expected_native_packages_for(platform, "rolldown")
    allowed_rolldown_dirs = {name.removeprefix("@rolldown/") for name in allowed_rolldown_bindings}
    rolldown_roots = (
        [path for path in (root / "@rolldown").iterdir()]
        if (root / "@rolldown").is_dir()
        else []
    )
    for path in rolldown_roots:
        if path.name.startswith("binding-") and path.name not in allowed_rolldown_dirs:
            fail(f"runtime node_modules resolved an unexpected Rolldown binding: @rolldown/{path.name}")
    for name, version in allowed_rolldown_bindings.items():
        package_root = root / pathlib.PurePosixPath(name)
        package = load_json(package_root / "package.json")
        if package.get("version") != version:
            fail(f"runtime node_modules did not resolve {name}@{version}")
        binary = package_root / expected_native_binary_for(name)
        native_bindings = list(package_root.glob("*.node"))
        if (
            len(native_bindings) != 1
            or native_bindings[0].is_symlink()
            or native_bindings[0].name != binary.name
            or not binary.is_file()
        ):
            fail(f"runtime node_modules must contain one real native binding for {name}")
    allowed_rollup_bindings = expected_native_packages_for(platform, "rollup")
    allowed_rollup_dirs = {name.removeprefix("@rollup/") for name in allowed_rollup_bindings}
    rollup_roots = (
        [path for path in (root / "@rollup").iterdir()]
        if (root / "@rollup").is_dir()
        else []
    )
    for path in rollup_roots:
        if path.name.startswith("rollup-linux-") and path.name not in allowed_rollup_dirs:
            fail(f"runtime node_modules resolved an unexpected Rollup binding: @rollup/{path.name}")
    for name, version in allowed_rollup_bindings.items():
        package_root = root / pathlib.PurePosixPath(name)
        package = load_json(package_root / "package.json")
        if package.get("version") != version:
            fail(f"runtime node_modules did not resolve {name}@{version}")
        binary = package_root / expected_native_binary_for(name)
        native_bindings = list(package_root.glob("*.node"))
        if (
            len(native_bindings) != 1
            or native_bindings[0].is_symlink()
            or native_bindings[0].name != binary.name
            or not binary.is_file()
        ):
            fail(f"runtime node_modules must contain one real native binding for {name}")
    allowed_lightningcss_bindings = expected_native_packages_for(platform, "lightningcss")
    lightningcss_roots = [path for path in root.iterdir() if path.is_dir()]
    for path in lightningcss_roots:
        if (
            path.name.startswith("lightningcss-")
            and path.name not in allowed_lightningcss_bindings
        ):
            fail(f"runtime node_modules resolved an unexpected Lightning CSS binding: {path.name}")
    for name, version in allowed_lightningcss_bindings.items():
        package_root = root / name
        package = load_json(package_root / "package.json")
        if package.get("version") != version:
            fail(f"runtime node_modules did not resolve {name}@{version}")
        binary = package_root / expected_native_binary_for(name)
        native_bindings = list(package_root.glob("*.node"))
        if (
            len(native_bindings) != 1
            or native_bindings[0].is_symlink()
            or native_bindings[0].name != binary.name
            or not binary.is_file()
        ):
            fail(f"runtime node_modules must contain one real native binding for {name}")
    allowed_native_bindings = {
        (root / pathlib.PurePosixPath(name) / expected_native_binary_for(name)).resolve()
        for name in (
            *allowed_rolldown_bindings.keys(),
            *allowed_rollup_bindings.keys(),
            *allowed_lightningcss_bindings.keys(),
        )
    }
    for path in root.rglob("*.node"):
        if path.is_symlink():
            fail(f"runtime node_modules native binding must be a real file: {path}")
        if path.resolve() not in allowed_native_bindings:
            fail(f"runtime node_modules contains an unexpected native binding: {path}")
    bin_dir = root / ".bin"
    for name in ("corepack", "npm", "npx", "pnpm", "yarn"):
        path = bin_dir / name
        if path.exists() or path.is_symlink():
            fail(f"forbidden package-manager executable in node_modules: {path}")


def make_read_only(root: pathlib.Path) -> None:
    for current_root, directories, files in os.walk(root, topdown=False, followlinks=False):
        current = pathlib.Path(current_root)
        for filename in files:
            path = current / filename
            if path.is_symlink():
                continue
            mode = stat.S_IMODE(path.stat().st_mode)
            path.chmod(0o555 if mode & 0o111 else 0o444)
        for dirname in directories:
            path = current / dirname
            if not path.is_symlink():
                path.chmod(0o555)
        current.chmod(0o555)
    # Keep the staging root writable by its owner so it can be atomically
    # renamed or replaced. Bundled descendants remain immutable host inputs;
    # builds copy them into a disposable project rather than mounting them.
    root.chmod(0o755)


def inventory(root: pathlib.Path) -> list[dict]:
    entries = []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            target = os.readlink(path)
            entries.append(
                {
                    "path": relative,
                    "kind": "symlink",
                    "sha256": hashlib.sha256(target.encode("utf-8")).hexdigest(),
                    "size_bytes": len(target.encode("utf-8")),
                }
            )
        elif path.is_file():
            entries.append(
                {
                    "path": relative,
                    "kind": "file",
                    "sha256": sha256(path),
                    "size_bytes": path.stat().st_size,
                }
            )
    return entries


def remove_tree(path: pathlib.Path) -> None:
    if path.is_symlink():
        path.unlink()
        return
    # Unlink needs a writable parent, not a writable target. Prepare only real
    # directories before removal: .bin links may already be dangling by the
    # time rmtree reaches them, and chmod must never follow an external link.
    path.chmod(0o700)
    for directory, children, _files in os.walk(path, topdown=True, followlinks=False):
        for name in children:
            child = pathlib.Path(directory) / name
            if not child.is_symlink():
                child.chmod(0o700)
    shutil.rmtree(path)


def assert_safe_output(repo: pathlib.Path, output: pathlib.Path) -> None:
    resolved = output.resolve()
    # Source checkouts may be Cargo's immutable Git cache. Only the standalone
    # build tree or an explicit external output directory can be replaced.
    if resolved == repo or (resolved.is_relative_to(repo) and not resolved.is_relative_to(repo / "build")):
        fail(f"runtime staging output cannot overwrite the source checkout: {output}")
    if resolved == pathlib.Path(resolved.anchor):
        fail("runtime staging output cannot be a filesystem root")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--repo-root", required=True)
    parser.add_argument("--sdk-root")
    parser.add_argument("--node-modules", required=True)
    parser.add_argument("--output", "--output-dir", required=True)
    parser.add_argument("--platform", choices=("android", "ios"), required=True)
    parser.add_argument("--variant", required=True)
    args = parser.parse_args()

    repo = pathlib.Path(args.repo_root).resolve()
    node_modules = pathlib.Path(args.node_modules).resolve()
    output = pathlib.Path(args.output)
    assert_safe_output(repo, output)

    pins = _VERIFY.effective_pins(repo, args.sdk_root)
    template = repo / pins["local_app_runtime"]["template"]
    validate_apk_pins(pins, release=False, apk_dir=None)
    validate_runtime_profile_lock("react-dom", template, pins)
    validate_runtime_profile_source_policy("react-dom", template)
    validate_base_seed_profile_relationships(repo, pins)
    validate_sbom(repo, template)
    validate_runtime_policy(repo)
    allowed_rolldown_bindings = expected_native_packages_for(args.platform, "rolldown")
    allowed_rollup_bindings = expected_native_packages_for(args.platform, "rollup")
    allowed_lightningcss_bindings = expected_native_packages_for(args.platform, "lightningcss")
    validate_node_modules(node_modules, args.platform)

    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = pathlib.Path(tempfile.mkdtemp(prefix=f".{output.name}.", dir=output.parent))
    try:
        # Android assets do not preserve Unix symlinks when AAPT packages and
        # AssetManager extracts them. Dereference only after validating every
        # source link stays inside node_modules; iOS bundles can preserve the
        # original, already-validated link topology.
        preserve_symlinks = args.platform == "ios"
        shutil.copytree(
            node_modules,
            temporary / "node_modules",
            symlinks=preserve_symlinks,
        )
        shutil.copytree(
            template,
            temporary / "template",
            symlinks=preserve_symlinks,
        )
        shutil.copy2(
            repo / "docs" / "mobile-linux" / "local-app-runtime-policy.json",
            temporary / "runtime-policy.json",
        )
        (temporary / "runtime-pins.json").write_text(json.dumps(pins, indent=2) + "\n")
        shutil.copy2(
            repo / "docs" / "mobile-linux" / "sbom" / "local-app-runtime.spdx.json",
            temporary / "runtime.spdx.json",
        )
        manifest = {
            "schema_version": 1,
            "platform": args.platform,
            "variant": args.variant,
            "read_only": True,
            "pnpm_lock_sha256": sha256(template / "pnpm-lock.yaml"),
            "resolved_rolldown_bindings": sorted(allowed_rolldown_bindings),
            "resolved_rollup_bindings": sorted(allowed_rollup_bindings),
            "resolved_lightningcss_bindings": sorted(allowed_lightningcss_bindings),
            "files": inventory(temporary),
        }
        (temporary / "runtime-manifest.json").write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        make_read_only(temporary)
        if output.exists():
            remove_tree(output)
        os.replace(temporary, output)
    finally:
        if temporary.exists():
            remove_tree(temporary)

    print(f"staged verified read-only local-app runtime seed: {output}")


if __name__ == "__main__":
    main()
