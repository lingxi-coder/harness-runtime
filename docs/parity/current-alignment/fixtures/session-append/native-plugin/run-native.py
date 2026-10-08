#!/usr/bin/env python3
"""Run the pinned Native public plugin-test session.append fixture."""

import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent
BASELINE = ROOT / "native-baseline"
BINARY_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
MARKER = b"NATIVE_APPEND_SYSTEM_RESULT="

if not BINARY_VALUE:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
BINARY = Path(BINARY_VALUE).expanduser().resolve()


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


binary_sha = sha256(BINARY.read_bytes())
if binary_sha != EXPECTED_SHA256:
    raise SystemExit(f"Native SHA mismatch: {binary_sha}")

version = subprocess.run(
    [str(BINARY), "--version"], capture_output=True, timeout=15, check=False
)
if version.returncode != 0:
    raise SystemExit("Could not verify Native version")
version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
if not version_text.startswith("2.1.293 "):
    raise SystemExit(f"Native version mismatch: {version_text!r}")

with tempfile.TemporaryDirectory(prefix="native-session-append-utf16-") as temp_dir:
    temp = Path(temp_dir)
    env = {
        "PATH": "/usr/bin:/bin",
        "TMPDIR": str(temp / "tmp"),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "HOME": str(temp / "home"),
        "CLAUDE_CONFIG_DIR": str(temp / "config"),
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    for key in ("TMPDIR", "HOME", "CLAUDE_CONFIG_DIR"):
        Path(env[key]).mkdir()
    argv = [str(BINARY), "plugin", "test", str(ROOT)]
    completed = subprocess.run(
        argv, cwd=temp, env=env, capture_output=True, timeout=60, check=False
    )

stdout, stderr = completed.stdout, completed.stderr
BASELINE.mkdir(parents=True, exist_ok=True)
(BASELINE / "native.stdout.raw").write_bytes(stdout)
(BASELINE / "native.stderr.raw").write_bytes(stderr)
system_lines = [
    line[len(MARKER) :]
    for stream in (stdout, stderr)
    for line in stream.splitlines()
    if line.startswith(MARKER)
]
case_dir = BASELINE / "system-content-lone-surrogate"
case_dir.mkdir(parents=True, exist_ok=True)
if len(system_lines) == 1:
    (case_dir / "result-event.raw.json").write_bytes(system_lines[0])

expected_input = b"system-before-\\ud800-after"
expected_result = b"system-prefix:system-before-\\ud800-after"
raw_match = len(system_lines) == 1 and expected_input in system_lines[0] and expected_result in system_lines[0]
source_files = {
    str(path.relative_to(ROOT)): sha256(path.read_bytes())
    for path in (
        ROOT / "append.test.ts",
        ROOT / "hooks" / "hooks.json",
        ROOT / "hooks" / "register.js",
        ROOT / "plugin.json",
    )
}
manifest = ROOT / "SOURCE-MANIFEST.json"
manifest.write_text(json.dumps(source_files, indent=2) + "\n", encoding="utf-8")
receipt = {
    "native_version": version_text,
    "native_sha256": binary_sha,
    "argv": ["$CLAUDE_NATIVE_BINARY", "plugin", "test", "<fixture-directory>"],
    "isolated_home_and_config": True,
    "fixture_network_scope": "No model request in the test; $.session.append is exercised through public plugin-test API.",
    "exit_code": completed.returncode,
    "stdout_bytes": len(stdout),
    "stdout_sha256": sha256(stdout),
    "stdout_path": "native.stdout.raw",
    "stderr_bytes": len(stderr),
    "stderr_sha256": sha256(stderr),
    "stderr_path": "native.stderr.raw",
    "source_files_sha256": source_files,
    "system_case": {
        "case": "system-lone",
        "raw_result_event_count": len(system_lines),
        "raw_result_event_path": "system-content-lone-surrogate/result-event.raw.json"
        if len(system_lines) == 1
        else None,
        "raw_result_event_sha256": sha256(system_lines[0]) if len(system_lines) == 1 else None,
        "raw_input_and_rewrite_escape_observed": raw_match,
        "assertion": "Public plugin-test expects system message rewrite to retain the lone UTF-16 unit.",
    },
    "lifecycle": {
        "plugin_test_process_exits_after_tests": True,
        "explicit_module_unload_trigger_observed": False,
        "explicit_unload_behavior": "unverified; process exit is only the test boundary",
    },
    "test_result": "passed" if completed.returncode == 0 and raw_match else "failed_or_unobserved",
}
(BASELINE / "native-receipt.json").write_text(
    json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
)
print(json.dumps(receipt, ensure_ascii=False, indent=2))
raise SystemExit(0 if completed.returncode == 0 and raw_match else 1)
