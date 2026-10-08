#!/usr/bin/env python3
"""Capture embedded Native Bun string-width and grapheme segmentation."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
MARKER = b"NATIVE_BUN_UNICODE_ORACLE="
DUMMY_KEY = "native-bun-unicode-fixture-dummy-key"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def response_stream() -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_native_bun_unicode_oracle", "type": "message",
            "role": "assistant", "model": "claude-sonnet-4-5-20250929",
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 5, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": "unicode oracle captured"}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "unicode oracle captured"}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
            "usage": {"output_tokens": 2}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


binary_text = os.environ.get("CLAUDE_NATIVE_BINARY")
if not binary_text:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
binary = Path(binary_text).expanduser().resolve()
binary_sha = sha256(binary.read_bytes())
if binary_sha != EXPECTED_SHA256:
    raise SystemExit(f"Native SHA mismatch: {binary_sha}")
version = subprocess.run([str(binary), "--version"], capture_output=True, check=False)
version_text = (version.stdout + version.stderr).decode("utf-8", errors="replace").strip()
if "2.1.293" not in version_text:
    raise SystemExit(f"Native version mismatch: {version_text!r}")

if OUTPUT.exists():
    shutil.rmtree(OUTPUT)
OUTPUT.mkdir(parents=True)
for name in ("home", "config", "tmp", "cwd"):
    (OUTPUT / name).mkdir()
request_records = []


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length)
        accepted = (
            self.client_address[0] == "127.0.0.1"
            and self.path == "/v1/messages?beta=true"
            and self.headers.get("x-api-key") == DUMMY_KEY
        )
        record = {"method": "POST", "path": self.path, "accepted": accepted,
                  "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                  "request_bytes": len(raw), "request_sha256": sha256(raw)}
        request_records.append(record)
        if accepted:
            (OUTPUT / "request.full.raw.json").write_bytes(raw)
            body = response_stream()
            (OUTPUT / "response.raw.sse").write_bytes(body)
            status, content_type = 200, "text/event-stream"
        else:
            body, status, content_type = b"{}", 404, "application/json"
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", content_type)
        self.end_headers()
        self.wfile.write(body)


server = HTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
env = {
    "PATH": "/usr/bin:/bin",
    "TMPDIR": str(OUTPUT / "tmp"),
    "LANG": "C.UTF-8",
    "TZ": "UTC",
    "TERM": "dumb",
    "HOME": str(OUTPUT / "home"),
    "CLAUDE_CONFIG_DIR": str(OUTPUT / "config"),
    "ANTHROPIC_API_KEY": DUMMY_KEY,
    "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
    "ANTHROPIC_MAX_RETRIES": "0",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "DISABLE_TELEMETRY": "1",
    "DO_NOT_TRACK": "1",
}
argv = [
    str(binary), "--plugin-dir", str(ROOT / "plugin"), "--setting-sources", "project",
    "--strict-mcp-config", "--print", "--no-session-persistence", "--tools", "",
    "--dangerously-skip-permissions", "--debug", "hooks", "--max-turns", "1",
    "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"], FIXTURE["prompt"],
]
try:
    completed = subprocess.run(argv, cwd=OUTPUT / "cwd", env=env, capture_output=True,
                               timeout=60, check=False)
    stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
except subprocess.TimeoutExpired as error:
    stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)

(OUTPUT / "native.stdout.raw").write_bytes(stdout)
(OUTPUT / "native.stderr.raw").write_bytes(stderr)
marker_lines = [(name, line) for name, data in (("stdout", stdout), ("stderr", stderr))
                for line in data.splitlines() if MARKER in line]
for debug_path in (OUTPUT / "config" / "debug").glob("*.txt"):
    marker_lines.extend((str(debug_path.relative_to(OUTPUT)), line)
                        for line in debug_path.read_bytes().splitlines() if MARKER in line)
marker_payload = None
marker_source = None
result = None
if len(marker_lines) == 1:
    marker_source, marker_raw = marker_lines[0]
    marker_payload = marker_raw[marker_raw.index(MARKER) + len(MARKER):]
    result = json.loads(marker_payload)
    (OUTPUT / "unicode-oracle-marker.raw.txt").write_bytes(marker_raw)
    (OUTPUT / "unicode-oracle.raw.json").write_bytes(marker_payload)

receipt = {
    "native_version": version_text,
    "native_sha256": binary_sha,
    "runner_sha256": sha256(Path(__file__).read_bytes()),
    "module_sha256": sha256((ROOT / "plugin" / "hooks" / "register.js").read_bytes()),
    "argv": ["$CLAUDE_NATIVE_BINARY", "--plugin-dir", "<fixture-plugin>",
             "--setting-sources", "project", "--strict-mcp-config", "--print",
             "--no-session-persistence", "--tools", "", "--dangerously-skip-permissions",
             "--debug", "hooks", "--max-turns", "1", "--model", FIXTURE["model"],
             "--session-id", FIXTURE["session_id"], FIXTURE["prompt"]],
    "model_request_is_fixed_loopback_stub": True,
    "real_model_or_external_network": False,
    "isolated_home_config_tmp": True,
    "timed_out": timed_out,
    "exit_code": exit_code,
    "requests": request_records,
    "marker_count": len(marker_lines),
    "marker_source": marker_source,
    "unicode_oracle_raw": {
        "path": "unicode-oracle.raw.json",
        "bytes": len(marker_payload) if marker_payload is not None else None,
        "sha256": sha256(marker_payload) if marker_payload is not None else None,
    } if marker_payload is not None else None,
    "bun_version": result.get("bunVersion") if result else None,
    "process_bun_version": result.get("processBunVersion") if result else None,
    "process_node_version": result.get("processNodeVersion") if result else None,
    "process_global_available": result.get("processGlobalAvailable") if result else None,
    "bun_global_available": result.get("bunGlobalAvailable") if result else None,
    "claude_code_exports": result.get("claudeCodeExports") if result else None,
    "width_provider": result.get("widthProvider") if result else None,
    "width_import_error": result.get("widthImportError") if result else None,
    "string_width_available": result.get("stringWidthAvailable") if result else None,
    "segmenter_available": result.get("segmenterAvailable") if result else None,
    "samples": result.get("samples") if result else None,
    "stdout": {"path": "native.stdout.raw", "bytes": len(stdout), "sha256": sha256(stdout)},
    "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
}
(OUTPUT / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n")
print(json.dumps(receipt, ensure_ascii=True, indent=2))
if timed_out or exit_code != 0 or len(request_records) != 1 or len(marker_lines) != 1:
    raise SystemExit(1)
