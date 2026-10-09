#!/usr/bin/env python3
"""Capture the frozen Native 2.1.295 plugin-test UI contract."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
NATIVE_PATH = os.environ.get("CLAUDE_NATIVE_BINARY")
if not NATIVE_PATH:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native 2.1.295 executable")
BINARY = Path(NATIVE_PATH).expanduser().resolve()
EXPECTED_SHA256 = "0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


binary_sha = sha256(BINARY.read_bytes())
if binary_sha != EXPECTED_SHA256:
    raise SystemExit(f"Native 2.1.295 SHA mismatch: {binary_sha}")

OUTPUT.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix="native-mod-ui-render-295-") as temp_dir:
    temp = Path(temp_dir)
    home = temp / "home"
    config = temp / "config"
    tmp = temp / "tmp"
    for directory in (home, config, tmp):
        directory.mkdir()
    env = {
        "PATH": "/usr/bin:/bin",
        "TMPDIR": str(tmp),
        "HOME": str(home),
        "CLAUDE_CONFIG_DIR": str(config),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    completed = subprocess.run(
        [str(BINARY), "plugin", "test", str(ROOT)],
        cwd=temp,
        env=env,
        capture_output=True,
        timeout=45,
        check=False,
    )

stdout = completed.stdout
stderr = completed.stderr
(OUTPUT / "native.stdout.raw").write_bytes(stdout)
(OUTPUT / "native.stderr.raw").write_bytes(stderr)
ui_records = [
    line.decode("utf-8", errors="backslashreplace")
    for line in stderr.splitlines()
    if line.startswith(b"NATIVE_MOD_UI_")
]
receipt = {
    "native_version": "2.1.295",
    "native_sha256": binary_sha,
    "test_module_sha256": sha256((ROOT / "ui.test.ts").read_bytes()),
    "hook_module_sha256": sha256((ROOT / "hooks" / "register.js").read_bytes()),
    "isolated_home_and_config": True,
    "native_exit_code": completed.returncode,
    "stdout_bytes": len(stdout),
    "stdout_sha256": sha256(stdout),
    "stderr_bytes": len(stderr),
    "stderr_sha256": sha256(stderr),
    "ui_records": ui_records,
    "notify_test_host_result": "no implementation for ui.notify",
    "scope": "Native 2.1.295 public plugin-test ui.render child tree and ui.notify string call boundary; no desktop notification delivery claim",
}
(OUTPUT / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n")
print(json.dumps(receipt, ensure_ascii=False, indent=2))
raise SystemExit(0 if completed.returncode == 0 else 1)
