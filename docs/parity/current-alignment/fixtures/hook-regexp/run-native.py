#!/usr/bin/env python3
"""Run the public Native CLI Hook matcher fixture with loopback-only routing."""
import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
NATIVE_PATH = os.environ.get("CLAUDE_NATIVE_BINARY")
if not NATIVE_PATH:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
BINARY = Path(NATIVE_PATH).expanduser().resolve()
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-hook-fixture-dummy-key"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


class LoopbackHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    requests = []

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def _reply(self, status: int, body: bytes = b"", content_type: str = "application/json") -> None:
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        if body:
            self.send_header("Content-Type", content_type)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_HEAD(self) -> None:
        if self.client_address[0] != "127.0.0.1" or self.path != "/api/hello":
            self.requests.append({"method": "HEAD", "path": self.path, "accepted": False})
            self._reply(404)
            return
        self.requests.append({"method": "HEAD", "path": self.path, "accepted": True})
        self._reply(200)

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        if self.client_address[0] != "127.0.0.1" or self.path != "/v1/messages?beta=true":
            self.requests.append({"method": "POST", "path": self.path, "accepted": False})
            self._reply(404)
            return
        if length > 2 * 1024 * 1024:
            self.requests.append({"method": "POST", "path": self.path, "accepted": False, "reason": "body-too-large"})
            self._reply(413)
            return
        raw = self.rfile.read(length)
        if self.headers.get("x-api-key") != DUMMY_KEY:
            self.requests.append({"method": "POST", "path": self.path, "accepted": False, "reason": "dummy-key-mismatch"})
            self._reply(401)
            return
        try:
            request = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            self.requests.append({"method": "POST", "path": self.path, "accepted": False, "reason": "invalid-json"})
            self._reply(400)
            return
        self.requests.append({
            "method": "POST", "path": self.path, "accepted": True,
            "dummy_key_matched": True, "request_bytes": length,
            "request_sha256": hashlib.sha256(raw).hexdigest(),
            "stream": request.get("stream") is True,
        })
        if request.get("stream") is True:
            events = [
                ("message_start", {"type": "message_start", "message": {
                    "id": "msg_acceptance_fixture", "type": "message", "role": "assistant",
                    "model": "claude-sonnet-4-5-20250929", "content": [],
                    "stop_reason": None, "stop_sequence": None,
                    "usage": {"input_tokens": 8, "output_tokens": 0,
                              "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
                }}),
                ("content_block_start", {"type": "content_block_start", "index": 0,
                                          "content_block": {"type": "text", "text": ""}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                          "delta": {"type": "text_delta", "text": "fixture complete"}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta",
                                   "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                                   "usage": {"output_tokens": 3}}),
                ("message_stop", {"type": "message_stop"}),
            ]
            body = "".join(f"event: {name}\ndata: {json.dumps(value, separators=(',', ':'))}\n\n"
                           for name, value in events).encode("utf-8")
            self._reply(200, body, "text/event-stream")
        else:
            body = json.dumps({
                "id": "msg_acceptance_fixture", "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-5-20250929",
                "content": [{"type": "text", "text": "fixture complete"}],
                "stop_reason": "end_turn", "stop_sequence": None,
                "usage": {"input_tokens": 8, "output_tokens": 3},
            }, separators=(",", ":")).encode("utf-8")
            self._reply(200, body)


def main() -> int:
    actual_binary_hash = sha256(BINARY)
    if actual_binary_hash != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {actual_binary_hash}")

    if OUTPUT.exists():
        shutil.rmtree(OUTPUT)
    OUTPUT.mkdir(parents=True)
    subprocess.run([sys.executable, str(ROOT / "prepare_native_plugin.py"), str(OUTPUT)], check=True)
    marker = OUTPUT / "fired.log"
    marker.unlink(missing_ok=True)

    run_root = OUTPUT / "run"
    workdir = run_root / "cwd"
    config = run_root / "config"
    temp = run_root / "tmp"
    workdir.mkdir(parents=True, exist_ok=True)
    config.mkdir(parents=True, exist_ok=True)
    temp.mkdir(parents=True, exist_ok=True)
    (run_root / "home").mkdir(parents=True, exist_ok=True)

    LoopbackHandler.requests = []
    server = HTTPServer(("127.0.0.1", 0), LoopbackHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()

    env = {
        "PATH": "/usr/bin:/bin",
        "TMPDIR": str(temp),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "HOME": str(run_root / "home"),
        "CLAUDE_CONFIG_DIR": str(config),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    argv = [
        str(BINARY), "--setting-sources", "project", "--strict-mcp-config",
        "--plugin-dir", str(OUTPUT / "native-plugin"),
        "--print", "--no-session-persistence", "--tools", "",
        "--max-budget-usd", "1", "--model", "claude-sonnet-4-5",
        "--session-id", "00000000-0000-4000-8000-000000002936",
        "--debug", "hooks",
        "Run the Hook matcher fixture.",
    ]
    try:
        completed = subprocess.run(
            argv, cwd=workdir, env=env, capture_output=True, timeout=60, check=False
        )
        timed_out = False
        exit_code = completed.returncode
        stdout = completed.stdout
        stderr = completed.stderr
    except subprocess.TimeoutExpired as error:
        timed_out = True
        exit_code = None
        stdout = error.stdout or b""
        stderr = error.stderr or b""
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    (OUTPUT / "native.stdout").write_bytes(stdout)
    (OUTPUT / "native.stderr").write_bytes(stderr)
    actual = marker.read_text(encoding="utf-8").splitlines() if marker.exists() else []
    cases = json.loads((ROOT / "matcher-cases.json").read_text(encoding="utf-8"))
    expected = [case["id"] for case in cases["native_comparable"] if case["expected"]]
    forbidden = [case["id"] for case in cases["native_comparable"] if not case["expected"]]
    native_inputs = {}
    for case in cases["native_comparable"]:
        path = OUTPUT / "inputs" / f"{case['id']}.json"
        if not path.is_file():
            continue
        raw_input = path.read_bytes()
        try:
            event = json.loads(raw_input)
            source = event.get("source") if isinstance(event, dict) else None
        except (UnicodeDecodeError, json.JSONDecodeError):
            source = None
        native_inputs[case["id"]] = {
            "bytes": len(raw_input),
            "sha256": hashlib.sha256(raw_input).hexdigest(),
            "source": source,
        }
    debug = stderr.decode("utf-8", errors="replace")
    mentions_session_start = "SessionStart" in debug or "session start" in debug.lower()
    mentions_startup_query = "startup" in debug.lower()
    sources_confirmed = (
        len(native_inputs) == len(expected)
        and all(native_inputs[case_id]["source"] == "startup" for case_id in expected)
    )
    receipt = {
        "native_version": "2.1.293",
        "native_sha256": actual_binary_hash,
        "argv": argv,
        "working_directory": str(workdir),
        "network_boundary": {
            "base_url": env["ANTHROPIC_BASE_URL"],
            "dummy_api_key_only": True,
            "proxy_variables_passed": False,
            "telemetry_disabled_by_fixture_environment": True,
            "upstream_relay": False,
            "requests": LoopbackHandler.requests,
        },
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "stdout_bytes": len(stdout),
        "stdout_sha256": hashlib.sha256(stdout).hexdigest(),
        "stderr_bytes": len(stderr),
        "stderr_sha256": hashlib.sha256(stderr).hexdigest(),
        "expected_matches": expected,
        "actual_matches": actual,
        "native_event_inputs": native_inputs,
        "all_expected_event_sources_are_startup": sources_confirmed,
        "expected_nonmatches_absent": all(item not in actual for item in forbidden),
        "all_expected_matches_observed": sorted(actual) == sorted(expected),
        "debug_mentions_session_start": mentions_session_start,
        "debug_mentions_startup_query": mentions_startup_query,
        "native_match_result": "passed" if (
            sorted(actual) == sorted(expected)
            and all(item not in actual for item in forbidden)
            and sources_confirmed
        ) else "unverified",
        "scope": "Native public CLI SessionStart command-hook matcher fixture only; not a Harness byte-parity claim",
    }
    (OUTPUT / "native-receipt.json").write_text(
        json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    print(json.dumps(receipt, ensure_ascii=False, indent=2))
    return 0 if receipt["native_match_result"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
