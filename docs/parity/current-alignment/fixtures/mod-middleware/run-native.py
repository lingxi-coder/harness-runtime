#!/usr/bin/env python3
"""Run the pinned Native plugin-test contract with isolated local state."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
NATIVE_PATH = os.environ.get("CLAUDE_NATIVE_BINARY")
if not NATIVE_PATH:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
BINARY = Path(NATIVE_PATH).expanduser().resolve()
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


actual = sha256(BINARY.read_bytes())
if actual != EXPECTED_SHA256:
    raise SystemExit(f"Native SHA mismatch: {actual}")

if OUTPUT.exists():
    shutil.rmtree(OUTPUT)
OUTPUT.mkdir(parents=True)
with tempfile.TemporaryDirectory(prefix="native-mod-middleware-") as temp_dir:
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
    Path(env["TMPDIR"]).mkdir()
    Path(env["CLAUDE_CONFIG_DIR"]).mkdir()
    Path(env["HOME"]).mkdir()
    argv = [str(BINARY), "plugin", "test", str(ROOT)]
    completed = subprocess.run(argv, cwd=temp, env=env, capture_output=True, timeout=45, check=False)

stdout = completed.stdout
stderr = completed.stderr
(OUTPUT / "native.stdout").write_bytes(stdout)
(OUTPUT / "native.stderr").write_bytes(stderr)
raw_events = []
for stream_name, stream in (("stdout", stdout), ("stderr", stderr)):
    raw_events.extend({"stream": stream_name, "json": line.removeprefix(b"NATIVE_MOD_RAW_EVENT=")}
                      for line in stream.splitlines()
                      if line.startswith(b"NATIVE_MOD_RAW_EVENT="))

probe_root = OUTPUT / "unregister-probe"
shutil.copytree(ROOT / "unregister-probe", probe_root)
with tempfile.TemporaryDirectory(prefix="native-mod-unregister-probe-") as temp_dir:
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
    Path(env["TMPDIR"]).mkdir()
    Path(env["CLAUDE_CONFIG_DIR"]).mkdir()
    Path(env["HOME"]).mkdir()
    probe_argv = [str(BINARY), "plugin", "test", str(probe_root)]
    probe = subprocess.run(probe_argv, cwd=temp, env=env, capture_output=True, timeout=45, check=False)
probe_stdout, probe_stderr = probe.stdout, probe.stderr
(probe_root / "native.stdout").write_bytes(probe_stdout)
(probe_root / "native.stderr").write_bytes(probe_stderr)
probe_output = probe_stdout + probe_stderr
disposer_policy_rejected = b'value of on("session.append") is kept' in probe_output

receipt = {
    "native_version": "2.1.293",
    "native_sha256": actual,
    "fixture_module_sha256": sha256((ROOT / "hooks/register.js").read_bytes()),
    "argv": argv,
    "isolated_config": True,
    "native_exit_code": completed.returncode,
    "stdout_bytes": len(stdout),
    "stdout_sha256": sha256(stdout),
    "stderr_bytes": len(stderr),
    "stderr_sha256": sha256(stderr),
    "raw_event_lines_utf8": [
        {"stream": item["stream"], "json": item["json"].decode("utf-8", errors="backslashreplace")}
        for item in raw_events
    ],
    "module_registration_observed_by_rewrite_case": any(b'"case":"rewrite"' in item["json"]
                                                         for item in raw_events),
    "native_plugin_test_process_exits_after_tests": completed.returncode is not None,
    "per_handler_unregistration_api": "not observed; Native plugin static policy rejects retaining on() return handles",
    "on_return_disposer_probe": {
        "argv": probe_argv,
        "native_exit_code": probe.returncode,
        "stdout_bytes": len(probe_stdout),
        "stdout_sha256": sha256(probe_stdout),
        "stderr_bytes": len(probe_stderr),
        "stderr_sha256": sha256(probe_stderr),
        "stdout_file": "unregister-probe/native.stdout",
        "stderr_file": "unregister-probe/native.stderr",
        "static_policy_rejected_retained_on_result": disposer_policy_rejected,
        "conclusion": "on() return cannot be kept as an explicit per-handler unsubscribe handle; module/process unload lifecycle remains unverified",
    },
    "test_result": "passed" if completed.returncode == 0 else "failed",
    "scope": "Native 2.1.293 public plugin-test/session.append middleware contract; process exit is test-run boundary, not proof of explicit unload hook; not whole Mods parity",
}
(OUTPUT / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n")
print(json.dumps(receipt, ensure_ascii=False, indent=2))
raise SystemExit(0 if completed.returncode == 0 and disposer_policy_rejected else 1)
