#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
TOOL="${SCRIPT_DIR}/verify-local-app-supply-chain.py"
# The runtime-profile templates live in TWO on-disk copies: this one, which
# every supply-chain assertion below is written against, and
# crates/plugins/lingxi-local-app/assets/templates/, which is what
# `profile_file!` actually `include_bytes!`es into the engine. They are held
# byte-identical by `compare_runtime_profile_trees` in the verifier (proved
# red and green further down), so copying a fixture from here is copying the
# bytes the product ships.
PROFILE_ROOT="${REPO_ROOT}/crates/local-apps/templates/runtime-profiles"
REACT_PROFILE="${PROFILE_ROOT}/react-dom/r4"
CANVAS_PROFILE="${PROFILE_ROOT}/canvas-2d/r4"
TEMP_ROOT="$(mktemp -d)"

# A negative test must fail for the reason it names, not merely fail.
#
# `if cmd; then echo "expected ..."; exit 1; fi` cannot tell "rejected the bad
# input" from "crashed before it ever looked at the input": `fail()` raises
# SystemExit(1) and an uncaught Python exception also exits 1, while `if`
# suspends `set -e` around both. That is not hypothetical -- a NameError
# introduced while editing the verifier made every one of these cases report
# success, and this suite printed "tests passed".
#
# Exit code 1 AND no traceback is the discriminator: a usage error exits 2, a
# crash prints a traceback, and only a real rejection is a silent exit 1.
expect_rejection() {
  local label="$1"
  shift
  local output status
  set +e
  output="$("$@" 2>&1)"
  status=$?
  set -e
  if [[ "${status}" -eq 0 ]]; then
    echo "expected ${label}, but the command succeeded" >&2
    printf '%s\n' "${output}" >&2
    exit 1
  fi
  if printf '%s' "${output}" | grep -q "Traceback (most recent call last)"; then
    echo "expected ${label}, but the command CRASHED instead of rejecting" >&2
    printf '%s\n' "${output}" >&2
    exit 1
  fi
  if [[ "${status}" -ne 1 ]]; then
    echo "expected ${label} to exit 1, got ${status}" >&2
    printf '%s\n' "${output}" >&2
    exit 1
  fi
}
STAGED_OUTPUT="${LINGXI_RUNTIME_TEST_OUTPUT_DIR:-${REPO_ROOT}/build}/local-app-supply-chain-test-${RANDOM}"
IOS_STAGED_OUTPUT="${LINGXI_RUNTIME_TEST_OUTPUT_DIR:-${REPO_ROOT}/build}/local-app-supply-chain-test-${RANDOM}"
trap 'chmod -R u+w "${TEMP_ROOT}" "${STAGED_OUTPUT}" "${IOS_STAGED_OUTPUT}" 2>/dev/null || true; rm -rf "${TEMP_ROOT}" "${STAGED_OUTPUT}" "${IOS_STAGED_OUTPUT}"' EXIT

python3 "${TOOL}" --repo-root "${REPO_ROOT}"
python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile babylon-3d

# `validate_node_modules` must accept a rollup-FREE tree and must still pin the
# `@rollup/*` native bindings at 4.44.0.
#
# These two belong together. The staging validator used to open
# `rollup/package.json` before it reached the binding loop, and the Rolldown
# move took that package out of the graph — all five seed lockfiles carry zero
# `rollup@` entries — so every stage aborted in `load_json` with "invalid JSON
# … No such file or directory". That did not merely block staging: it made the
# 4.44.0 binding pin below it UNREACHABLE, so the pin that actually protects
# the shipped `.node` files was never evaluated at all.
#
# The fixture is synthetic on purpose: exercising this through a real staged
# tree needs the build container, which is exactly what the bug prevented.
python3 - "${SCRIPT_DIR}/stage-local-app-runtime.py" <<'PY'
import importlib.util
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile

stage_path = pathlib.Path(sys.argv[1])
spec = importlib.util.spec_from_file_location("stage_local_app_runtime", stage_path)
stage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stage)
verify = stage._VERIFY

# Replacement/cleanup of a read-only staged tree must unlink .bin links even
# after their internal target disappeared, and never chmod external targets.
with tempfile.TemporaryDirectory() as temporary:
    base = pathlib.Path(temporary)
    external = base / "external"
    external.mkdir()
    payload = external / "payload"
    payload.write_text("outside tree remains unchanged")
    payload.chmod(0o400)
    external.chmod(0o500)
    tree = base / "staged"
    bins = tree / "node_modules/.bin"
    bins.mkdir(parents=True)
    (bins / "dangling").symlink_to("../removed-package/bin/tool")
    (bins / "external-file").symlink_to(payload)
    (bins / "external-directory").symlink_to(external, target_is_directory=True)
    package = tree / "node_modules/package"
    package.mkdir()
    (package / "tool").write_text("internal tool")
    (bins / "internal").symlink_to("../package/tool")
    for directory in (bins, package, tree / "node_modules", tree):
        directory.chmod(0o500)
    try:
        stage.remove_tree(tree)
        assert not tree.exists(), "read-only staged tree was not fully removed"
        assert payload.read_text() == "outside tree remains unchanged"
        assert payload.stat().st_mode & 0o777 == 0o400, "external file permissions changed"
        assert external.stat().st_mode & 0o777 == 0o500, "external directory permissions changed"
        root_link = base / "root-link"
        root_link.symlink_to(external, target_is_directory=True)
        stage.remove_tree(root_link)
        assert not root_link.is_symlink() and external.exists(), "root symlink followed instead of unlinked"
        dangling_root = base / "dangling-root"
        dangling_root.symlink_to(base / "absent")
        stage.remove_tree(dangling_root)
        assert not dangling_root.is_symlink(), "dangling root link retained"
    finally:
        # The external fixture is ours, but remove_tree itself must not touch it.
        external.chmod(0o700)
        payload.chmod(0o600)
        if tree.exists():
            for directory, _children, _files in os.walk(tree):
                pathlib.Path(directory).chmod(0o700)
print("read-only staging cleanup: dangling/internal/external symlinks handled safely")


def write_package(root, name, version):
    directory = root / pathlib.PurePosixPath(name)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "package.json").write_text(
        json.dumps({"name": name, "version": version}), encoding="utf-8"
    )
    return directory


def build_tree(root, rollup_binding_version):
    for name, version in stage.EXPECTED_DEPENDENCIES.items():
        write_package(root, name, version)
    write_package(root, "rolldown", verify.EXPECTED_ROLLDOWN_VERSION)
    write_package(root, "lightningcss", verify.EXPECTED_LIGHTNINGCSS_VERSION)
    vite = root / "vite" / "bin"
    vite.mkdir(parents=True, exist_ok=True)
    (vite / "vite.js").write_text("// cli\n", encoding="utf-8")
    # ios stages the arm64-musl slice only.
    natives = {
        "@rolldown/binding-linux-arm64-musl": verify.EXPECTED_ROLLDOWN_BINDINGS[
            "@rolldown/binding-linux-arm64-musl"
        ],
        "@rollup/rollup-linux-arm64-musl": rollup_binding_version,
        "lightningcss-linux-arm64-musl": verify.EXPECTED_LIGHTNINGCSS_BINDINGS[
            "lightningcss-linux-arm64-musl"
        ],
    }
    for name, version in natives.items():
        directory = write_package(root, name, version)
        (directory / verify.EXPECTED_NATIVE_PACKAGE_BINARIES[name]).write_bytes(b"\x00native\x00")
    if (root / "rollup").exists():
        raise SystemExit("fixture bug: the tree must NOT contain a rollup package")


def run(rollup_binding_version):
    temp = pathlib.Path(tempfile.mkdtemp())
    try:
        root = temp / "node_modules"
        root.mkdir()
        build_tree(root, rollup_binding_version)
        code = (
            "import importlib.util,pathlib\n"
            f"spec=importlib.util.spec_from_file_location('s',{str(stage_path)!r})\n"
            "m=importlib.util.module_from_spec(spec); spec.loader.exec_module(m)\n"
            f"m.validate_node_modules(pathlib.Path({str(root)!r}),'ios')\n"
        )
        done = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True)
        return done.returncode, done.stdout + done.stderr
    finally:
        shutil.rmtree(temp, ignore_errors=True)


status, output = run("4.44.0")
if status != 0:
    raise SystemExit(
        "a node_modules with no rollup package and correct bindings must "
        f"validate, got exit {status}: {output}"
    )

status, output = run("4.43.0")
if status == 0:
    raise SystemExit("a wrong @rollup binding version must be rejected, but it validated")
if "Traceback (most recent call last)" in output:
    raise SystemExit(f"the rejection must not be a crash: {output}")
# The point of the assertion: the rejection has to name the binding and the
# pin. Merely exiting non-zero is what the old code did, for the wrong reason.
if "@rollup/rollup-linux-arm64-musl@4.44.0" not in output:
    raise SystemExit(
        "the rejection must name @rollup/rollup-linux-arm64-musl@4.44.0, "
        f"got: {output}"
    )
print("node_modules staging: rollup-free tree accepted, @rollup 4.44.0 pin still enforced")
PY

python3 - "${REPO_ROOT}/docs/mobile-linux/local-app-runtime-policy.json" <<'PY'
import json
import pathlib
import sys

policy = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
assert "next_executable" not in policy
assert "node_modules_mount" not in policy
assert "scaffold" not in policy
assert "package_manager_policy" not in policy
assert policy["vite_executable"] == "/var/lingxi/local-app-build/{app_id}/{channel}/project/node_modules/vite/bin/vite.js"
dependency_snapshot = policy["dependency_snapshot"]
assert dependency_snapshot["source"] == "embedded:runtime-profiles/react-dom/r4/pnpm-lock.yaml"
assert dependency_snapshot["materialize_into"] == "/var/lingxi/local-app-build/{app_id}/{channel}/project/node_modules"
assert dependency_snapshot["guest_mount"] == "forbidden"
assert dependency_snapshot["selection_policy"] == "exact_lock_only"
assert "pnpm install --frozen-lockfile --ignore-scripts --no-runtime --prefer-offline" in dependency_snapshot["install_command"]
assert policy["build_mount"] == {
    "kind": "LocalAppBuild",
    "count": 1,
    "host_path_policy": "workspace_or_staging_or_store_root",
    "guest_path": "/var/lingxi/local-app-build/{app_id}/{channel}/project",
    "writable": True,
}
commands = policy["commands"]
assert sorted(commands) == ["vite_static_build"]
command = commands["vite_static_build"]
assert command["network_policy"] == "disabled"
assert command["memory_limit_policy"] == "physical_memory_tier"
assert "memory_limit_bytes" not in command
assert command["argv"][1] == "--max-old-space-size={build_node_old_space_size_mib}"
assert command["argv"][2] == "/var/lingxi/local-app-build/{app_id}/{channel}/project/node_modules/vite/bin/vite.js"
assert command["argv"][-4:] == ["build", "--outDir", "dist", "--emptyOutDir"]
assert command["cwd"] == "/var/lingxi/local-app-build/{app_id}/{channel}/project"
assert command["output_dir"] == "dist"
assert command["environment"] == {
    "NODE_ENV": "production",
    "HOME": "/var/lingxi/local-app-build/{app_id}/{channel}/project/.lingxi-build-state/home",
    "TMPDIR": "/var/lingxi/local-app-build/{app_id}/{channel}/project/.lingxi-build-state/tmp",
    "TMP": "/var/lingxi/local-app-build/{app_id}/{channel}/project/.lingxi-build-state/tmp",
    "TEMP": "/var/lingxi/local-app-build/{app_id}/{channel}/project/.lingxi-build-state/tmp",
    "XDG_CACHE_HOME": "/var/lingxi/local-app-build/{app_id}/{channel}/project/.lingxi-build-state/xdg-cache",
    "XDG_CONFIG_HOME": "/var/lingxi/local-app-build/{app_id}/{channel}/project/.lingxi-build-state/xdg-config",
    "XDG_DATA_HOME": "/var/lingxi/local-app-build/{app_id}/{channel}/project/.lingxi-build-state/xdg-data",
}
limits = policy["limits"]
assert limits["build_node_old_space_percent"] == 75
assert limits["build_memory_tiers"] == [
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
assert limits["runtime_process_tree_memory_bytes"] == 838860800
assert "node_process_tree_memory_bytes" not in limits

PY

cp -R "${REACT_PROFILE}" "${TEMP_ROOT}/react-dom-compressed-size"
python3 - "${TEMP_ROOT}/react-dom-compressed-size/vite.config.mjs" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
config = path.read_text(encoding="utf-8")
if "reportCompressedSize: false" not in config:
    raise SystemExit("fixed Vite config is missing the compressed-size policy fixture")
path.write_text(
    config.replace(
        "reportCompressedSize: false",
        "reportCompressedSize: true,\n    // reportCompressedSize: false",
    ),
    encoding="utf-8",
)
PY
expect_rejection "Vite compressed-size reporting to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile react-dom \
  --template "${TEMP_ROOT}/react-dom-compressed-size"

python3 "${SCRIPT_DIR}/generate-local-app-sbom.py" \
  --lock "${REACT_PROFILE}/pnpm-lock.yaml" \
  --output "${TEMP_ROOT}/local-app-runtime.spdx.json"
cmp "${TEMP_ROOT}/local-app-runtime.spdx.json" \
  "${REPO_ROOT}/docs/mobile-linux/sbom/local-app-runtime.spdx.json"

expect_rejection "release validation to remain fail-closed" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --release

cp -R "${REACT_PROFILE}" "${TEMP_ROOT}/react-dom-dependency-drift"
python3 - "${TEMP_ROOT}/react-dom-dependency-drift/package.json" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
value = json.loads(path.read_text(encoding="utf-8"))
value["dependencies"]["vite"] = "8.2.2"
path.write_text(json.dumps(value), encoding="utf-8")
PY
expect_rejection "dependency drift to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile react-dom \
  --template "${TEMP_ROOT}/react-dom-dependency-drift"

cp -R "${REACT_PROFILE}" "${TEMP_ROOT}/react-dom-network-bypass"
printf '\nfetch("https://example.com");\n' >> "${TEMP_ROOT}/react-dom-network-bypass/app/main.jsx"
expect_rejection "direct network access to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile react-dom \
  --template "${TEMP_ROOT}/react-dom-network-bypass"

cp -R "${REACT_PROFILE}" "${TEMP_ROOT}/react-dom-symlink"
ln -s /tmp "${TEMP_ROOT}/react-dom-symlink/public/escape"
expect_rejection "a template symlink to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile react-dom \
  --template "${TEMP_ROOT}/react-dom-symlink"

cp -R "${REACT_PROFILE}" "${TEMP_ROOT}/react-dom-path-escape"
printf 'console.log("outside policy");\n' > "${TEMP_ROOT}/react-dom-path-escape/server.js"
expect_rejection "a source file outside writable roots to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile react-dom \
  --template "${TEMP_ROOT}/react-dom-path-escape"

cp -R "${REACT_PROFILE}" "${TEMP_ROOT}/react-dom-lock-drift"
printf '\n' >> "${TEMP_ROOT}/react-dom-lock-drift/pnpm-lock.yaml"
expect_rejection "pnpm-lock byte drift to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile react-dom \
  --template "${TEMP_ROOT}/react-dom-lock-drift"

# Everything above validates the copy under
# crates/local-apps/templates/runtime-profiles, but the bytes the engine
# SHIPS come from a SECOND on-disk copy: `profile_file!` in
# crates/harness-runtime/src/mobile/local_app_runtime_profiles.rs
# `include_bytes!`es crates/plugins/lingxi-local-app/assets/templates/.
# `compare_runtime_profile_trees` is what makes the attestation cover the
# shipped bytes, so it gets its own red-and-green proof: exercised directly on
# temp trees, because the only other way to make it go red is to corrupt the
# real repository.
python3 - "${TOOL}" "${REPO_ROOT}" <<'PY'
import contextlib
import importlib.util
import io
import pathlib
import shutil
import sys
import tempfile

tool_path = pathlib.Path(sys.argv[1])
repo = pathlib.Path(sys.argv[2])
spec = importlib.util.spec_from_file_location("verify_local_app_supply_chain", tool_path)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


def expect_fail(label, fn):
    buf = io.StringIO()
    try:
        with contextlib.redirect_stderr(buf):
            fn()
    except SystemExit as exc:
        if exc.code != 1:
            raise SystemExit(f"{label}: expected exit 1, got {exc.code}")
        return buf.getvalue()
    raise SystemExit(f"{label}: expected a rejection, but it passed")


# The compiled file list must come out of the macro call sites, and must be
# big enough that a broken regex cannot masquerade as an all-clear.
entries = mod.compiled_runtime_profile_files(repo)
if len(entries) < mod.MIN_COMPILED_PROFILE_FILES:
    raise SystemExit(f"expected >= {mod.MIN_COMPILED_PROFILE_FILES} profile_file! entries, got {len(entries)}")
families = {family for family, _ in entries}
if families != {"babylon-3d", "canvas-2d", "phaser-2d", "react-dom", "three-3d"}:
    raise SystemExit(f"unexpected compiled runtime-profile families: {sorted(families)}")

compiled_root = mod.compiled_runtime_profile_template_root(repo)
sample_family, sample_rel = next((f, r) for f, r in entries if f == "react-dom")

with tempfile.TemporaryDirectory() as tmp:
    tmp = pathlib.Path(tmp)
    attested = tmp / "attested"
    for family in sorted(families):
        shutil.copytree(compiled_root / family, attested / family)

    # GREEN: an identical copy compares clean, and compares every entry.
    compared = mod.compare_runtime_profile_trees(attested, compiled_root, entries)
    if compared != len(entries):
        raise SystemExit(f"expected {len(entries)} files compared, got {compared}")

    # RED 1: one byte of drift is named by path.
    drifted = attested / sample_family / "r4" / sample_rel
    drifted.write_bytes(drifted.read_bytes() + b"\n")
    message = expect_fail(
        "byte drift between the attested and compiled template trees",
        lambda: mod.compare_runtime_profile_trees(attested, compiled_root, entries),
    )
    if str(drifted) not in message or "diverged from the bytes the engine compiles" not in message:
        raise SystemExit(f"drift rejection did not name the file: {message!r}")

    # RED 2: a file the engine compiles that the attested tree does not have.
    drifted.unlink()
    message = expect_fail(
        "a compiled template file missing from the attested tree",
        lambda: mod.compare_runtime_profile_trees(attested, compiled_root, entries),
    )
    if str(drifted) not in message:
        raise SystemExit(f"missing-file rejection did not name the file: {message!r}")

    # RED 3: an empty comparison is not an all-clear.
    expect_fail(
        "an empty runtime-profile comparison",
        lambda: mod.compare_runtime_profile_trees(attested, compiled_root, entries, "no-such-family"),
    )

    # RED 4: the macro no longer builds its paths from the plugin tree, so this
    # verifier's idea of which bytes ship is stale.
    fake_repo = tmp / "fake-repo"
    macro_source = fake_repo.joinpath(*mod.COMPILED_PROFILE_MACRO_SOURCE)
    macro_source.parent.mkdir(parents=True)
    macro_source.write_text(
        (repo.joinpath(*mod.COMPILED_PROFILE_MACRO_SOURCE)).read_text(encoding="utf-8").replace(
            mod.COMPILED_PROFILE_ROOT_LITERAL, "/../../somewhere-else/"
        ),
        encoding="utf-8",
    )
    message = expect_fail(
        "a moved include_bytes! root",
        lambda: mod.compiled_runtime_profile_files(fake_repo),
    )
    if mod.COMPILED_PROFILE_ROOT_LITERAL not in message:
        raise SystemExit(f"moved-root rejection did not name the literal: {message!r}")

print("compiled runtime-profile tree comparison: green and red both proven")
PY

python3 - "${REACT_PROFILE}" "${CANVAS_PROFILE}" "${PROFILE_ROOT}/three-3d/r4" "${PROFILE_ROOT}/phaser-2d/r4" "${PROFILE_ROOT}/babylon-3d/r4" <<'PY'
import hashlib
import json
import pathlib
import sys

react = pathlib.Path(sys.argv[1])
canvas = pathlib.Path(sys.argv[2])
three = pathlib.Path(sys.argv[3])
phaser = pathlib.Path(sys.argv[4])
babylon = pathlib.Path(sys.argv[5])

def lock_sha(path: pathlib.Path) -> str:
    return hashlib.sha256((path / "pnpm-lock.yaml").read_bytes()).hexdigest()

react_sha = lock_sha(react)
assert react_sha == lock_sha(canvas)
assert react_sha != lock_sha(three)
assert react_sha != lock_sha(phaser)
assert react_sha != lock_sha(babylon)
deps = json.loads((react / "package.json").read_text(encoding="utf-8"))["dependencies"]
for forbidden in ["three", "phaser", "@babylonjs/core", "@babylonjs/loaders", "@babylonjs/havok"]:
    assert forbidden not in deps, forbidden
PY

cp -R "${REACT_PROFILE}" "${TEMP_ROOT}/react-dom-engine-lock-drift"
python3 - "${TEMP_ROOT}/react-dom-engine-lock-drift/pnpm-lock.yaml" <<'PY'
import pathlib
import sys
path = pathlib.Path(sys.argv[1])
text = path.read_text(encoding="utf-8")
needle = "@rollup/rollup-linux-x64-musl@4.44.0"
assert needle in text
path.write_text(text.replace(needle, needle + "-drift", 1), encoding="utf-8")
PY
expect_rejection "a base seed lock containing engine/runtime drift to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile react-dom \
  --template "${TEMP_ROOT}/react-dom-engine-lock-drift"

cp -R "${REACT_PROFILE}" "${TEMP_ROOT}/react-dom-external-script"
python3 - "${TEMP_ROOT}/react-dom-external-script/index.html" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
path.write_text(
    path.read_text(encoding="utf-8").replace(
        "</body>",
        '    <script src="https://example.com/escape.js"></script>\n  </body>',
    ),
    encoding="utf-8",
)
PY
expect_rejection "an external script tag to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile react-dom \
  --template "${TEMP_ROOT}/react-dom-external-script"

cp -R "${CANVAS_PROFILE}" "${TEMP_ROOT}/canvas-missing-frame-loop"
python3 - "${TEMP_ROOT}/canvas-missing-frame-loop/.lingxi/source-policy.json" <<'PY'
import json
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
value = json.loads(path.read_text(encoding="utf-8"))
value["host_managed_paths"] = [
    entry for entry in value["host_managed_paths"] if entry != "lib/frame-loop.js"
]
path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")
PY
expect_rejection "a canvas runtime profile missing frame-loop in source policy to fail validation" \
  python3 "${TOOL}" --repo-root "${REPO_ROOT}" --profile canvas-2d \
  --template "${TEMP_ROOT}/canvas-missing-frame-loop"

NODE_MODULES="${TEMP_ROOT}/node_modules"
mkdir -p "${NODE_MODULES}"
python3 - "${NODE_MODULES}" "${TOOL}" <<'PY'
import importlib.util
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
source = pathlib.Path(sys.argv[2])
spec = importlib.util.spec_from_file_location("local_app_supply_chain", source)
if spec is None or spec.loader is None:
    raise SystemExit(f"cannot load {source}")
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)
packages = dict(verify.EXPECTED_DEPENDENCIES)
packages.update({
    "rolldown": "1.2.9",
    "rollup": "4.44.0",
    "lightningcss": "1.33.0",
    "@rolldown/binding-linux-arm64-musl": "1.2.9",
    "@rolldown/binding-linux-x64-musl": "1.2.9",
    "@rollup/rollup-linux-arm64-musl": "4.44.0",
    "@rollup/rollup-linux-x64-musl": "4.44.0",
    "lightningcss-linux-arm64-musl": "1.33.0",
    "lightningcss-linux-x64-musl": "1.33.0",
})
for name, version in packages.items():
    package = root.joinpath(*name.split("/"))
    package.mkdir(parents=True, exist_ok=True)
    (package / "package.json").write_text(
        json.dumps({"name": name, "version": version}),
        encoding="utf-8",
    )
(root / "vite/bin").mkdir(parents=True, exist_ok=True)
(root / "vite/bin/vite.js").write_text("#!/usr/bin/env node\n", encoding="utf-8")
(root / "@rolldown/binding-linux-arm64-musl/rolldown-binding.linux-arm64-musl.node").write_bytes(bytes.fromhex("7f454c46") + b"fixture")
(root / "@rolldown/binding-linux-x64-musl/rolldown-binding.linux-x64-musl.node").write_bytes(bytes.fromhex("7f454c46") + b"fixture")
(root / "@rollup/rollup-linux-arm64-musl/rollup.linux-arm64-musl.node").write_bytes(bytes.fromhex("7f454c46") + b"fixture")
(root / "@rollup/rollup-linux-x64-musl/rollup.linux-x64-musl.node").write_bytes(bytes.fromhex("7f454c46") + b"fixture")
(root / "lightningcss-linux-arm64-musl/lightningcss.linux-arm64-musl.node").write_bytes(bytes.fromhex("7f454c46") + b"fixture")
(root / "lightningcss-linux-x64-musl/lightningcss.linux-x64-musl.node").write_bytes(bytes.fromhex("7f454c46") + b"fixture")
PY
python3 "${SCRIPT_DIR}/stage-local-app-runtime.py" \
  --repo-root "${REPO_ROOT}" \
  --node-modules "${NODE_MODULES}" \
  --output "${STAGED_OUTPUT}" \
  --platform android \
  --variant play
test -f "${STAGED_OUTPUT}/runtime-manifest.json"
test ! -w "${STAGED_OUTPUT}/node_modules/vite/package.json"
test ! -e "${STAGED_OUTPUT}/node_modules/@next"
test ! -e "${STAGED_OUTPUT}/node_modules/@tailwindcss"

# Tailwind left the pinned set with the move to Ionic, taking the Oxide-binding
# validation with it -- and nothing replaced it, so a stale seed still carrying
# `@tailwindcss/oxide-*/*.node` staged clean and `copytree` shipped unpinned
# native code to every device. Asserting the manifest KEY is gone (below) only
# asserts itself; this drives a tree that actually has the packages.
TAILWIND_NODE_MODULES="${TEMP_ROOT}/node_modules-tailwind"
cp -R "${NODE_MODULES}" "${TAILWIND_NODE_MODULES}"
mkdir -p "${TAILWIND_NODE_MODULES}/@tailwindcss/oxide-linux-arm64-musl"
printf '{"name":"@tailwindcss/oxide-linux-arm64-musl","version":"4.3.3"}\n' \
  > "${TAILWIND_NODE_MODULES}/@tailwindcss/oxide-linux-arm64-musl/package.json"
expect_rejection "a stale Tailwind Oxide binding to fail staging" \
  python3 "${SCRIPT_DIR}/stage-local-app-runtime.py" \
  --repo-root "${REPO_ROOT}" \
  --node-modules "${TAILWIND_NODE_MODULES}" \
  --output "${TEMP_ROOT}/staged-tailwind" \
  --platform android \
  --variant play
python3 - "${STAGED_OUTPUT}/runtime-manifest.json" <<'PY'
import json
import pathlib
import sys

manifest = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
assert manifest["resolved_rolldown_bindings"] == [
    "@rolldown/binding-linux-arm64-musl",
    "@rolldown/binding-linux-x64-musl",
], manifest
assert manifest["resolved_rollup_bindings"] == [
    "@rollup/rollup-linux-arm64-musl",
    "@rollup/rollup-linux-x64-musl",
], manifest
assert manifest["resolved_lightningcss_bindings"] == [
    "lightningcss-linux-arm64-musl",
    "lightningcss-linux-x64-musl",
], manifest
# Tailwind left the pinned set with the move to Ionic. Asserted absent rather
# than simply unchecked: a stale seed that still carried the Oxide bindings
# would otherwise stage clean and ship dead native code to every device.
assert "resolved_oxide_bindings" not in manifest, manifest
PY
python3 "${SCRIPT_DIR}/stage-local-app-runtime.py" \
  --repo-root "${REPO_ROOT}" \
  --node-modules "${NODE_MODULES}" \
  --output "${STAGED_OUTPUT}" \
  --platform android \
  --variant play

NEXT_DRIFT_NODE_MODULES="${TEMP_ROOT}/node-modules-next-drift"
cp -R "${NODE_MODULES}" "${NEXT_DRIFT_NODE_MODULES}"
mkdir -p "${NEXT_DRIFT_NODE_MODULES}/next" "${NEXT_DRIFT_NODE_MODULES}/@next/swc-linux-arm64-musl"
python3 - "${NEXT_DRIFT_NODE_MODULES}" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
(root / "next/package.json").write_text(
    json.dumps({"name": "next", "version": "0.0.0-forbidden"}),
    encoding="utf-8",
)
(root / "@next/swc-linux-arm64-musl/package.json").write_text(
    json.dumps({"name": "@next/swc-linux-arm64-musl", "version": "0.0.0-forbidden"}),
    encoding="utf-8",
)
(root / "@next/swc-linux-arm64-musl/next-swc.linux-arm64-musl.node").write_bytes(
    bytes.fromhex("7f454c46") + b"fixture"
)
PY
expect_rejection "staged runtime to reject Next/SWC drift" \
  python3 "${SCRIPT_DIR}/stage-local-app-runtime.py" \
  --repo-root "${REPO_ROOT}" \
  --node-modules "${NEXT_DRIFT_NODE_MODULES}" \
  --output "${TEMP_ROOT}/next-drift-output" \
  --platform android \
  --variant play

ADDON_DRIFT_NODE_MODULES="${TEMP_ROOT}/node-modules-addon-drift"
cp -R "${NODE_MODULES}" "${ADDON_DRIFT_NODE_MODULES}"
mkdir -p "${ADDON_DRIFT_NODE_MODULES}/left-pad"
printf '{"name":"left-pad","version":"1.3.0"}\n' \
  > "${ADDON_DRIFT_NODE_MODULES}/left-pad/package.json"
printf '\177ELFfixture' > "${ADDON_DRIFT_NODE_MODULES}/left-pad/left-pad.node"
expect_rejection "staged runtime to reject an arbitrary native addon" \
  python3 "${SCRIPT_DIR}/stage-local-app-runtime.py" \
  --repo-root "${REPO_ROOT}" \
  --node-modules "${ADDON_DRIFT_NODE_MODULES}" \
  --output "${TEMP_ROOT}/addon-drift-output" \
  --platform android \
  --variant play

IOS_NODE_MODULES="${TEMP_ROOT}/node_modules-ios"
cp -R "${NODE_MODULES}" "${IOS_NODE_MODULES}"
rm -rf \
  "${IOS_NODE_MODULES}/@rolldown/binding-linux-x64-musl" \
  "${IOS_NODE_MODULES}/@rollup/rollup-linux-x64-musl" \
  "${IOS_NODE_MODULES}/lightningcss-linux-x64-musl"
python3 "${SCRIPT_DIR}/stage-local-app-runtime.py" \
  --repo-root "${REPO_ROOT}" \
  --node-modules "${IOS_NODE_MODULES}" \
  --output "${IOS_STAGED_OUTPUT}" \
  --platform ios \
  --variant store
python3 - "${IOS_STAGED_OUTPUT}/runtime-manifest.json" <<'PY'
import json
import pathlib
import sys

manifest = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
assert manifest["resolved_rolldown_bindings"] == ["@rolldown/binding-linux-arm64-musl"], manifest
assert manifest["resolved_rollup_bindings"] == ["@rollup/rollup-linux-arm64-musl"], manifest
assert manifest["resolved_lightningcss_bindings"] == ["lightningcss-linux-arm64-musl"], manifest
assert "resolved_oxide_bindings" not in manifest, manifest
PY

# The iOS bundle install step. The staged tree is 0555/0444, so a second build
# cannot `rm -rf` the previous copy without first restoring write permission —
# and `cp -R` into a destination that still exists nests the new runtime inside
# the stale one, shipping build #1's node_modules. That removal-and-copy used to
# live inline in the Xcode build phase where nothing could run it; it is now
# clients/ios/scripts/install-staged-local-app-runtime.sh.
ROOTFS_PINS="${TEMP_ROOT}/rootfs-pins"
mkdir -p "${ROOTFS_PINS}"
printf 'package-augmented rootfs fixture\n' > "${ROOTFS_PINS}/rootfs.tar.gz"
python3 - "${ROOTFS_PINS}" <<'PY'
import hashlib
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
digest = hashlib.sha256((root / "rootfs.tar.gz").read_bytes()).hexdigest()
(root / "source-only-pins.json").write_text(
    json.dumps({"rootfs": {"archives": {"arm64-v8a": {"sha256": "e" * 64}}}}),
    encoding="utf-8",
)
(root / "wrong-pins.json").write_text(
    json.dumps({"rootfs": {"release_archives": {"arm64-v8a": {"sha256": "0" * 64}}}}),
    encoding="utf-8",
)
(root / "pins.json").write_text(
    json.dumps({"rootfs": {"release_archives": {"arm64-v8a": {"sha256": digest}}}}),
    encoding="utf-8",
)
PY
expect_rejection "an uncommitted release rootfs digest to fail validation" \
  python3 "${SCRIPT_DIR}/rootfs_tool.py" verify-release-archive \
  --pins "${ROOTFS_PINS}/source-only-pins.json" --abi arm64-v8a \
  --archive "${ROOTFS_PINS}/rootfs.tar.gz"
expect_rejection "a release rootfs digest mismatch to fail validation" \
  python3 "${SCRIPT_DIR}/rootfs_tool.py" verify-release-archive \
  --pins "${ROOTFS_PINS}/wrong-pins.json" --abi arm64-v8a \
  --archive "${ROOTFS_PINS}/rootfs.tar.gz"
python3 "${SCRIPT_DIR}/rootfs_tool.py" verify-release-archive \
  --pins "${ROOTFS_PINS}/pins.json" --abi arm64-v8a \
  --archive "${ROOTFS_PINS}/rootfs.tar.gz"

# KNOWN-GAP ANCHOR. Everything above drives SYNTHETIC pin fixtures, so it proves
# the comparator works, not that the shipped bytes are anchored to anything. They
# are not: docs/mobile-linux/mobile-linux-pins.json commits no
# `rootfs.release_archives` digest, because the package-augmented rootfs cannot be
# reproducibly built while docs/mobile-linux/local-app-runtime-pins.json reports
# closure_status "blocked" for both ABIs. See the "KNOWN UNANCHORED STEP" section
# of docs/mobile-linux/README.md.
#
# This block asserts the ABSENCE of that anchor, by its exact reason, for every
# ABI staging iterates over. It goes RED the moment a digest is committed — that
# is deliberate: replace it with a positive match assertion and update the README
# section named above in the same change.
COMMITTED_PINS="${REPO_ROOT}/docs/mobile-linux/mobile-linux-pins.json"
# The key must be absent, checked structurally. `verify-release-archive` reports
# a MALFORMED digest with the same "no committed release rootfs digest" message
# as an absent one, so the reason check below cannot tell the two apart on its
# own — and a botched pin must not read as an untouched gap.
COMMITTED_ABIS="$(python3 - "${COMMITTED_PINS}" <<'PY'
import json
import pathlib
import sys

rootfs = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))["rootfs"]
if "release_archives" in rootfs:
    raise SystemExit(
        "known-gap anchor is stale: mobile-linux-pins.json now carries "
        "rootfs.release_archives. Replace the known-gap anchor in "
        "test-local-app-supply-chain.sh with a real match assertion and update "
        "the 'KNOWN UNANCHORED STEP' section of docs/mobile-linux/README.md."
    )
archives = rootfs["archives"]
if not archives:
    raise SystemExit("expected committed pins to list at least one rootfs ABI")
print(" ".join(sorted(archives)))
PY
)"
for abi in ${COMMITTED_ABIS}; do
  gap_reason="$(python3 "${SCRIPT_DIR}/rootfs_tool.py" verify-release-archive \
    --pins "${COMMITTED_PINS}" --abi "${abi}" \
    --archive "${ROOTFS_PINS}/rootfs.tar.gz" 2>&1 || true)"
  case "${gap_reason}" in
    "no committed release rootfs digest for ${abi}: "*) ;;
    *)
      echo "known-gap anchor is stale for ${abi}: ${gap_reason}" >&2
      echo "docs/mobile-linux/mobile-linux-pins.json appears to carry a release" >&2
      echo "digest now. Replace this block with a real match assertion and update" >&2
      echo "the 'KNOWN UNANCHORED STEP' section of docs/mobile-linux/README.md." >&2
      exit 1
      ;;
  esac
done

# The plan-driven create gate must reject semantic regressions, not merely
# match the current file on the happy path.
#
# RETIRED (2026-09-21): the fixtures here used to mutate the OLD create skill's
# tokens ("never re-ask an answered decision", the UI-summary sentence, and an
# `authoring_spec`/`mcp_intent` Workflow launch example) and then drove
# `validate_optimized_create_skill_contract`. `local-app-build.js` is deleted
# and `create-local-app/SKILL.md` was rewritten for the plan-driven flow
# (EnterPlanMode -> ExitPlanMode -> LocalAppPrepare -> LocalAppBuild ->
# LocalAppRuntime), so the fixtures now mutate THAT contract and drive
# `validate_create_flow_contract`. The former `workflow_mutations` half is gone
# with `local-app-build.js`.
python3 - "${REPO_ROOT}" <<'PY'
import contextlib
import importlib.util
import io
import pathlib
import sys

repo = pathlib.Path(sys.argv[1])
module_path = repo / "scripts/mobile-linux/verify-local-app-supply-chain.py"
spec = importlib.util.spec_from_file_location("local_app_supply_verify", module_path)
verify = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verify)
text = (repo / "skills/create-local-app/SKILL.md").read_text(encoding="utf-8")
mutations = {
    "plan approval is the create confirmation": (
        "That approval — the Allow on the plan — IS the create confirmation.",
        "The plan is approved later by a native sheet.",
    ),
    "host prepare step": (
        "LocalAppPrepare({",
        "LocalAppScaffold({",
    ),
    "build is the completion condition": (
        "A successful `LocalAppBuild` is the completion condition.",
        "The build is followed by an automatic verification stage.",
    ),
    "no automatic verification": (
        "automatic verification stage after it",
        "a verification stage after it",
    ),
    "retired build workflow reintroduced": (
        "## 5. Deliver",
        "## 5. Deliver\n\nLaunch the build workflow lingxi-local-app:local-app-build to finish.",
    ),
    "deleted create agent reintroduced": (
        "## 1. Plan",
        "## 1. Plan\n\nThe designer drafts the UI recommendation.",
    ),
}
for label, (needle, replacement) in mutations.items():
    mutated = text.replace(needle, replacement)
    if mutated == text:
        raise SystemExit(f"negative fixture {label!r} did not mutate the skill")
    try:
        with contextlib.redirect_stderr(io.StringIO()):
            verify.validate_create_flow_contract(mutated)
    except SystemExit:
        continue
    raise SystemExit(f"plan-driven create gate accepted regression: {label}")
PY

echo "local-app supply-chain tests passed (release rootfs digest: KNOWN GAP, unanchored)"
