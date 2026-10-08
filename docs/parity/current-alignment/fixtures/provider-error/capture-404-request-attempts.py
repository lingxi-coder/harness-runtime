#!/usr/bin/env python3
"""Capture only Native 404 request bodies to explain its two loopback POSTs."""
from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output" / "status-404-attempts"
NATIVE_PATH = os.environ.get("CLAUDE_NATIVE_BINARY")
if not NATIVE_PATH:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
BINARY = Path(NATIVE_PATH).expanduser().resolve()
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
CASES = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
STATUS = json.loads((ROOT / "status-controls.json").read_text(encoding="utf-8"))
CASE = next(case for case in STATUS["cases"] if case["http_status"] == 404)
BODY_CASE = next(case for case in CASES["cases"] if case["id"] == STATUS["response_body_case"])
RESPONSE = bytes.fromhex(BODY_CASE["response_body_hex"])
DUMMY_KEY = "native-provider-error-fixture-dummy-key"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def changed_paths(left: object, right: object, path: str = "") -> list[str]:
    if type(left) is not type(right):
        return [path or "/"]
    if isinstance(left, dict) and isinstance(right, dict):
        output = []
        for key in sorted(set(left) | set(right)):
            child = f"{path}/{key}"
            if key not in left or key not in right:
                output.append(child)
            else:
                output.extend(changed_paths(left[key], right[key], child))
        return output
    if isinstance(left, list) and isinstance(right, list):
        output = []
        for index in range(max(len(left), len(right))):
            child = f"{path}/{index}"
            if index >= len(left) or index >= len(right):
                output.append(child)
            else:
                output.extend(changed_paths(left[index], right[index], child))
        return output
    return [] if left == right else [path or "/"]


binary_sha = sha256(BINARY.read_bytes())
if binary_sha != EXPECTED_SHA256:
    raise SystemExit(f"Native SHA mismatch: {binary_sha}")
if OUTPUT.exists():
    shutil.rmtree(OUTPUT)
OUTPUT.mkdir(parents=True)
cwd = OUTPUT / "cwd"
config = OUTPUT / "config"
home = OUTPUT / "home"
temp = OUTPUT / "tmp"
for directory in (cwd, config, home, temp):
    directory.mkdir(parents=True, exist_ok=True)

requests: list[dict] = []


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_HEAD(self) -> None:
        self.send_response(200 if self.client_address[0] == "127.0.0.1" and self.path == "/api/hello" else 404)
        self.end_headers()

    def do_POST(self) -> None:
        raw = self.rfile.read(int(self.headers.get("content-length", "0")))
        accepted = (
            self.client_address[0] == "127.0.0.1"
            and self.path == "/v1/messages?beta=true"
            and self.headers.get("x-api-key") == DUMMY_KEY
        )
        if not accepted:
            self.send_response(404)
            self.end_headers()
            return
        index = len(requests) + 1
        body_path = OUTPUT / f"request-{index}.body.raw"
        body_path.write_bytes(raw)
        requests.append({
            "index": index,
            "path": self.path,
            "accepted_local_dummy_key": True,
            "body_path": str(body_path.relative_to(ROOT)),
            "body_bytes": len(raw),
            "body_sha256": sha256(raw),
            "headers": {
                "content-type": self.headers.get("content-type"),
                "anthropic-version": self.headers.get("anthropic-version"),
                "anthropic-beta": self.headers.get("anthropic-beta"),
            },
        })
        self.send_response(404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(RESPONSE)))
        self.end_headers()
        self.wfile.write(RESPONSE)


server = HTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
try:
    argv = [
        str(BINARY), "--setting-sources", "project", "--strict-mcp-config",
        "--print", "--output-format", "json", "--no-session-persistence",
        "--tools", "", "--max-budget-usd", "1",
        "--model", STATUS["model"], "--session-id", CASE["session_id"],
        STATUS["prompt"],
    ]
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(home),
        "TMPDIR": str(temp),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "CLAUDE_CONFIG_DIR": str(config),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": str(STATUS["max_retries"]),
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    try:
        completed = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=15, check=False)
        stdout, stderr = completed.stdout, completed.stderr
        exit_code, timed_out = completed.returncode, False
    except subprocess.TimeoutExpired as error:
        stdout, stderr = error.stdout or b"", error.stderr or b""
        exit_code, timed_out = None, True
    (OUTPUT / "native.stdout.raw").write_bytes(stdout)
    (OUTPUT / "native.stderr.raw").write_bytes(stderr)
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)

parsed = [json.loads((OUTPUT / f"request-{item['index']}.body.raw").read_bytes()) for item in requests]
model_and_stream = [
    {
        "model": item.get("model"),
        "stream": item.get("stream"),
        "top_level_keys": sorted(item.keys()),
        "message_count": len(item.get("messages", [])) if isinstance(item.get("messages"), list) else None,
    }
    for item in parsed
]
diffs = changed_paths(parsed[0], parsed[1]) if len(parsed) == 2 else []
receipt = {
    "native_version": STATUS["native_version"],
    "native_sha256": binary_sha,
    "mode": "public --print --output-format json; session persistence disabled",
    "fixed_model": STATUS["model"],
    "fixed_prompt": STATUS["prompt"],
    "fixed_session_id": CASE["session_id"],
    "response_status": 404,
    "response_body_case": STATUS["response_body_case"],
    "response_body_sha256": sha256(RESPONSE),
    "max_retries_environment": STATUS["max_retries"],
    "network_boundary": "127.0.0.1-only; fixed dummy key; no upstream relay",
    "request_count": len(requests),
    "requests": requests,
    "request_model_stream_constraints": model_and_stream,
    "request_bodies_byte_equal": len(requests) == 2 and (OUTPUT / "request-1.body.raw").read_bytes() == (OUTPUT / "request-2.body.raw").read_bytes(),
    "changed_json_paths_only_names_not_values": diffs,
    "exit_code": exit_code,
    "timed_out": timed_out,
    "stdout_bytes": len(stdout),
    "stdout_sha256": sha256(stdout),
    "stderr_bytes": len(stderr),
    "stderr_sha256": sha256(stderr),
    "capture_boundary": "Complete request bodies and CLI outputs are local ignored run-output only; receipt exposes hashes and selected model/stream fields, not body values or metadata.user_id.",
}
(OUTPUT / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n")
portable_receipt = {
    "native_version": receipt["native_version"],
    "native_sha256": receipt["native_sha256"],
    "runner": "capture-404-request-attempts.py",
    "fixed_input_reference": "../status-controls.json case http-404",
    "http_status": 404,
    "response_body_case": receipt["response_body_case"],
    "response_body_sha256": receipt["response_body_sha256"],
    "max_retries_environment": 0,
    "network_boundary": receipt["network_boundary"],
    "request_count": len(requests),
    "requests": [
        {
            "index": request["index"],
            "body_bytes": request["body_bytes"],
            "body_sha256": request["body_sha256"],
            "model": shape["model"],
            "stream": shape["stream"],
            "top_level_keys": shape["top_level_keys"],
            "message_count": shape["message_count"],
            "headers": request["headers"],
        }
        for request, shape in zip(requests, model_and_stream, strict=True)
    ],
    "request_bodies_byte_equal": receipt["request_bodies_byte_equal"],
    "changed_json_paths": diffs,
    "native_result_sha256": receipt["stdout_sha256"],
    "raw_request_body_location": "Ignored run-output/status-404-attempts/request-{1,2}.body.raw; rerun the portable capture script to regenerate.",
    "account_identifying_metadata": "Full request bodies are not checked in; metadata.user_id and values are omitted from this receipt.",
}
baseline_receipt = ROOT / "native-baseline" / "status-controls" / "404-request-shape-receipt.json"
baseline_receipt.write_text(json.dumps(portable_receipt, ensure_ascii=True, indent=2) + "\n")
print(json.dumps(receipt, ensure_ascii=True, indent=2))
if len(requests) != 2 or not all(record["accepted_local_dummy_key"] for record in requests):
    raise SystemExit("Expected exactly two accepted loopback requests")
