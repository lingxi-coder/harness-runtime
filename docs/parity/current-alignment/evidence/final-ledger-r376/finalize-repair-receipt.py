#!/usr/bin/env python3
"""Validate final r376 repair evidence, then stage receipts without running gates.

Usage (supply the final paths and new output names from the completed receipts):
  /usr/local/bin/python3.11 finalize-repair-receipt.py \
    --h-sha <final-H-SHA> --p-sha <final-P-SHA> \
    --h-gates-dir harness-repair-gates-r6 --h-gates-dir harness-repair-gates-r7 \
    --h-gates-dir harness-repair-gates-r8 --h-gates-dir harness-repair-gates-r9 \
    --h-gates-dir harness-repair-gates-r10 \
    --product-results-dir <product-results> --platform-dir <platform-results> \
    --package-receipt <final-package-receipt> --cli-run04-receipt <run04-receipt> \
    --check-runtime-dependency-log <dependency-check.log> \
    --evidence-dir <new-evidence-dir> --repair-receipt-out <new-repair.json> \
    --final-baseline-out <new-baseline.json> --platform-receipt-out <new-platform.json> \
    --gates-audit-out <new-gates-audit.json>

This helper is intentionally acceptance-only. It reads committed H/P trees and
existing gate/package receipts, archives referenced output logs below a fresh
acceptance evidence directory, and writes four explicitly named JSON outputs.
It never invokes Cargo, Native, packaging, or source-mutating commands. Python
3.11 or newer is required for the standard-library TOML parser.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any

if sys.version_info < (3, 11):
    raise SystemExit("finalize-repair-receipt.py requires Python 3.11 or newer")

import tomllib


HERE = Path(__file__).resolve().parent
BASE = HERE.parents[1]
ACCEPTANCE = BASE / "acceptance"
HARNESS = BASE / "harness-runtime"
PRODUCT = BASE / "lingxi-app"
SDK = BASE / "llm-client"
BASELINE_PATH = HERE / "baseline.json"

REPAIR_SCHEMA = "claude-295-repairs-gate-receipt-r1"
FINAL_SCHEMA = "claude-295-final-baseline-r376-v1"
PLATFORM_SCHEMA = "final-ledger-r376-platform-cargo-check-v1"
NATIVE_SHA = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"
SDK_SHA = "a363c5b3a237d53eea12c3abfb3b98648ef15d3e"
MOBILE_SHA = "6b033d41a2b6b736fda1e8f996f55a4b20bd885d"
H_BASELINE = "4d6d0d6c428e6624a303f5e2a5c96eb4a392f1a1"
P_BASELINE = "f71087b428665c30d0bd752e16f76cb53ab7fef6"

H_URL = "https://github.com/lingxi-coder/harness-runtime.git"
SDK_URL = "https://github.com/lingxi-coder/llm-client.git"
MOBILE_URL = "https://github.com/lingxi-coder/mobile-linux-runtime.git"
H_WORKSPACE_REF_COUNT = 39
H_LOCK_REF_COUNT = 78
SDK_LOCK_REF_COUNT = 5
MOBILE_LOCK_REF_COUNT = 5

H_PHASE_COMMANDS = {
    "fmt": "cargo fmt --all -- --check",
    "permission-lib": "cargo test --locked --offline -p permission --all-features --lib",
    "agent-lib": "cargo test --locked --offline -p agent --all-features --lib",
    "tasks-lib": "cargo test --locked --offline -p tasks --all-features --lib",
    "cancel-inflight": (
        "cargo test --locked --offline -p orchestrator --test current_alignment_cancel "
        "native_inflight_sigint_contract_reaches_orchestrator_output_and_jsonl "
        "-- --exact --nocapture"
    ),
    "orchestrator-lib": "cargo test --locked --offline -p orchestrator --all-features --lib",
    "runtime-lib": "cargo test --locked --offline -p harness-runtime --all-features --lib -- --test-threads=2",
    "changed-clippy": (
        "cargo clippy --locked --offline -p harness-runtime --all-features --all-targets -- -D warnings"
    ),
    "changed-docs": (
        "cargo doc --locked --offline -p core -p permission -p agent -p tasks -p orchestrator "
        "-p harness-runtime --all-features --no-deps"
    ),
    "hooks-lib": "cargo test --locked --offline -p hooks --all-features --lib",
    "hooks-focused": (
        "cargo test --locked --offline -p hooks --all-features --lib "
        "native_295_comma_mode_event_set_matches_directory_added_and_model_switches"
    ),
}
R6_REUSED_PHASES = {"permission-lib", "agent-lib", "tasks-lib", "cancel-inflight", "orchestrator-lib"}
R7_SUPERSEDED_PHASES = {"runtime-lib", "changed-clippy"}
R8_REUSED_PHASES = {"changed-docs", "hooks-lib", "hooks-focused"}
R9_INTERMEDIATE_PHASES = {"fmt", "runtime-lib", "changed-clippy"}
R10_FINAL_PHASES = {"fmt", "runtime-lib", "changed-clippy"}
R8_FINAL_DELTA_PATH = "crates/runtime/src/headless/run.rs"
PRODUCT_FIXTURE_PATHS = {
    "apps/cli/host/tests/fixtures/cli-cancel-native-295/README.md",
    "apps/cli/host/tests/fixtures/cli-cancel-native-295/stdout.raw",
    "apps/cli/host/tests/fixtures/cli-cancel-native-295/prompt-content.raw.json",
    "apps/cli/host/tests/fixtures/cli-cancel-native-295/interrupt-content.raw.json",
}

REQUIRED_H_SOURCES = {
    "crates/core/src/host/mod.rs",
    "crates/core/src/host/permission_gate.rs",
    "crates/agent/src/handle.rs",
    "crates/agent/src/handle/spawn_context.rs",
    "crates/agent/src/handle/tests/tests.rs",
    "crates/agent/src/tool_resolver.rs",
    "crates/tasks/src/handlers/in_process_teammate.rs",
    "crates/tasks/src/handlers/in_process_teammate_test.rs",
    "crates/hooks/src/matcher.rs",
    "crates/hooks/src/registry.rs",
    "crates/permission/src/policy_gate_test.rs",
    "crates/orchestrator/tests/current_alignment_cancel.rs",
    "crates/orchestrator/src/conversation/model.rs",
    "crates/runtime/src/mobile/host/assembly.rs",
    "crates/runtime/src/mobile/host/tests/bash_precommit_wiring_tests.rs",
    "crates/runtime/src/headless/run/control.rs",
    R8_FINAL_DELTA_PATH,
}
REQUIRED_P_SOURCES = {
    "apps/bridge-server/src/driver.rs",
    "apps/bridge-server/src/server.rs",
    "apps/cli/host/src/loop_wakeup.rs",
    "apps/cli/host/src/init.rs",
    "apps/cli/host/src/repl.rs",
    "apps/cli/host/tests/commands_native_cli.rs",
    *PRODUCT_FIXTURE_PATHS,
}
DOC_PREFIXES = ("docs/", "acceptance/", "apps/web/src/data/docs/")
DOC_EXACT_PATHS = {
    "apps/web/public/api-index.json",
    "apps/web/src/data/rustdoc-manifest.json",
}
DOC_SUFFIXES = {".md", ".mdx", ".rst"}

PLATFORM_COMMANDS = {
    "android-arm64-default": [
        "cargo", "ndk", "--target", "arm64-v8a", "--platform", "28",
        "--manifest-path", "apps/android/ffi/Cargo.toml", "check", "-p",
        "android-aar", "--locked", "--offline",
    ],
    "android-x86_64-default": [
        "cargo", "ndk", "--target", "x86_64", "--platform", "28",
        "--manifest-path", "apps/android/ffi/Cargo.toml", "check", "-p",
        "android-aar", "--locked", "--offline",
    ],
    "android-arm64-computer-use": [
        "cargo", "ndk", "--target", "arm64-v8a", "--platform", "28",
        "--manifest-path", "apps/android/ffi/Cargo.toml", "check", "-p",
        "android-aar", "--features", "android-computer-use", "--locked", "--offline",
    ],
    "android-x86_64-computer-use": [
        "cargo", "ndk", "--target", "x86_64", "--platform", "28",
        "--manifest-path", "apps/android/ffi/Cargo.toml", "check", "-p",
        "android-aar", "--features", "android-computer-use", "--locked", "--offline",
    ],
    "ios-simulator-default": [
        "cargo", "check", "--locked", "--offline", "-p", "ios-framework",
        "--target", "aarch64-apple-ios-sim",
    ],
}
NATIVE_PROBE_RECEIPTS = (
    "native-ltr-alias-probe-run-once/receipt.json",
    "native-ltr-alias-probe-approved-endpoint-run/receipt.json",
)


class FinalizeError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise FinalizeError(message)


def sha_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha_file(path: Path) -> str:
    return sha_bytes(path.read_bytes())


def diff_sections(patch: bytes, label: str) -> dict[str, bytes]:
    chunks = re.split(rb"(?=^diff --git a/)", patch, flags=re.MULTILINE)
    sections: dict[str, bytes] = {}
    for chunk in chunks:
        if not chunk:
            continue
        match = re.match(rb"diff --git a/(.*?) b/(.*?)\n", chunk)
        require(match is not None, f"{label} contains bytes outside a standard diff section")
        old_path, new_path = (value.decode("utf-8") for value in match.groups())
        require(old_path == new_path, f"{label} contains a rename: {old_path} -> {new_path}")
        require(old_path not in sections, f"{label} contains duplicate path sections: {old_path}")
        sections[old_path] = chunk
    return sections


def git_blob_sha(data: bytes) -> str:
    result = subprocess.run(
        ["git", "-C", str(HARNESS), "hash-object", "--stdin"],
        check=False,
        input=data,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode:
        raise FinalizeError(f"git hash-object failed: {result.stderr.decode('utf-8', errors='replace')}")
    return result.stdout.decode("ascii").strip()


def reverse_apply_test_patch(current: bytes, patch: bytes, relative_path: str) -> bytes:
    with tempfile.TemporaryDirectory(prefix="r376-test-only-patch-") as temporary:
        root = Path(temporary)
        target = root / relative_path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(current)
        for check in (True, False):
            args = ["git", "apply", "--reverse"]
            if check:
                args.append("--check")
            args.append("-")
            result = subprocess.run(
                args,
                cwd=root,
                check=False,
                input=patch,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            if result.returncode:
                stage = "check" if check else "apply"
                raise FinalizeError(
                    f"test-only delta patch reverse-{stage} failed: "
                    f"{result.stderr.decode('utf-8', errors='replace')}"
                )
        return target.read_bytes()


def validate_test_only_patch_locations(patch: bytes, path: str, marker_line: int) -> None:
    try:
        lines = patch.decode("utf-8").splitlines()
    except UnicodeDecodeError as error:
        raise FinalizeError(f"test-only delta patch is not UTF-8: {error}") from error

    file_headers = [
        match.groups()
        for line in lines
        if (match := re.fullmatch(r"diff --git a/(.*?) b/(.*?)", line)) is not None
    ]
    require(file_headers == [(path, path)],
            "test-only delta patch must contain one exact control.rs diff header")
    old_headers = [line[6:] for line in lines if line.startswith("--- a/")]
    new_headers = [line[6:] for line in lines if line.startswith("+++ b/")]
    require(old_headers == [path] and new_headers == [path],
            "test-only delta patch file paths differ from control.rs")

    hunk = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
    old_line = new_line = 0
    in_hunk = False
    seen_hunk = False
    for line in lines:
        match = hunk.match(line)
        if match:
            old_line = int(match.group(1))
            new_line = int(match.group(3))
            in_hunk = True
            seen_hunk = True
            continue
        if not in_hunk:
            continue
        if line.startswith("\\ No newline"):
            continue
        if line.startswith(" "):
            old_line += 1
            new_line += 1
        elif line.startswith("-"):
            require(old_line > marker_line,
                    f"test-only patch removes a line at/before cfg(test) marker: {old_line}")
            old_line += 1
        elif line.startswith("+"):
            require(new_line > marker_line,
                    f"test-only patch adds a line at/before cfg(test) marker: {new_line}")
            new_line += 1
        else:
            in_hunk = False
    require(seen_hunk, "test-only delta patch has no unified diff hunks")


def regular_file(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve(strict=True)
    require(path.exists() and not path.is_symlink(), f"{label} is missing or symlinked: {path}")
    require(resolved.is_file(), f"{label} must be a regular file: {path}")
    return resolved


def require_nonempty(path: Path, label: str) -> Path:
    resolved = regular_file(path, label)
    require(resolved.stat().st_size > 0, f"{label} must not be empty: {path}")
    return resolved


def regular_dir(path: Path, label: str) -> Path:
    resolved = path.expanduser().resolve(strict=True)
    require(path.exists() and not path.is_symlink(), f"{label} is missing or symlinked: {path}")
    require(resolved.is_dir(), f"{label} must be a directory: {path}")
    return resolved


def read_json(path: Path, label: str) -> dict[str, Any]:
    try:
        value = json.loads(regular_file(path, label).read_text(encoding="utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise FinalizeError(f"{label} is not valid UTF-8 JSON: {path}: {error}") from error
    require(isinstance(value, dict), f"{label} must be a JSON object: {path}")
    return value


def git(repo: Path, *args: str, binary: bool = False) -> str | bytes:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if result.returncode:
        raise FinalizeError(
            f"git command failed in {repo}: {' '.join(args)}\n"
            f"{result.stderr.decode('utf-8', errors='replace')}"
        )
    return result.stdout if binary else result.stdout.decode("utf-8").strip()


def validate_sha(value: str, label: str) -> str:
    require(bool(re.fullmatch(r"[0-9a-f]{40}", value)), f"{label} must be a full lowercase commit SHA")
    return value


def verify_commit(repo: Path, value: str, label: str, baseline: str) -> None:
    validate_sha(value, label)
    require(git(repo, "rev-parse", "HEAD") == value, f"{label} is not current HEAD")
    git(repo, "cat-file", "-e", f"{value}^{{commit}}")
    require(value != baseline, f"{label} still equals retired baseline {baseline}")


def verify_source_tree_at(repo: Path, tested_head: str) -> None:
    current = str(git(repo, "rev-parse", "HEAD"))
    require(current == tested_head, f"source-tree check expected {tested_head}, found {current}")
    later = str(git(repo, "diff", "--name-only", f"{tested_head}..HEAD", "--")).splitlines()
    staged = str(git(repo, "diff", "--cached", "--name-only", "--")).splitlines()
    unstaged = str(git(repo, "diff", "--name-only", "--")).splitlines()
    unexpected = sorted({path for path in later + staged + unstaged if not is_document_path(path)})
    require(not unexpected, f"{repo.name} has untested source changes after {tested_head}: {unexpected}")


def is_document_path(name: str) -> bool:
    if name in PRODUCT_FIXTURE_PATHS:
        return False
    return name.startswith(DOC_PREFIXES) or name in DOC_EXACT_PATHS or Path(name).suffix.lower() in DOC_SUFFIXES


def source_paths(repo: Path, old: str, new: str) -> list[str]:
    changed = str(git(repo, "diff", "--name-only", f"{old}..{new}", "--")).splitlines()
    return sorted(name for name in changed if not is_document_path(name))


def relative_to_here(path: Path) -> str:
    resolved = path.resolve(strict=True)
    require(resolved.is_relative_to(ACCEPTANCE.resolve()), f"receipt reference escapes acceptance root: {path}")
    return os.path.relpath(resolved, HERE.resolve()).replace(os.sep, "/")


def add_archive(
    archive_plan: dict[str, Path],
    evidence_dir: Path,
    destination: str,
    source: Path,
    label: str,
) -> str:
    source_path = regular_file(source, label)
    dest_path = evidence_dir / destination
    require(dest_path.resolve().is_relative_to(HERE.resolve()),
            f"{label} archive destination escapes final-ledger-r376: {dest_path}")
    key = dest_path.relative_to(HERE.resolve()).as_posix()
    require(key not in archive_plan, f"duplicate archive destination for {label}: {key}")
    archive_plan[key] = source_path
    return key


def archive_or_reference(
    archive_plan: dict[str, Path],
    evidence_dir: Path,
    source: Path,
    destination: str,
    label: str,
) -> str:
    source_path = regular_file(source, label)
    if source_path.is_relative_to(ACCEPTANCE.resolve()):
        return relative_to_here(source_path)
    return add_archive(archive_plan, evidence_dir, destination, source_path, label)


def staged_output_path(raw: str, label: str) -> Path:
    path = Path(raw)
    if not path.is_absolute():
        path = HERE / path
    resolved_parent = path.parent.resolve()
    require(resolved_parent.is_relative_to(HERE.resolve()), f"{label} must be under {HERE}")
    require(not path.exists() and not path.is_symlink(), f"{label} already exists: {path}")
    return resolved_parent / path.name


def parse_identities(path: Path, label: str) -> dict[str, str]:
    lines = regular_file(path, label).read_text(encoding="utf-8").splitlines()
    values: dict[str, str] = {}
    for line in lines:
        if "=" in line:
            key, value = line.split("=", 1)
            values[key.strip()] = value.strip()
    return values


def require_identity(actual: dict[str, str], expected: dict[str, str], label: str) -> None:
    for key, value in expected.items():
        require(actual.get(key) == value, f"{label} {key}={actual.get(key)!r}; expected {value!r}")


def validate_product_pins(product_sha: str, harness_sha: str, baseline: dict[str, Any]) -> dict[str, str]:
    manifest_text = git(PRODUCT, "show", f"{product_sha}:Cargo.toml")
    lock_text = git(PRODUCT, "show", f"{product_sha}:Cargo.lock")
    api_index_text = git(PRODUCT, "show", f"{product_sha}:apps/web/public/api-index.json")
    try:
        manifest = tomllib.loads(str(manifest_text))
        lock = tomllib.loads(str(lock_text))
        api_index = json.loads(str(api_index_text))
    except (tomllib.TOMLDecodeError, json.JSONDecodeError) as error:
        raise FinalizeError(f"committed Product dependency inputs are malformed: {error}") from error

    dependencies = manifest["workspace"]["dependencies"]
    harness_dependencies = {
        name: value
        for name, value in dependencies.items()
        if isinstance(value, dict) and value.get("git") == H_URL
    }
    require(len(harness_dependencies) == H_WORKSPACE_REF_COUNT,
            f"expected {H_WORKSPACE_REF_COUNT} canonical Harness workspace dependencies; found {len(harness_dependencies)}")
    stale_harness = {name: value.get("rev") for name, value in harness_dependencies.items() if value.get("rev") != harness_sha}
    require(not stale_harness, f"Product Harness refs do not all pin final H: {stale_harness}")

    sdk_dep = dependencies.get("lingxi-llm-client")
    require(isinstance(sdk_dep, dict) and sdk_dep.get("git") == SDK_URL and sdk_dep.get("rev") == SDK_SHA,
            "Product SDK workspace dependency must retain its canonical URL and frozen SDK SHA")
    mobile_aliases = ("mobile-linux-api", "mobile-linux-ios", "platform-pty")
    for alias in mobile_aliases:
        dep = dependencies.get(alias)
        require(isinstance(dep, dict) and dep.get("git") == MOBILE_URL and dep.get("rev") == MOBILE_SHA,
                f"Product Mobile dependency {alias} changed its canonical URL or frozen revision")

    packages = lock["package"]
    sources = [item.get("source", "") for item in packages]
    h_source = f"git+{H_URL}?rev={harness_sha}#{harness_sha}"
    sdk_source = f"git+{SDK_URL}?rev={SDK_SHA}#{SDK_SHA}"
    mobile_source = f"git+{MOBILE_URL}?rev={MOBILE_SHA}#{MOBILE_SHA}"
    for source, count, label in (
        (h_source, H_LOCK_REF_COUNT, "Harness"),
        (sdk_source, SDK_LOCK_REF_COUNT, "SDK"),
        (mobile_source, MOBILE_LOCK_REF_COUNT, "Mobile"),
    ):
        actual = sum(item_source == source for item_source in sources)
        require(actual == count, f"Product Cargo.lock has {actual} {label} rows at the canonical source; expected {count}")
    for source, prefix, label in (
        (h_source, f"git+{H_URL}?rev=", "Harness"),
        (sdk_source, f"git+{SDK_URL}?rev=", "SDK"),
        (mobile_source, f"git+{MOBILE_URL}?rev=", "Mobile"),
    ):
        divergent = [item for item in sources if item.startswith(prefix) and item != source]
        require(not divergent, f"Product lock contains divergent {label} Git sources: {divergent}")

    index_rows = api_index.get("repositories")
    require(isinstance(index_rows, list), "committed Product API index is missing repositories[]")
    require(all(row.get("dirty") is False for row in index_rows), "Product API index contains dirty source rows")
    index_revisions = {row.get("sdk"): row.get("revision") for row in index_rows}
    require(index_revisions.get("harness") == harness_sha, "Product API index Harness revision is not final H")
    require(index_revisions.get("llm") == SDK_SHA, "Product API index SDK revision is not frozen SDK")
    require(index_revisions.get("mobile") == MOBILE_SHA, "Product API index Mobile revision is not frozen Mobile")
    require(bool(re.fullmatch(r"[0-9a-f]{40}", str(index_revisions.get("bridge", "")))),
            "Product API index Bridge row is not a full source SHA")
    require(baseline["baseline"]["llm-client"]["head"] == SDK_SHA, "frozen SDK differs from baseline")
    return {"harness": harness_sha, "sdk": SDK_SHA, "mobile": MOBILE_SHA}


def validate_harness_source_binding(
    h_sha: str,
    gate_dirs: list[Path],
    proof_path: Path,
    delta_patch_path: Path,
    production_proof_path: Path,
    production_delta_patch_path: Path,
    baseline: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, Path]]:
    by_name = {directory.name: directory for directory in gate_dirs}
    require(set(by_name) == {
        "harness-repair-gates-r6", "harness-repair-gates-r7", "harness-repair-gates-r8",
        "harness-repair-gates-r9", "harness-repair-gates-r10",
    }, f"expected exactly the r6, r7, r8, r9, and r10 Harness gate folders; got {sorted(by_name)}")
    r6_dir = by_name["harness-repair-gates-r6"]
    r7_dir = by_name["harness-repair-gates-r7"]
    r8_dir = by_name["harness-repair-gates-r8"]
    r9_dir = by_name["harness-repair-gates-r9"]
    r10_dir = by_name["harness-repair-gates-r10"]
    baseline_head = baseline["baseline"]["harness-runtime"]["head"]
    require(baseline_head == H_BASELINE, "frozen Harness baseline changed")

    snapshots: dict[str, dict[str, Any]] = {}
    for name, directory in (("r6", r6_dir), ("r7", r7_dir), ("r8", r8_dir), ("r9", r9_dir), ("r10", r10_dir)):
        base_head = regular_file(directory / "base-head.txt", f"{name} Harness base head").read_text(encoding="utf-8").strip()
        require(base_head == baseline_head, f"{name} Harness gate used base {base_head}, expected {baseline_head}")
        candidate_path = directory / "candidate-head.txt"
        if candidate_path.exists() or candidate_path.is_symlink():
            execution_head = validate_sha(
                regular_file(candidate_path, f"{name} Harness execution head").read_text(encoding="utf-8").strip(),
                f"{name} Harness execution head",
            )
        else:
            execution_head = base_head
        patch_path = regular_file(directory / "source.patch", f"{name} tested source patch")
        current_path = regular_file(directory / "source-current.patch", f"{name} current source patch")
        patch_sha = sha_file(patch_path)
        sidecar = regular_file(directory / "source.patch.sha256", f"{name} source patch SHA sidecar").read_text(encoding="utf-8").strip()
        sidecar_tokens = sidecar.split()
        require(sidecar_tokens and sidecar_tokens[0] == patch_sha,
                f"{name} source patch SHA sidecar mismatch")
        snapshots[name] = {
            "directory": directory,
            "base_head": base_head,
            "execution_head": execution_head,
            "patch": patch_path,
            "patch_sha256": patch_sha,
            "current_patch": current_path,
            "current_patch_sha256": sha_file(current_path),
        }

    r9_candidate = regular_file(r9_dir / "candidate-head.txt", "r9 candidate H head").read_text(encoding="utf-8").strip()
    r10_candidate = regular_file(r10_dir / "candidate-head.txt", "r10 candidate H head").read_text(encoding="utf-8").strip()
    validate_sha(r9_candidate, "r9 candidate H head")
    validate_sha(r10_candidate, "r10 candidate H head")
    require(r10_candidate == r9_candidate,
            "r10 candidate HEAD differs from the source baseline used by the preserved r9 intermediate gate")
    require(snapshots["r9"]["execution_head"] == r9_candidate
            and snapshots["r10"]["execution_head"] == r10_candidate,
            "r9/r10 execution HEAD does not match its recorded candidate-head.txt")

    require(snapshots["r6"]["current_patch"].read_bytes() == snapshots["r7"]["patch"].read_bytes(),
            "r6 current source snapshot is not the exact r7 tested source patch")
    require(snapshots["r7"]["current_patch"].read_bytes() == snapshots["r7"]["patch"].read_bytes(),
            "r7 source changed while its runtime and Clippy gates were running")
    require(snapshots["r8"]["patch"].read_bytes() == snapshots["r7"]["patch"].read_bytes(),
            "r8 did not reuse the exact source snapshot passed by r7 runtime and Clippy")
    require(snapshots["r8"]["current_patch"].read_bytes() == snapshots["r8"]["patch"].read_bytes(),
            "r8 source changed while its final H gates were running")
    require(snapshots["r9"]["current_patch"].read_bytes() == snapshots["r9"]["patch"].read_bytes(),
            "r9 intermediate source changed while its runtime, Clippy, or formatting gates were running")
    require(snapshots["r10"]["current_patch"].read_bytes() == snapshots["r10"]["patch"].read_bytes(),
            "r10 source changed while its final runtime, Clippy, or formatting gates were running")

    r8_sections = diff_sections(snapshots["r8"]["patch"].read_bytes(), "r8 source patch")
    r9_sections = diff_sections(snapshots["r9"]["patch"].read_bytes(), "r9 source patch")
    r10_sections = diff_sections(snapshots["r10"]["patch"].read_bytes(), "r10 source patch")
    r8_r9_changed_paths = sorted(
        path for path in set(r8_sections) | set(r9_sections)
        if r8_sections.get(path) != r9_sections.get(path)
    )
    require(r8_r9_changed_paths == [R8_FINAL_DELTA_PATH],
            f"r8 to r9 source delta differs from the one authorized production file: {r8_r9_changed_paths}")
    require(R8_FINAL_DELTA_PATH not in r8_sections and R8_FINAL_DELTA_PATH in r9_sections,
            "r9 source patch must add only the run.rs source section")

    r9_r10_changed_paths = sorted(
        path for path in set(r9_sections) | set(r10_sections)
        if r9_sections.get(path) != r10_sections.get(path)
    )
    require(r9_r10_changed_paths == [R8_FINAL_DELTA_PATH],
            f"r9 to r10 source delta differs from the one authorized production file: {r9_r10_changed_paths}")
    r8_r10_changed_paths = sorted(
        path for path in set(r8_sections) | set(r10_sections)
        if r8_sections.get(path) != r10_sections.get(path)
    )
    require(r8_r10_changed_paths == [R8_FINAL_DELTA_PATH],
            f"r8 to r10 source delta differs from the one authorized production file: {r8_r10_changed_paths}")

    production_proof = read_json(production_proof_path, "r8 to final production source proof")
    require(production_proof.get("schema") == "r376-r8-final-single-production-file-proof-v1"
            and production_proof.get("status") == "verified_source_delta"
            and production_proof.get("gate_status") == "not_assessed_by_source_proof",
            "r8/final source proof is not a source-only delta proof")
    require(production_proof.get("all_other_source_diff_sections_identical") is True
            and production_proof.get("changed_paths") == [R8_FINAL_DELTA_PATH],
            "r8/final proof does not establish the exact one-file production delta")
    require(production_proof.get("intermediate_deltas") == {
        "r8_to_r9_changed_paths": [R8_FINAL_DELTA_PATH],
        "r9_to_final_changed_paths": [R8_FINAL_DELTA_PATH],
    }, "r8/final proof does not retain the one-file intermediate r9 source transition")
    previous_snapshot = production_proof.get("previous_snapshot")
    current_snapshot = production_proof.get("current_snapshot")
    require(isinstance(previous_snapshot, dict) and isinstance(current_snapshot, dict),
            "r8/final proof snapshot bindings are malformed")
    require(previous_snapshot.get("name") == "r8" and previous_snapshot.get("base_head") == baseline_head
            and previous_snapshot.get("candidate_head") == r10_candidate
            and previous_snapshot.get("source_patch") == relative_to_here(snapshots["r8"]["patch"])
            and previous_snapshot.get("source_patch_sha256") == snapshots["r8"]["patch_sha256"]
            and previous_snapshot.get("source_current_patch_sha256") == snapshots["r8"]["current_patch_sha256"],
            "r8/final proof does not bind the accepted r8 tested source snapshot")
    require(current_snapshot.get("name") == "r10" and current_snapshot.get("base_head") == baseline_head
            and current_snapshot.get("candidate_head") == r10_candidate
            and current_snapshot.get("source_patch") == relative_to_here(snapshots["r10"]["patch"])
            and current_snapshot.get("source_patch_sha256") == snapshots["r10"]["patch_sha256"]
            and current_snapshot.get("source_current_patch_sha256") == snapshots["r10"]["current_patch_sha256"],
            "r8/final proof does not bind the r10 source snapshot")
    intermediate_snapshots = production_proof.get("intermediate_snapshots")
    require(isinstance(intermediate_snapshots, list) and len(intermediate_snapshots) == 1
            and intermediate_snapshots[0].get("name") == "r9"
            and intermediate_snapshots[0].get("base_head") == baseline_head
            and intermediate_snapshots[0].get("candidate_head") == r10_candidate
            and intermediate_snapshots[0].get("source_patch") == relative_to_here(snapshots["r9"]["patch"])
            and intermediate_snapshots[0].get("source_patch_sha256") == snapshots["r9"]["patch_sha256"]
            and intermediate_snapshots[0].get("source_current_patch_sha256") == snapshots["r9"]["current_patch_sha256"],
            "r8/final proof does not preserve the intermediate r9 source snapshot")
    unchanged_section_sha = {
        path: sha_bytes(section) for path, section in sorted(r8_sections.items()) if path != R8_FINAL_DELTA_PATH
    }
    require(production_proof.get("unchanged_source_section_sha256") == unchanged_section_sha,
            "r8/final proof per-file section hashes do not match the gate snapshots")

    production_delta = regular_file(production_delta_patch_path, "r8/final production delta patch")
    production_delta_bytes = production_delta.read_bytes()
    delta_record = production_proof.get("delta")
    require(isinstance(delta_record, dict) and delta_record.get("path") == R8_FINAL_DELTA_PATH
            and delta_record.get("patch") == relative_to_here(production_delta)
            and delta_record.get("patch_sha256") == sha_bytes(production_delta_bytes),
            "r8/final proof does not bind the exact run.rs production delta patch")
    previous_blob = str(git(HARNESS, "rev-parse", f"{r10_candidate}:{R8_FINAL_DELTA_PATH}"))
    baseline_blob = str(git(HARNESS, "rev-parse", f"{baseline_head}:{R8_FINAL_DELTA_PATH}"))
    current_blob = str(git(HARNESS, "rev-parse", f"{h_sha}:{R8_FINAL_DELTA_PATH}"))
    previous_bytes = git(HARNESS, "show", f"{r10_candidate}:{R8_FINAL_DELTA_PATH}", binary=True)
    current_bytes = git(HARNESS, "show", f"{h_sha}:{R8_FINAL_DELTA_PATH}", binary=True)
    require(previous_blob == baseline_blob == delta_record.get("previous_git_blob")
            and current_blob == delta_record.get("current_git_blob"),
            "r8/final proof old/current run.rs Git blobs differ from committed H")
    require(sha_bytes(previous_bytes) == delta_record.get("previous_content_sha256")
            and sha_bytes(current_bytes) == delta_record.get("current_content_sha256"),
            "r8/final proof old/current run.rs content hashes differ from committed H")
    require(delta_record.get("baseline_git_blob") == baseline_blob,
            "r8/final proof does not bind the original run.rs baseline blob")
    r9_delta_section = re.sub(
        rb"(?m)^index [0-9a-f]+\.\.([0-9a-f]+)(.*)$",
        lambda match: b"index " + previous_blob.encode("ascii") + b".."
        + current_blob.encode("ascii") + match.group(2),
        r10_sections[R8_FINAL_DELTA_PATH],
    )
    require(r9_delta_section == production_delta_bytes,
            "r10 run.rs source section differs from the explicit candidate-to-final delta patch")
    committed_r8_patch = git(HARNESS, "diff", "--binary", f"{baseline_head}..{r10_candidate}",
                             "--", "crates", "Cargo.toml", "Cargo.lock", binary=True)
    require(committed_r8_patch == snapshots["r8"]["patch"].read_bytes(),
            "r8 tested source snapshot does not byte-match the candidate commit shared by r9/r10")
    candidate_to_final_paths = source_paths(HARNESS, r10_candidate, h_sha)
    require(candidate_to_final_paths == [R8_FINAL_DELTA_PATH],
            f"final H differs from the r10 candidate outside run.rs: {candidate_to_final_paths}")
    git(HARNESS, "merge-base", "--is-ancestor", r10_candidate, h_sha)
    final_delta_patch = git(HARNESS, "diff", "--binary", "--full-index", f"{r10_candidate}..{h_sha}",
                            "--", R8_FINAL_DELTA_PATH, binary=True)
    require(final_delta_patch == production_delta_bytes,
            "committed r8/final run.rs delta does not byte-match the source proof patch")

    r9_status = regular_file(r9_dir / "status.txt", "r9 intermediate Harness gate status")
    require(r9_status.read_text(encoding="utf-8").strip() == "passed",
            "r9 intermediate Harness gate status is not passed")
    r9_intermediate_phases: list[dict[str, Any]] = []
    for phase_name in sorted(R9_INTERMEDIATE_PHASES):
        exit_path = regular_file(r9_dir / f"{phase_name}.exit", f"r9 intermediate {phase_name} exit receipt")
        require(exit_path.read_text(encoding="utf-8").strip() == "0",
                f"r9 intermediate {phase_name} did not pass")
        log_path = regular_file(r9_dir / f"{phase_name}.log", f"r9 intermediate {phase_name} log")
        r9_intermediate_phases.append({
            "name": phase_name,
            "status": "passed_intermediate",
            "command": H_PHASE_COMMANDS[phase_name],
            "exit_code": 0,
            "source_patch_sha256": snapshots["r9"]["patch_sha256"],
            "log": relative_to_here(log_path),
            "log_sha256": sha_file(log_path),
            "exit_receipt": relative_to_here(exit_path),
            "exit_receipt_sha256": sha_file(exit_path),
        })

    final_status = regular_file(r10_dir / "status.txt", "r10 final Harness gate status")
    require(final_status.read_text(encoding="utf-8").strip() == "passed",
            "r10 final Harness gate status is not passed")
    final_patch = git(HARNESS, "diff", "--binary", f"{baseline_head}..{h_sha}", "--", "crates", "Cargo.toml", "Cargo.lock", binary=True)
    require(final_patch == snapshots["r10"]["patch"].read_bytes(),
            "final committed H source patch does not byte-match the r10 tested source snapshot")

    proof = read_json(proof_path, "r6 to r7 test-only source proof")
    require(proof.get("schema") == "r376-test-only-followup-proof-v1" and proof.get("status") == "verified",
            "r6/r7 test-only source proof is not verified")
    control_path = "crates/runtime/src/headless/run/control.rs"
    require(proof.get("path") == control_path, "test-only follow-up proof names an unexpected path")
    require(proof.get("previous_source_patch_sha256") == snapshots["r6"]["patch_sha256"],
            "test-only proof does not bind the r6 tested patch")
    require(proof.get("current_source_patch_sha256") == snapshots["r7"]["patch_sha256"],
            "test-only proof does not bind the r7 tested patch")
    require(proof.get("all_other_source_diff_sections_identical") is True,
            "test-only proof does not establish identical non-control source sections")
    require(set(proof.get("reused_phases", [])) == R6_REUSED_PHASES,
            "test-only proof reused phases differ from the five reviewed r6 gates")
    require(set(proof.get("changes", [])) == {
        "cfg(test) helper missing import",
        "cfg(test) unused parameter name",
        "cfg(test) argument-count lint annotation",
    }, "test-only proof lists unexpected control-module changes")

    delta_patch = regular_file(delta_patch_path, "r6/r7 test-only delta patch")
    delta_bytes = delta_patch.read_bytes()
    require(proof.get("test_only_delta_patch") == relative_to_here(delta_patch),
            "test-only proof points at a different delta patch")
    require(proof.get("test_only_delta_patch_sha256") == sha_bytes(delta_bytes),
            "test-only delta patch SHA differs from the proof")

    current_control_blob = git(HARNESS, "rev-parse", f"{h_sha}:{control_path}")
    require(current_control_blob == proof.get("current_control_git_blob"), "test-only proof current control blob differs from final H")
    current_control = git(HARNESS, "show", f"{h_sha}:{control_path}", binary=True)
    marker = re.search(rb"(?m)^#\[cfg\(test\)\]\r?\nmod tests\b", current_control)
    require(marker is not None, "final control.rs has no inline cfg(test) module boundary")
    prefix_sha = sha_bytes(current_control[:marker.start()])
    require(prefix_sha == proof.get("production_prefix_sha256"),
            "final control.rs production prefix differs from the test-only proof")
    test_marker_line = current_control[:marker.start()].count(b"\n") + 1
    validate_test_only_patch_locations(delta_bytes, control_path, test_marker_line)
    previous_control = reverse_apply_test_patch(current_control, delta_bytes, control_path)
    require(git_blob_sha(previous_control) == proof.get("previous_control_git_blob"),
            "reverse-applying the test-only patch does not reproduce the r6 control.rs blob")

    h_delta = source_paths(HARNESS, baseline_head, h_sha)
    missing = sorted(REQUIRED_H_SOURCES - set(h_delta))
    require(not missing, f"final H source delta is missing required paths: {missing}")
    allowed = {
        "crates/core/src/host/mod.rs", "crates/core/src/host/permission_gate.rs",
        "crates/agent/src/handle.rs", "crates/agent/src/handle/spawn_context.rs",
        "crates/agent/src/handle/tests/tests.rs", "crates/agent/src/tool_resolver.rs",
        "crates/tasks/src/handlers/in_process_teammate.rs", "crates/tasks/src/handlers/in_process_teammate_test.rs",
        "crates/hooks/src/matcher.rs", "crates/hooks/src/registry.rs",
        "crates/orchestrator/src/conversation.rs", "crates/orchestrator/src/conversation/model.rs",
        "crates/orchestrator/src/conversation/drivers/mod.rs",
        "crates/orchestrator/src/conversation/drivers/projected_content.rs",
        "crates/orchestrator/src/conversation/drivers/streaming.rs",
        "crates/orchestrator/src/conversation/model_reminders.rs",
        "crates/orchestrator/src/conversation/reminders.rs",
        "crates/orchestrator/src/conversation/tests/turn_admission_cancel_tests.rs",
        "crates/orchestrator/src/conversation/tooling.rs", "crates/orchestrator/src/conversation/transcript.rs",
        "crates/orchestrator/src/conversation/wiring.rs", "crates/orchestrator/src/streaming_executor_test.rs",
        "crates/orchestrator/src/turn_loop_test.rs", "crates/orchestrator/tests/current_alignment_cancel.rs",
        "crates/orchestrator/tests/queued_prompt_batch_test.rs", "crates/permission/src/lib.rs",
        "crates/permission/src/policy.rs", "crates/permission/src/policy_gate.rs",
        "crates/permission/src/policy_gate_test.rs", "crates/permission/src/policy_test.rs",
        "crates/runtime/src/desktop/assembly.rs", "crates/runtime/src/desktop/configuration.rs",
        "crates/runtime/src/desktop/mod.rs", "crates/runtime/src/desktop/tests/tests.rs",
        "crates/runtime/src/mobile/host/assembly.rs", "crates/runtime/src/mobile/host/tests/bash_precommit_wiring_tests.rs",
        control_path, R8_FINAL_DELTA_PATH,
    }
    unexpected = sorted(set(h_delta) - allowed)
    require(not unexpected, f"final H source delta exceeds the exact allowlist: {unexpected}")
    binding = {
        "baseline_head": baseline_head,
        "final_head": h_sha,
        "r6_tested_source_patch_sha256": snapshots["r6"]["patch_sha256"],
        "r7_tested_source_patch_sha256": snapshots["r7"]["patch_sha256"],
        "r8_tested_source_patch_sha256": snapshots["r8"]["patch_sha256"],
        "r9_intermediate_source_patch_sha256": snapshots["r9"]["patch_sha256"],
        "r10_final_source_patch_sha256": snapshots["r10"]["patch_sha256"],
        "final_commit_source_patch_sha256": sha_bytes(final_patch),
        "final_commit_patch_matches_r8": False,
        "final_commit_patch_matches_r9": False,
        "final_commit_patch_matches_r10": True,
        "final_gate_status": relative_to_here(final_status),
        "final_gate_status_sha256": sha_file(final_status),
        "r6_current_patch_matches_r7_tested_patch": True,
        "r7_current_patch_matches_r7_tested_patch": True,
        "r8_tested_patch_matches_r7_tested_patch": True,
        "r8_current_patch_matches_r8_tested_patch": True,
        "test_only_followup_proof": relative_to_here(proof_path),
        "test_only_followup_proof_sha256": sha_file(proof_path),
        "test_only_delta_patch": relative_to_here(delta_patch),
        "test_only_delta_patch_sha256": sha_bytes(delta_bytes),
        "test_only_path": control_path,
        "test_only_changes": sorted(proof["changes"]),
        "current_control_git_blob": current_control_blob,
        "production_prefix_sha256": prefix_sha,
        "reused_r6_phases": sorted(R6_REUSED_PHASES),
        "superseded_r7_source_phases": sorted(R7_SUPERSEDED_PHASES),
        "reused_r8_source_phases": sorted(R8_REUSED_PHASES),
        "final_r10_phases": sorted(R10_FINAL_PHASES),
        "r9_intermediate_phases": r9_intermediate_phases,
        "r9_intermediate_gate_status": relative_to_here(r9_status),
        "r9_intermediate_gate_status_sha256": sha_file(r9_status),
        "final_r8_to_r10_source_proof": relative_to_here(production_proof_path),
        "final_r8_to_r10_source_proof_sha256": sha_file(production_proof_path),
        "final_r8_to_r10_delta_patch": relative_to_here(production_delta),
        "final_r8_to_r10_delta_patch_sha256": sha_bytes(production_delta_bytes),
        "r10_candidate_head": r10_candidate,
        "r10_delta_path": R8_FINAL_DELTA_PATH,
        "r10_previous_run_git_blob": previous_blob,
        "r10_final_run_git_blob": current_blob,
        "r10_final_patch_matches_r10": True,
        "r10_gate_status": relative_to_here(final_status),
        "r10_gate_status_sha256": sha_file(final_status),
        "source_patch_snapshots": {
            key: {
                "execution_head": value["execution_head"],
                "tested_source_patch": relative_to_here(value["patch"]),
                "sha256": value["patch_sha256"],
                "current_patch": relative_to_here(value["current_patch"]),
                "current_patch_sha256": value["current_patch_sha256"],
            }
            for key, value in snapshots.items()
        },
    }
    return binding, {"r6": r6_dir, "r7": r7_dir, "r8": r8_dir, "r9": r9_dir, "r10": r10_dir}


def validate_harness_phases(
    gate_dirs: dict[str, Path], h_sha: str, source_binding: dict[str, Any]
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    source_by_phase = {
        "permission-lib": "r6",
        "agent-lib": "r6",
        "tasks-lib": "r6",
        "cancel-inflight": "r6",
        "orchestrator-lib": "r6",
        "runtime-lib": "r10",
        "changed-clippy": "r10",
        "fmt": "r10",
        "changed-docs": "r8",
        "hooks-lib": "r8",
        "hooks-focused": "r8",
    }
    require(set(source_by_phase) == set(H_PHASE_COMMANDS), "internal H phase provenance map is incomplete")
    phases: list[dict[str, Any]] = []
    for name, origin in source_by_phase.items():
        directory = gate_dirs[origin]
        exit_path = regular_file(directory / f"{name}.exit", f"Harness {origin} {name} exit receipt")
        raw_exit = exit_path.read_text(encoding="utf-8").strip()
        require(raw_exit == "0", f"selected Harness {origin} phase {name} exit code is {raw_exit!r}, expected 0")
        stdout_log = regular_file(directory / f"{name}.log", f"Harness {origin} {name} log")
        stdout_sha = sha_file(stdout_log)
        # A successful `cargo fmt --check` is silent. Its nonempty .exit receipt
        # is the validator-facing log; retain the actual empty stdout path/hash
        # separately so silence is not confused with a missing log.
        primary_log = exit_path if stdout_log.stat().st_size == 0 else stdout_log
        phases.append({
            "name": name,
            "repository": "harness-runtime",
            "head": h_sha,
            "execution_head": source_binding["source_patch_snapshots"][origin]["execution_head"],
            "execution_gate_snapshot": origin,
            "command": H_PHASE_COMMANDS[name],
            "exit_code": 0,
            "log": relative_to_here(primary_log),
            "log_sha256": sha_file(primary_log),
            "stdout_log": relative_to_here(stdout_log),
            "stdout_log_sha256": stdout_sha,
            "exit_receipt": relative_to_here(exit_path),
            "exit_receipt_sha256": sha_file(exit_path),
            "tested_source_patch_sha256": source_binding["source_patch_snapshots"][origin]["sha256"],
            "followup_source_closure_proof_sha256": source_binding["test_only_followup_proof_sha256"],
            "production_followup_source_closure_proof_sha256": source_binding["final_r8_to_r10_source_proof_sha256"],
        })

    superseded_test_only: list[dict[str, Any]] = []
    superseded_environment: list[dict[str, Any]] = []
    for origin, directory in gate_dirs.items():
        for phase_name, command in H_PHASE_COMMANDS.items():
            exit_path = directory / f"{phase_name}.exit"
            if not exit_path.is_file() or phase_name in source_by_phase and source_by_phase[phase_name] == origin:
                continue
            code_text = exit_path.read_text(encoding="utf-8").strip()
            if code_text == "0":
                continue

            log = regular_file(directory / f"{phase_name}.log", f"superseded {origin} {phase_name} log")
            record = {
                "phase": phase_name,
                "snapshot": origin,
                "exit_code": int(code_text) if code_text.isdecimal() else code_text,
                "command": command,
                "log": relative_to_here(log),
                "log_sha256": sha_file(log),
                "exit_receipt": relative_to_here(regular_file(exit_path, f"superseded {origin} {phase_name} exit receipt")),
                "exit_receipt_sha256": sha_file(exit_path),
            }
            if origin == "r6" and phase_name == "runtime-lib":
                require(code_text == "101", "r6 runtime failure no longer matches the recorded test-only compile failure")
                log_text = log.read_text(encoding="utf-8", errors="replace")
                require("cannot find type `PrintAuxTaskGroup` in this scope" in log_text,
                        "r6 runtime failure is not the missing cfg(test) import covered by the follow-up patch")
                record["reason"] = (
                    "Superseded first by the r7 runtime-lib pass after the proof-bound cfg(test)-only fix, "
                    "then by the final r10 runtime-lib pass bound to the one-file run.rs production delta."
                )
                superseded_test_only.append(record)
                continue

            if origin == "r7" and phase_name == "hooks-lib":
                require(code_text == "101", "r7 hooks-lib environment failure has an unexpected exit code")
                log_text = log.read_text(encoding="utf-8", errors="replace")
                require("could not start Bun source compiler bun: No such file or directory" in log_text
                        and re.search(r"test result: FAILED\. \d+ passed; 5 failed;", log_text),
                        "r7 hooks-lib failure does not match the five Bun PATH lookup failures")
                record["superseded_environment"] = True
                record["environment_failure"] = "Bun executable was absent from PATH in the r7 runner environment."
                record["resolution"] = "r8 reran hooks-lib with the fixed Bun executable directory on PATH."
                superseded_environment.append(record)
                continue

            raise FinalizeError(
                f"unexpected superseded Harness failure: {origin}/{phase_name} exit {code_text}"
            )
    require({(item["snapshot"], item["phase"]) for item in superseded_test_only}
            == {("r6", "runtime-lib")},
            "expected exactly the known r6 cfg(test)-only runtime compile failure")
    require({(item["snapshot"], item["phase"]) for item in superseded_environment}
            == {("r7", "hooks-lib")},
            "expected exactly the known r7 hooks-lib Bun PATH environment failure")
    return phases, superseded_test_only, superseded_environment


def parse_progress(path: Path, expected_names: set[str], label: str) -> dict[str, int]:
    starts: list[str] = []
    exits: dict[str, int] = {}
    for line in regular_file(path, f"{label} progress log").read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r"START ([A-Za-z0-9_-]+) \S+", line)
        if match:
            starts.append(match.group(1))
            continue
        match = re.fullmatch(r"EXIT ([A-Za-z0-9_-]+) ([0-9]+)", line)
        if match:
            name = match.group(1)
            require(name not in exits, f"duplicate {label} phase exit: {name}")
            exits[name] = int(match.group(2))
    require(set(starts) == expected_names and len(starts) == len(expected_names),
            f"{label} phase starts are incomplete or unexpected: {starts}")
    require(set(exits) == expected_names, f"{label} phase exits are incomplete: {exits}")
    failures = {name: code for name, code in exits.items() if code != 0}
    require(not failures, f"{label} phases did not all exit zero: {failures}")
    return exits


def parse_recorded_commands(path: Path) -> dict[str, str]:
    commands: dict[str, str] = {}
    for line in regular_file(path, "Product commands log").read_text(encoding="utf-8").splitlines():
        if not line.strip():
            continue
        head, _, rest = line.partition(" ")
        # The current runner records metadata as an unlabelled cargo command.
        if head == "cargo" and rest.startswith("metadata "):
            head, rest = "metadata", line
        require(bool(rest.strip()), f"malformed Product command record: {line}")
        require(head not in commands, f"duplicate Product command record: {head}")
        commands[head] = rest.strip()
    return commands


def validate_product_result_folder(
    product_dir: Path,
    product_sha: str,
    harness_sha: str,
    baseline: dict[str, Any],
    runtime_log: Path,
    runtime_log_exit: int,
    evidence_dir: Path,
    archive_plan: dict[str, Path],
) -> tuple[list[dict[str, Any]], dict[str, Any], dict[str, Any]]:
    expected_pins = {
        "product_sha": product_sha,
        "harness_sha": harness_sha,
        "sdk_sha": SDK_SHA,
        "mobile_sha": MOBILE_SHA,
    }
    identities = parse_identities(product_dir / "identities.txt", "Product result identities")
    require_identity(identities, expected_pins, "Product result")
    summary = regular_file(product_dir / "summary.txt", "Product gate summary").read_text(encoding="utf-8")
    require("status=passed" in summary, "Product result summary is not passed")

    gate_logs = regular_dir(product_dir / "gate-logs", "Product gate logs directory")
    expected_run_names = {
        "metadata", "source-validator", "fmt", "check", "clippy", "skill-cold", "full-runner", "cli-8mib"
    }
    exits = parse_progress(gate_logs / "progress.log", expected_run_names, "Product")
    commands = parse_recorded_commands(gate_logs / "commands.txt")
    require(set(commands) == expected_run_names, f"Product recorded command phases differ: {sorted(commands)}")
    command_requirements = {
        "metadata": ("cargo metadata", "--all-features", "--locked", "--offline"),
        "source-validator": ("runtime_source.py", "--json"),
        "fmt": ("cargo fmt", "--all", "--check"),
        "check": ("cargo check", "--workspace", "--all-features", "--all-targets"),
        "clippy": ("cargo clippy", "--workspace", "--all-targets", "-D warnings"),
        "skill-cold": ("cli_cold_resume_preserves_skill_source_and_normalizes_anthropic_wire", "--exact"),
        "full-runner": ("run_product_full_gate.py", "--expected-product-head", product_sha),
        "cli-8mib": ("actual_cli_matches_native_2_1_295_slash_skill_and_stream_json_fixed_fields", "--exact"),
    }
    for name, fragments in command_requirements.items():
        command = commands[name]
        require(all(fragment in command for fragment in fragments), f"Product {name} command differs: {command}")
    require("--all-features" not in commands["clippy"], "Product Clippy must match default-feature workspace CI")

    source_runtime_text = runtime_log.read_text(encoding="utf-8")
    require(runtime_log_exit == 0, f"check-runtime-dependency exit code is {runtime_log_exit}, expected 0")
    require(source_runtime_text.rstrip().endswith("exit_code=0"),
            "check-runtime-dependency log must end with the runner's explicit exit_code=0 marker")
    require(len(re.findall(rf"^runtime-dependency: \d+ packages at {re.escape(harness_sha)}$", source_runtime_text, re.M)) == 1,
            "check-runtime-dependency log does not report exactly one final H runtime revision")
    require(len(re.findall(rf"^mobile-linux-dependency: \d+ packages at {MOBILE_SHA}$", source_runtime_text, re.M)) == 1,
            "check-runtime-dependency log does not report exactly one frozen Mobile revision")

    product_phases: list[dict[str, Any]] = []

    def add_product_phase(name: str, command: str, source: Path, archive_name: str) -> None:
        regular_file(source, f"Product {name} log")
        silent_fmt = name == "product-fmt" and source.stat().st_size == 0
        primary = gate_logs / "progress.log" if silent_fmt else source
        require_nonempty(primary, f"Product {name} acceptance log")
        log_ref = add_archive(archive_plan, evidence_dir,
                              "product/gates/fmt-progress.log" if silent_fmt else archive_name,
                              primary, f"Product {name} acceptance log")
        product_phases.append({
            "name": name,
            "repository": "lingxi-app",
            "head": product_sha,
            "command": command,
            "exit_code": 0,
            "log": log_ref,
            "log_sha256": sha_file(primary),
        })
        if silent_fmt:
            product_phases[-1]["stdout_log"] = add_archive(
                archive_plan, evidence_dir, archive_name, source, "silent Product fmt stdout")
            product_phases[-1]["stdout_log_sha256"] = sha_file(source)

    runtime_log_command = "python3 scripts/checks/check-runtime-dependency.py"
    add_product_phase("product-source", runtime_log_command, runtime_log, "product/check-runtime-dependency.log")

    product_commands = {
        "metadata": ("product-metadata", commands["metadata"], product_dir / "metadata.json"),
        "source-validator": ("product-runtime-source", commands["source-validator"], gate_logs / "source-validator.log"),
        "fmt": ("product-fmt", commands["fmt"], gate_logs / "fmt.log"),
        "check": ("product-check", commands["check"], gate_logs / "check.log"),
        "clippy": ("product-clippy", commands["clippy"], gate_logs / "clippy.log"),
        "skill-cold": ("skill-cold", commands["skill-cold"], gate_logs / "skill-cold.log"),
        "full-runner": ("product-full-runner", commands["full-runner"], product_dir / "runner" / "run.json"),
        "cli-8mib": ("product-cli-8mib", commands["cli-8mib"], gate_logs / "cli-8mib.log"),
    }
    for gate_name, (phase_name, command, log) in product_commands.items():
        add_product_phase(phase_name, command, log, f"product/gates/{gate_name}.log" if gate_name != "full-runner" else "product/runner/run.json")

    runner_path = regular_file(product_dir / "runner" / "run.json", "Product full runner receipt")
    runner = read_json(runner_path, "Product full runner receipt")
    require(runner.get("status") == "complete", "Product full runner status is not complete")
    require(runner.get("product_head") == product_sha and runner.get("git", {}).get("head") == product_sha,
            "Product full runner HEAD does not match final P")
    require(runner.get("pins") == {"harness": harness_sha, "sdk": SDK_SHA, "mobile": MOBILE_SHA},
            "Product full runner canonical pins do not match final H/SDK/Mobile")
    require(runner.get("git", {}).get("source_status_sha256") == hashlib.sha256(b"").hexdigest(),
            "Product full runner source tree was not clean")
    require(runner.get("actual_test_summary", {}).get("failed") == 0,
            "Product full runner reports failed tests")

    runner_phases = runner.get("phases")
    require(isinstance(runner_phases, dict) and runner_phases, "Product full runner has no phase receipt map")
    runner_phase_artifacts: list[dict[str, Any]] = []
    for phase_name, phase_summary in sorted(runner_phases.items()):
        require(phase_summary.get("status") == "passed", f"Product runner phase {phase_name} is not passed")
        require(phase_summary.get("test_summary", {}).get("failed", 0) == 0,
                f"Product runner phase {phase_name} reports failed tests")
        phase_dir = product_dir / "runner" / "phases" / phase_name
        phase_json_path = regular_file(phase_dir / "phase.json", f"Product runner phase {phase_name} receipt")
        phase_json = read_json(phase_json_path, f"Product runner phase {phase_name} receipt")
        require(phase_json.get("status") == "passed", f"Product runner phase file {phase_name} is not passed")
        phase_json_sha = sha_file(phase_json_path)
        phase_json_ref = add_archive(archive_plan, evidence_dir, f"product/runner/phases/{phase_name}/phase.json", phase_json_path,
                                     f"Product runner phase {phase_name} JSON")
        artifacts = {"phase_json": {"path": phase_json_ref, "sha256": phase_json_sha}}
        for key in ("stdout", "stderr"):
            path_key = f"{key}_path"
            sha_key = f"{key}_sha256"
            stream_path = regular_file(Path(phase_json[path_key]), f"Product runner {phase_name} {key}")
            require(sha_file(stream_path) == phase_json[sha_key],
                    f"Product runner {phase_name} {key} SHA mismatch")
            stream_ref = add_archive(
                archive_plan,
                evidence_dir,
                f"product/runner/phases/{phase_name}/{key}.log",
                stream_path,
                f"Product runner {phase_name} {key}",
            )
            artifacts[key] = {"path": stream_ref, "sha256": phase_json[sha_key]}
        runner_phase_artifacts.append({
            "name": phase_name,
            "status": "passed",
            "test_summary": phase_summary.get("test_summary", {}),
            "receipt": artifacts["phase_json"],
            "logs": {key: artifacts[key] for key in ("stdout", "stderr")},
        })

    product_runner_ref = product_phases[[item["name"] for item in product_phases].index("product-full-runner")]["log"]
    runner_ref = {"path": product_runner_ref, "sha256": sha_file(runner_path)}
    return product_phases, {"path": runner_ref["path"], "sha256": runner_ref["sha256"], "phases": runner_phase_artifacts}, {
        "commands": commands,
        "progress": gate_logs / "progress.log",
        "command_log": gate_logs / "commands.txt",
        "exits": exits,
    }


def validate_runtime_dependency_log(path: Path) -> int:
    text = regular_file(path, "check-runtime-dependency log").read_text(encoding="utf-8")
    require_nonempty(path, "check-runtime-dependency log")
    marker = re.search(r"(?:^|\n)exit_code=([0-9]+)\s*$", text)
    require(marker is not None, "check-runtime-dependency log needs a terminal exit_code=N marker")
    return int(marker.group(1))


def validate_native_probe_evidence() -> dict[str, Any]:
    attempts: list[dict[str, Any]] = []
    receipts: list[dict[str, Any]] = []
    for relative_path in NATIVE_PROBE_RECEIPTS:
        path = HERE / relative_path
        receipt = read_json(path, f"Native alias probe receipt {relative_path}")
        require(receipt.get("schema") == "native-2.1.295-ltr-alias-probe-v1",
                f"Native alias probe schema mismatch: {relative_path}")
        require(receipt.get("native_sha256_expected") == NATIVE_SHA,
                f"Native alias probe expected SHA differs from pinned 2.1.295: {relative_path}")
        native_binding = receipt.get("native_binding")
        require(isinstance(native_binding, dict) and native_binding.get("sha256") == NATIVE_SHA,
                f"Native alias probe binding differs from pinned 2.1.295: {relative_path}")
        count = receipt.get("native_process_count")
        require(type(count) is int and count >= 0,
                f"Native alias probe process count is invalid: {relative_path}")
        require(receipt.get("native_process_started") is (count > 0),
                f"Native alias probe started flag differs from its count: {relative_path}")
        attempts.append({
            "receipt": relative_path,
            "status": receipt.get("status"),
            "native_process_count": count,
        })
        receipts.append(receipt)

    first, approved = receipts
    require(first.get("status") == "capture_invalid" and first.get("native_process_count") == 0
            and first.get("provider_request_count") == 0,
            "first Native probe receipt no longer records the blocked zero-process attempt")
    require(approved.get("status") == "capture_invalid"
            and approved.get("native_process_count") == 1
            and approved.get("provider_request_count") == 1,
            "approved-endpoint Native probe receipt no longer records one invalid capture")
    approved_checks = approved.get("checks")
    require(isinstance(approved_checks, dict) and approved_checks.get("pinned_native_sha256") is True
            and approved_checks.get("positive_edit_deny_observed") is False,
            "approved-endpoint probe unexpectedly claims a positive Edit-deny observation")
    observations = approved.get("alias_observations")
    require(isinstance(observations, list) and observations
            and all(isinstance(item, dict)
                    and item.get("canonical_tool_present_in_provider_catalog") is True for item in observations),
            "approved-endpoint probe no longer records all aliases in the provider catalog")
    interpretation = approved.get("interpretation")
    require(isinstance(interpretation, str) and "do not infer alias/glob semantics" in interpretation,
            "approved-endpoint probe does not preserve its invalid-capture limitation")
    plan = regular_file(HERE / "native-ltr-alias-probe-plan.md", "Native alias probe plan")
    return {
        "new_native_launches": sum(item["native_process_count"] for item in attempts),
        "native_launch_accounting": {
            "scope": (
                "The two one-shot Native 2.1.295 alias/glob probe attempts in this cleanup batch; "
                "historical fixture captures are not counted."
            ),
            "attempts": attempts,
            "interpretation": (
                "Counts started Native processes from each runner receipt, including invalid captures; "
                "invalid attempts do not establish alias/glob semantics."
            ),
        },
        "native_alias_probe_semantics": {
            "status": approved["status"],
            "evidence": [relative_to_here(plan), NATIVE_PROBE_RECEIPTS[-1]],
            "limitation": interpretation,
        },
    }


def package_pins(package: dict[str, Any]) -> dict[str, str] | None:
    aliases = {
        "llm-client": ("llm-client", "llm_client", "sdk"),
        "harness-runtime": ("harness-runtime", "harness_runtime", "harness"),
        "lingxi-app": ("lingxi-app", "lingxi_app", "product"),
    }
    for key in ("main_commits", "tested_implementation", "pins", "candidate"):
        view = package.get(key)
        if isinstance(view, dict) and all(any(alias in view for alias in options) for options in aliases.values()):
            return {repo: next(view[alias] for alias in options if alias in view) for repo, options in aliases.items()}
    if all(key in package for key in ("sdk_sha", "harness_sha", "product_sha")):
        return {
            "llm-client": package["sdk_sha"],
            "harness-runtime": package["harness_sha"],
            "lingxi-app": package["product_sha"],
        }
    return None


def validate_hook_gate(path: Path, h_sha: str, baseline: dict[str, Any]) -> tuple[dict[str, Any], str]:
    receipt = read_json(path, "current Hook gate receipt")
    require(receipt.get("schema") == "r376-hook-comma-gate-v1", "Hook gate receipt schema mismatch")
    require(receipt.get("status") == "passed" and receipt.get("failed") == 0 and receipt.get("focused_new_regression", 0) >= 1,
            "Hook gate receipt is incomplete or failed")
    require(receipt.get("base_harness") == baseline["baseline"]["harness-runtime"]["head"],
            "Hook focused run was not started from the frozen pre-repair H base")
    expected_hook_sources = {
        "crates/hooks/src/matcher.rs",
        "crates/hooks/src/registry.rs",
        "crates/core/src/host/mod.rs",
        "crates/core/src/host/permission_gate.rs",
    }
    source_sha = receipt.get("source_sha256")
    require(isinstance(source_sha, dict) and set(source_sha) == expected_hook_sources,
            "Hook receipt source fingerprints do not match the four current Hook/Core files")
    for source, expected in source_sha.items():
        current = git(HARNESS, "show", f"{h_sha}:{source}", binary=True)
        require(sha_bytes(current) == expected, f"Hook source fingerprint differs from final H: {source}")
    for raw_path, expected in receipt.get("logs", {}).items():
        log = (HERE / raw_path).resolve()
        require(log.is_relative_to(ACCEPTANCE.resolve()) and log.is_file(), f"Hook log missing or outside acceptance: {raw_path}")
        require(sha_file(log) == expected and log.stat().st_size > 0, f"Hook log SHA/size mismatch: {raw_path}")
    return receipt, sha_file(path)


def validate_platform(
    platform_dir: Path,
    product_sha: str,
    harness_sha: str,
    evidence_dir: Path,
    archive_plan: dict[str, Path],
) -> dict[str, Any]:
    identities = parse_identities(platform_dir / "identities.txt", "platform identities")
    require_identity(identities, {
        "product_sha": product_sha,
        "harness_sha": harness_sha,
        "sdk_sha": SDK_SHA,
        "mobile_sha": MOBILE_SHA,
    }, "platform receipt")
    checks: list[dict[str, Any]] = []
    for name, argv in PLATFORM_COMMANDS.items():
        log = regular_file(platform_dir / f"{name}.log", f"platform {name} log")
        text = log.read_text(encoding="utf-8", errors="strict")
        require(text.startswith(f"\n=== {name} ===") or text.startswith(f"=== {name} ==="),
                f"platform log header does not identify {name}")
        matches = re.findall(r"^exit_code=([0-9]+)$", text, re.M)
        require(matches == ["0"], f"platform {name} log exit receipt is not exactly one zero: {matches}")
        log_ref = add_archive(archive_plan, evidence_dir, f"platform/{name}.log", log, f"platform {name} log")
        checks.append({
            "name": name,
            "argv": argv,
            "exit_code": 0,
            "log": str((HERE / log_ref).relative_to(BASE)).replace(os.sep, "/"),
            "log_bytes": log.stat().st_size,
            "log_sha256": sha_file(log),
        })
    return {
        "schema": PLATFORM_SCHEMA,
        "status": "passed",
        "candidate": {
            "product": product_sha,
            "harness_runtime": harness_sha,
            "llm_client": SDK_SHA,
            "mobile_linux": MOBILE_SHA,
        },
        "checks": checks,
        "source_guards": {"before_and_after_each_check": "passed", "identity": identities},
        "limits": ["Cross-target compilation does not establish generated-language integration or physical-device behavior."],
    }


def validate_cli_run04(path: Path, product_sha: str, harness_sha: str, product_run_sha: str) -> dict[str, Any]:
    receipt = read_json(path, "CLI SIGINT run-04 receipt")
    require(receipt.get("schema") == "product-cli-inflight-sigint-v1" and receipt.get("status") == "passed",
            "CLI run-04 receipt is missing or not passed")
    require(receipt.get("failures") == [], "CLI run-04 receipt contains failures")
    checks = receipt.get("checks")
    require(isinstance(checks, dict) and checks and all(value is True for value in checks.values()),
            "CLI run-04 does not have all checks true")
    require(receipt.get("candidate_process_started") is True and receipt.get("native_process_started") is False,
            "CLI run-04 process identity flags are invalid")
    require(receipt.get("candidate_exit_code") == receipt.get("native_fixture_result", {}).get("exit_code"),
            "CLI run-04 candidate exit code does not match the fixed Native fixture constraint")
    binding = receipt.get("binary_binding", {})
    require(binding.get("current_product_head") == product_sha and binding.get("current_harness_head") == harness_sha,
            "CLI run-04 binary binding is not tied to final P/H HEADs")
    require(binding.get("receipt_product_sha") == product_sha and binding.get("receipt_full_runner_head") == product_sha,
            "CLI run-04 Product receipt binding is not final P")
    require(binding.get("receipt_harness_sha") == harness_sha and binding.get("receipt_sdk_sha") == SDK_SHA,
            "CLI run-04 dependency receipt binding is not final H/frozen SDK")
    require(binding.get("receipt_source_status_sha256") == hashlib.sha256(b"").hexdigest(),
            "CLI run-04 Product gate receipt was not source-clean")
    require(binding.get("gate_receipt_sha256") == product_run_sha,
            "CLI run-04 binary was not bound to the supplied Product full-runner receipt")
    require(receipt.get("native_fixture_binding", {}).get("native_sha256") == NATIVE_SHA,
            "CLI run-04 Native fixture is not the pinned 2.1.295 capture")
    return receipt


def write_json_new(path: Path, data: dict[str, Any], label: str) -> str:
    require(not path.exists() and not path.is_symlink(), f"{label} output already exists: {path}")
    payload = (json.dumps(data, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    return payload.decode("utf-8")


def atomic_write_new(path: Path, content: str, label: str) -> None:
    require(not path.exists() and not path.is_symlink(), f"{label} output already exists: {path}")
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent, delete=False) as stream:
        temp = Path(stream.name)
        stream.write(content)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temp, path)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--h-sha", required=True, help="final committed full Harness SHA")
    parser.add_argument("--p-sha", required=True, help="final committed full Product SHA")
    parser.add_argument(
        "--h-gates-dir", required=True, action="append", type=Path,
        help="Harness gate folder; pass r6, r7, r8, r9, and r10 folders once each",
    )
    parser.add_argument("--h-source-followup-proof", type=Path,
                        default=HERE / "r6-test-only-followup-proof.json",
                        help="proof that the r6 to r7 H source delta is only the control.rs test module")
    parser.add_argument("--h-test-only-delta-patch", type=Path,
                        default=HERE / "r6-test-only-followup.patch",
                        help="exact control.rs cfg(test)-only follow-up patch bound by the proof")
    parser.add_argument("--h-production-followup-proof", type=Path,
                        default=HERE / "r8-r10-source-followup-proof.json",
                        help="source-only proof of the exact r8-to-r10 run.rs production delta")
    parser.add_argument("--h-production-delta-patch", type=Path,
                        default=HERE / "r8-r10-production-delta.patch",
                        help="exact candidate-to-final run.rs patch bound by the source proof")
    parser.add_argument("--product-results-dir", required=True, type=Path, help="Product run-product-gates result folder")
    parser.add_argument("--platform-dir", required=True, type=Path, help="five-check final platform log directory")
    parser.add_argument("--package-receipt", required=True, type=Path, help="final signed macOS package receipt")
    parser.add_argument("--cli-run04-receipt", required=True, type=Path, help="final-bound public CLI SIGINT run-04 receipt")
    parser.add_argument("--check-runtime-dependency-log", required=True, type=Path,
                        help="separately executed check-runtime-dependency log, ending in exit_code=0")
    parser.add_argument("--hooks-receipt", type=Path, default=HERE / "hooks-comma-receipt.json",
                        help="refreshed current Hook/Core source-bound receipt")
    parser.add_argument("--evidence-dir", required=True, help="new evidence directory under final-ledger-r376")
    parser.add_argument("--repair-receipt-out", required=True, help="new repair receipt path under final-ledger-r376")
    parser.add_argument("--final-baseline-out", required=True, help="new final-baseline path under final-ledger-r376")
    parser.add_argument("--platform-receipt-out", required=True, help="new final platform receipt path under final-ledger-r376")
    parser.add_argument("--gates-audit-template", type=Path, default=HERE / "gates-audit.json")
    parser.add_argument("--gates-audit-out", required=True, help="new updated gates-audit JSON path under final-ledger-r376")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    h_sha = validate_sha(args.h_sha, "Harness SHA")
    p_sha = validate_sha(args.p_sha, "Product SHA")
    verify_commit(HARNESS, h_sha, "Harness SHA", H_BASELINE)
    verify_commit(PRODUCT, p_sha, "Product SHA", P_BASELINE)
    require(git(SDK, "rev-parse", "HEAD") == SDK_SHA, "SDK HEAD moved from the frozen baseline SHA")
    verify_source_tree_at(HARNESS, h_sha)
    verify_source_tree_at(PRODUCT, p_sha)
    require(not git(SDK, "status", "--porcelain", "--untracked-files=no"),
            "frozen SDK has tracked changes")

    baseline = read_json(BASELINE_PATH, "frozen original baseline")
    require(baseline["target_version"] == "2.1.295" and baseline["native_sha256"] == NATIVE_SHA,
            "frozen baseline target identity changed")
    require(baseline["baseline"]["harness-runtime"]["head"] == H_BASELINE, "baseline H pin changed")
    require(baseline["baseline"]["lingxi-app"]["head"] == P_BASELINE, "baseline P pin changed")
    validate_product_pins(p_sha, h_sha, baseline)
    native_evidence = validate_native_probe_evidence()

    h_gate_dirs = [regular_dir(path, f"Harness gate folder {path.name}") for path in args.h_gates_dir]
    product_dir = regular_dir(args.product_results_dir, "Product result folder")
    platform_dir = regular_dir(args.platform_dir, "final platform folder")
    package_path = regular_file(args.package_receipt, "final package receipt")
    cli_path = regular_file(args.cli_run04_receipt, "CLI run-04 receipt")
    runtime_log = regular_file(args.check_runtime_dependency_log, "check-runtime-dependency log")
    hooks_path = regular_file(args.hooks_receipt, "Hook gate receipt")

    evidence_dir = Path(args.evidence_dir)
    if not evidence_dir.is_absolute():
        evidence_dir = HERE / evidence_dir
    evidence_dir = evidence_dir.parent.resolve() / evidence_dir.name
    require(evidence_dir.is_relative_to(HERE.resolve()), "evidence directory must be under final-ledger-r376")
    require(not evidence_dir.exists() and not evidence_dir.is_symlink(), f"evidence directory already exists: {evidence_dir}")

    output_paths = {
        "repair": staged_output_path(args.repair_receipt_out, "repair receipt"),
        "final": staged_output_path(args.final_baseline_out, "final baseline"),
        "platform": staged_output_path(args.platform_receipt_out, "platform receipt"),
        "gates": staged_output_path(args.gates_audit_out, "gates audit update"),
    }
    require(len(set(output_paths.values())) == len(output_paths), "output JSON paths must be distinct")
    gates_template_path = regular_file(args.gates_audit_template, "gates audit template")
    require(gates_template_path.resolve() not in output_paths.values(), "gates audit output must not overwrite its input template")

    source_binding, h_gate_by_name = validate_harness_source_binding(
        h_sha,
        h_gate_dirs,
        args.h_source_followup_proof,
        args.h_test_only_delta_patch,
        args.h_production_followup_proof,
        args.h_production_delta_patch,
        baseline,
    )
    h_delta = source_paths(HARNESS, H_BASELINE, h_sha)
    p_delta = source_paths(PRODUCT, P_BASELINE, p_sha)
    require(set(p_delta) >= REQUIRED_P_SOURCES, f"final P source delta lacks required paths: {sorted(REQUIRED_P_SOURCES - set(p_delta))}")
    allowed_p = {
        "Cargo.toml", "Cargo.lock", "apps/bridge-server/src/driver.rs", "apps/bridge-server/src/server.rs",
        "apps/cli/host/src/loop_wakeup.rs", "apps/cli/host/src/init.rs", "apps/cli/host/src/repl.rs",
        "apps/cli/host/tests/commands_native_cli.rs",
    } | PRODUCT_FIXTURE_PATHS
    require(not (set(p_delta) - allowed_p), f"final P source delta exceeds exact allowlist: {sorted(set(p_delta) - allowed_p)}")
    require(any(path.startswith("crates/permission/") for path in h_delta), "final H source delta lacks Permission sources")
    require(any(path.startswith("crates/orchestrator/src/") for path in h_delta), "final H source delta lacks Orchestrator sources")
    require(any(path.startswith("crates/runtime/src/desktop/") for path in h_delta), "final H source delta lacks Desktop assembly sources")

    harness_phases, superseded_harness_phases, superseded_environment = validate_harness_phases(
        h_gate_by_name, h_sha, source_binding
    )
    archive_plan: dict[str, Path] = {}
    runtime_exit = validate_runtime_dependency_log(runtime_log)
    product_phases, product_runner, product_inputs = validate_product_result_folder(
        product_dir,
        p_sha,
        h_sha,
        baseline,
        runtime_log,
        runtime_exit,
        evidence_dir,
        archive_plan,
    )

    platform_data = validate_platform(platform_dir, p_sha, h_sha, evidence_dir, archive_plan)
    package_data = read_json(package_path, "final package receipt")
    require(package_data.get("status") in ("passed", "delivered"), "final package receipt is not passed/delivered")
    tested = {"llm-client": SDK_SHA, "harness-runtime": h_sha, "lingxi-app": p_sha}
    require(package_pins(package_data) == tested, "final package receipt pins do not match final SDK/H/P")
    _hook_data, hook_sha = validate_hook_gate(hooks_path, h_sha, baseline)
    cli_data = validate_cli_run04(cli_path, p_sha, h_sha, product_runner["sha256"])

    # These output references are computed only after all source, gate, platform,
    # package, Hook and CLI checks pass. External logs are copied to a fresh
    # acceptance directory so the validator can resolve every phase log.
    package_ref = archive_or_reference(archive_plan, evidence_dir, package_path, "receipts/final-package.json", "final package receipt")
    cli_ref = archive_or_reference(archive_plan, evidence_dir, cli_path, "cli-run04/receipt.json", "CLI run-04 receipt")
    hook_ref = relative_to_here(hooks_path)
    platform_ref = os.path.relpath(output_paths["platform"], HERE).replace(os.sep, "/")
    repair_ref = os.path.relpath(output_paths["repair"], HERE).replace(os.sep, "/")
    platform_text = write_json_new(output_paths["platform"], platform_data, "platform receipt")
    platform_sha = sha_bytes(platform_text.encode("utf-8"))
    repair = {
        "schema": REPAIR_SCHEMA,
        "status": "passed",
        "repos": {"sdk": SDK_SHA, "harness": h_sha, "product": p_sha},
        "source_delta": {"harness": h_delta, "product": p_delta},
        "harness_source_binding": source_binding,
        "phases": harness_phases + product_phases,
        "superseded_harness_phases": superseded_harness_phases,
        "superseded_environment": superseded_environment,
        "product_runner": product_runner,
        "runtime_dependency_gate": {
            "command": "python3 scripts/checks/check-runtime-dependency.py",
            "exit_code": runtime_exit,
            "log": next(phase["log"] for phase in product_phases if phase["name"] == "product-source"),
            "log_sha256": sha_file(runtime_log),
            "harness_revision": h_sha,
            "mobile_revision": MOBILE_SHA,
        },
        "hook_gate": {"receipt": hook_ref, "sha256": hook_sha},
        "package": {"receipt": package_ref, "sha256": sha_file(package_path)},
        "platform": {"receipt": platform_ref, "sha256": platform_sha},
        "cli_sigint_run04": {"receipt": cli_ref, "sha256": sha_file(cli_path), "status": cli_data["status"]},
        "product_full_gate": {"receipt": product_runner["path"], "sha256": product_runner["sha256"]},
        "harness_gate_folders": {
            name: {
                "source_patch": relative_to_here(directory / "source.patch"),
                "source_patch_sha256": sha_file(directory / "source.patch"),
                **({"status": relative_to_here(directory / "status.txt"),
                   "status_sha256": sha_file(directory / "status.txt")}
                   if (directory / "status.txt").is_file() else {}),
            }
        for name, directory in h_gate_by_name.items()
        },
        "product_gate_inputs": {
            "progress_log": add_archive(archive_plan, evidence_dir, "product/gates/progress.log", product_inputs["progress"], "Product progress log"),
            "progress_log_sha256": sha_file(product_inputs["progress"]),
            "commands_log": add_archive(archive_plan, evidence_dir, "product/gates/commands.txt", product_inputs["command_log"], "Product command log"),
            "commands_log_sha256": sha_file(product_inputs["command_log"]),
        },
        "platform_candidate": platform_data["candidate"],
    }
    repair_text = write_json_new(output_paths["repair"], repair, "repair receipt")
    repair_sha = sha_bytes(repair_text.encode("utf-8"))

    final_baseline = {
        "schema": FINAL_SCHEMA,
        "initial_baseline_sha256": sha_file(BASELINE_PATH),
        "target_version": baseline["target_version"],
        "native_sha256": baseline["native_sha256"],
        "tested_implementation": tested,
        "repair_gate_receipt": {"path": repair_ref, "sha256": repair_sha},
        **native_evidence,
    }
    final_text = write_json_new(output_paths["final"], final_baseline, "final baseline")

    gates_audit = read_json(gates_template_path, "gates audit template")
    repair_status = gates_audit["repair_candidate"]
    repair_status.update({"status": "passed", "sdk": SDK_SHA, "harness": h_sha, "product": p_sha,
                          "required_evidence": repair_ref, "receipt_sha256": repair_sha})
    platform_status = gates_audit["platform"]["repair_pin_recheck"]
    platform_status.update({
        "status": "passed",
        "required": True,
        "candidate": {
            "harness_runtime": h_sha,
            "llm_client": SDK_SHA,
            "product": p_sha,
        },
        "receipt": platform_ref,
        "receipt_sha256": platform_sha,
    })
    gates_audit["decision"]["current_repair_candidate_status"] = (
        "passed source-bound H/P gates, package, run-04 CLI acceptance, and final-pin five-target matrix; see "
        f"{output_paths['final'].name} and {output_paths['repair'].name}"
    )
    gates_audit["repair_candidate"]["source_patch_binding"] = source_binding
    gates_audit["repair_candidate"]["cli_sigint_run04"] = {"receipt": cli_ref, "sha256": sha_file(cli_path)}
    gates_audit["repair_candidate"]["package_receipt"] = {"receipt": package_ref, "sha256": sha_file(package_path)}
    gates_text = write_json_new(output_paths["gates"], gates_audit, "gates audit update")

    # All evidence has passed. Only now create the archive and publish the JSON
    # files. Refuse every pre-existing destination; this is a fresh receipt set.
    for dest_rel, source in archive_plan.items():
        destination = HERE / dest_rel
        require(not destination.exists() and not destination.is_symlink(), f"evidence archive destination exists: {destination}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, destination)
        require(sha_file(destination) == sha_file(source), f"archived evidence SHA mismatch: {destination}")
    output_contents = (
        (output_paths["platform"], platform_text, "platform receipt"),
        (output_paths["repair"], repair_text, "repair receipt"),
        (output_paths["final"], final_text, "final baseline"),
        (output_paths["gates"], gates_text, "gates audit update"),
    )
    for path, content, label in output_contents:
        atomic_write_new(path, content, label)

    print(json.dumps({
        "status": "passed",
        "repair_receipt": {"path": str(output_paths["repair"]), "sha256": repair_sha},
        "final_baseline": str(output_paths["final"]),
        "platform_receipt": {"path": str(output_paths["platform"]), "sha256": platform_sha},
        "gates_audit_update": str(output_paths["gates"]),
        "harness_source_patch_sha256": source_binding["final_commit_source_patch_sha256"],
        "phases": len(repair["phases"]),
        "product_runner_phases": len(product_runner["phases"]),
    }, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except FinalizeError as error:
        print(f"finalize-repair-receipt: {error}", file=sys.stderr)
        raise SystemExit(2)
