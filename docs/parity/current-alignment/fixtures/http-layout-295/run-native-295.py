#!/usr/bin/env python3
"""Replay the primary headless print-text runner with the pinned Native 2.1.295 binary."""

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
from datetime import datetime, timezone


ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[4]
CASES = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
SOURCE_COMMIT = CASES["primary_runner_commit"]
NATIVE_SHA = CASES["native_sha256"]
RUNNER_FILES = (
    "scripts/tests/headless_native_differential.mjs",
    "scripts/tests/headless-byte-compare.mjs",
    "scripts/tests/headless-fixtures/matrix.mjs",
    "scripts/tests/headless-fixtures/loopback-provider.mjs",
    "scripts/tests/headless-fixtures/capture.mjs",
    "scripts/tests/headless-fixtures/normalization-adapter.mjs",
    "docs/parity/headless-native-baseline.json",
)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=os.environ.get("CLAUDE_NATIVE_BINARY"))
    parser.add_argument("--output", help="new output directory under this fixture's ignored run-output/")
    args = parser.parse_args()
    if not args.binary:
        raise SystemExit("Pass --binary or set CLAUDE_NATIVE_BINARY to the pinned 2.1.295 executable")

    binary = Path(args.binary).expanduser().resolve()
    binary_bytes = binary.read_bytes()
    if sha256(binary_bytes) != NATIVE_SHA:
        raise SystemExit(f"Native SHA mismatch: {sha256(binary_bytes)}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or version_text != CASES["target"] + " (Claude Code)":
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    output_root = ROOT / "run-output"
    output_root.mkdir(parents=True, exist_ok=True)
    if args.output:
        output = Path(args.output).expanduser().resolve()
    else:
        suffix = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        output = output_root / f"native-2.1.295-print-text-{suffix}"
    try:
        output.relative_to(output_root.resolve())
    except ValueError as error:
        raise SystemExit("Output must be contained in this fixture's run-output directory") from error
    if output.exists():
        raise SystemExit(f"Refusing to overwrite existing output: {output}")

    with tempfile.TemporaryDirectory(prefix="headless-native-295-primary-runner-") as temporary:
        temporary_root = Path(temporary)
        archive = subprocess.run(
            ["git", "-C", str(REPO), "archive", "--format=tar", SOURCE_COMMIT, *RUNNER_FILES],
            capture_output=True,
            check=False,
        )
        if archive.returncode != 0:
            raise SystemExit("Could not read the pinned primary headless runner revision: " + archive.stderr.decode("utf-8", errors="replace"))
        with tarfile.open(fileobj=io.BytesIO(archive.stdout), mode="r:") as bundle:
            bundle.extractall(temporary_root)

        baseline_path = temporary_root / "docs/parity/headless-native-baseline.json"
        baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
        # The only local baseline override is identity metadata for this pinned
        # executable; fixture definitions and capture/comparison code stay at
        # the exact primary-runner source commit.
        baseline["version"] = "2.1.295"
        baseline["pinnedAt"] = "2026-10-08"
        baseline["release"] = {"url": "https://github.com/anthropics/claude-code/releases/tag/v2.1.295", "tag": "v2.1.295"}
        baseline["nativeLatest"] = {"value": "2.1.295"}
        baseline["manifest"] = {"verifiedBy": "locally pinned binary version and SHA; no manifest download in this fixture"}
        baseline["binary"] = {
            "sha256": NATIVE_SHA,
            "size": len(binary_bytes),
            "verifiedVersionOutput": version_text + "\n",
            "localPathHint": str(binary),
        }
        baseline_path.write_text(json.dumps(baseline, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

        node = shutil.which("node")
        if not node:
            raise SystemExit("Node.js is required to run the primary headless Native capture runner")
        runner_path = temporary_root / "scripts/tests/headless_native_differential.mjs"
        command = [
            node, str(runner_path), "--execute", str(binary), "--only", "print-text", "--output", str(output),
        ]
        runner_env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8", "TZ": "UTC"}
        completed = subprocess.run(command, cwd=temporary_root, env=runner_env, capture_output=True, timeout=60, check=False)
        (output / "primary-runner.stdout.raw").write_bytes(completed.stdout)
        (output / "primary-runner.stderr.raw").write_bytes(completed.stderr)

        source_hashes = {}
        for path in RUNNER_FILES[:-1]:
            data = (temporary_root / path).read_bytes()
            source_hashes[path] = {"bytes": len(data), "sha256": sha256(data)}
        # Record original baseline source separately from the 295 identity-only
        # override used in the temporary run root.
        original_baseline = subprocess.run(
            ["git", "-C", str(REPO), "show", f"{SOURCE_COMMIT}:docs/parity/headless-native-baseline.json"],
            capture_output=True,
            check=True,
        ).stdout
        receipt = {
            "native_version": version_text,
            "native_sha256": NATIVE_SHA,
            "native_bytes": len(binary_bytes),
            "primary_runner_commit": SOURCE_COMMIT,
            "primary_runner_source_files": source_hashes,
            "baseline_file_overridden_in_temporary_archive": "docs/parity/headless-native-baseline.json",
            "original_293_baseline_source_sha256": sha256(original_baseline),
            "temporary_baseline_override": {
                "scope": "version/release/binary identity fields only; no fixture, capture or comparison code changed",
                "target_version": "2.1.295",
                "target_sha256": NATIVE_SHA,
            },
            "node_version": subprocess.run([node, "--version"], capture_output=True, timeout=10, check=True).stdout.decode().strip(),
            "invocation": command,
            "fixture_id": "print-text",
            "native_repetitions": 2,
            "native_only": True,
            "dummy_key_only": True,
            "provider_boundary": "127.0.0.1 loopback stub from the pinned primary runner; no upstream relay",
            "runner_exit_code": completed.returncode,
            "runner_stdout": {"bytes": len(completed.stdout), "sha256": sha256(completed.stdout)},
            "runner_stderr": {"bytes": len(completed.stderr), "sha256": sha256(completed.stderr)},
            "report": "report.json",
        }
        (output / "native-295-runner-receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        print(json.dumps({"output": str(output), "receipt": str(output / "native-295-runner-receipt.json"), "runner_exit_code": completed.returncode}, ensure_ascii=False, indent=2))
        return completed.returncode


if __name__ == "__main__":
    raise SystemExit(main())
