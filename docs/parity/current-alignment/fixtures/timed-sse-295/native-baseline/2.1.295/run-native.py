#!/usr/bin/env python3
"""Probe Native 2.1.295 timing fields against a deliberately delayed SSE stream."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parent
FIXTURE_PATH = ROOT / "cases.json"
FIXTURE = json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))
EXPECTED_SHA256 = FIXTURE["native_sha256"]
DUMMY_KEY = "native-timed-sse-295-dummy-key"
OUTPUT = ROOT / "run-output"
OUTPUT.mkdir(parents=True, exist_ok=True)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sse_events() -> list[tuple[str, dict]]:
    message_id = FIXTURE["message_id"]
    events = []
    for item in FIXTURE["events"]:
        kind = item["event"]
        if kind == "message_start":
            name = "message_start"
            data = {"type": "message_start", "message": {
                "id": message_id, "type": "message", "role": "assistant",
                "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
                "stop_sequence": None,
                "usage": {"input_tokens": 10, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            }}
        elif kind == "thinking_block_start":
            name = "content_block_start"
            data = {"type": name, "index": 0, "content_block": {"type": "thinking", "thinking": ""}}
        elif kind == "thinking_delta":
            name = "content_block_delta"
            data = {"type": name, "index": 0, "delta": {"type": "thinking_delta", "thinking": FIXTURE["thinking_text"]}}
        elif kind == "signature_delta":
            name = "content_block_delta"
            data = {"type": name, "index": 0, "delta": {"type": "signature_delta", "signature": "fixed-native-timing-signature"}}
        elif kind == "thinking_block_stop":
            name = "content_block_stop"
            data = {"type": name, "index": 0}
        elif kind == "text_block_start":
            name = "content_block_start"
            data = {"type": name, "index": 1, "content_block": {"type": "text", "text": ""}}
        elif kind == "text_delta":
            name = "content_block_delta"
            data = {"type": name, "index": 1, "delta": {"type": "text_delta", "text": FIXTURE["text"]}}
        elif kind == "text_block_stop":
            name = "content_block_stop"
            data = {"type": name, "index": 1}
        elif kind == "message_delta":
            name = "message_delta"
            data = {"type": name, "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                    "usage": {"output_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}
        elif kind == "message_stop":
            name = "message_stop"
            data = {"type": name}
        else:
            raise ValueError(f"unknown fixture SSE event {kind}")
        events.append((name, {"delay_before_ms": item["delay_before_ms"], "data": data}))
    return events


def main() -> int:
    binary_value = os.environ.get("CLAUDE_NATIVE_BINARY")
    if not binary_value:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the pinned Native 2.1.295 executable")
    binary = Path(binary_value).expanduser().resolve()
    binary_bytes = binary.read_bytes()
    if sha256(binary_bytes) != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {sha256(binary_bytes)}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or version_text != "2.1.295 (Claude Code)":
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    run_root = Path(tempfile.mkdtemp(prefix="native-timed-sse-295-", dir=OUTPUT))
    for name in ("home", "config", "tmp", "model-responses"):
        (run_root / name).mkdir()
    external_cwd = Path(tempfile.mkdtemp(prefix="native-timed-sse-295-cwd-", dir="/private/tmp"))
    event_trace: list[dict] = []
    request_trace: list[dict] = []
    raw_sse_parts: list[bytes] = []
    process_start_ns = [None]
    event_lock = threading.Lock()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def do_POST(self) -> None:
            request_received_ns = time.monotonic_ns()
            body = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            record = {
                "method": "POST", "path": self.path, "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(body), "request_sha256": sha256(body),
                "request_received_since_native_spawn_ms": round((request_received_ns - process_start_ns[0]) / 1_000_000, 3),
            }
            request_trace.append(record)
            (run_root / "model-request.raw.json").write_bytes(body)
            if not accepted:
                self.send_response(404 if self.path != "/v1/messages?beta=true" else 401)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")
                return

            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "keep-alive")
            self.end_headers()
            response_headers_ns = time.monotonic_ns()
            record["response_headers_since_request_ms"] = round((response_headers_ns - request_received_ns) / 1_000_000, 3)
            (run_root / "http-response-headers.raw").write_bytes(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: keep-alive\r\n\r\n"
            )

            for index, (event_name, item) in enumerate(sse_events(), start=1):
                time.sleep(item["delay_before_ms"] / 1000)
                event_data = json.dumps(item["data"], ensure_ascii=True, separators=(",", ":"))
                chunk = f"event: {event_name}\ndata: {event_data}\n\n".encode("utf-8")
                with event_lock:
                    raw_sse_parts.append(chunk)
                self.wfile.write(f"{len(chunk):X}\r\n".encode("ascii"))
                self.wfile.write(chunk)
                self.wfile.write(b"\r\n")
                self.wfile.flush()
                written_ns = time.monotonic_ns()
                event_trace.append({
                    "index": index, "fixture_event": item["data"]["type"], "sse_event": event_name,
                    "delay_before_ms": item["delay_before_ms"],
                    "write_since_response_headers_ms": round((written_ns - response_headers_ns) / 1_000_000, 3),
                    "write_since_request_received_ms": round((written_ns - request_received_ns) / 1_000_000, 3),
                    "write_since_native_spawn_ms": round((written_ns - process_start_ns[0]) / 1_000_000, 3),
                    "chunk_bytes": len(chunk), "chunk_sha256": sha256(chunk),
                })
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            record["response_finished_since_request_ms"] = round((time.monotonic_ns() - request_received_ns) / 1_000_000, 3)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = {
        "PATH": "/usr/bin:/bin", "TMPDIR": str(run_root / "tmp"), "LANG": "C.UTF-8", "TZ": "UTC",
        "TERM": "dumb", "HOME": str(run_root / "home"), "CLAUDE_CONFIG_DIR": str(run_root / "config"),
        "ANTHROPIC_API_KEY": DUMMY_KEY, "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1",
    }
    args = [
        "--bare", "--print", "--output-format", "json", "--tools", "",
        "--permission-mode", "dontAsk", "--permission-prompts", "none", "--max-turns", "1",
        "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"],
        "--system-prompt", FIXTURE["system_prompt"], FIXTURE["prompt"],
    ]
    process_start_ns[0] = time.monotonic_ns()
    started = time.monotonic()
    timed_out = False
    try:
        completed = subprocess.run([str(binary), *args], cwd=external_cwd, env=env,
                                   capture_output=True, timeout=FIXTURE["timeout_seconds"], check=False)
        stdout, stderr, exit_code = completed.stdout, completed.stderr, completed.returncode
    except subprocess.TimeoutExpired as error:
        stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    (run_root / "native.stdout.raw.json").write_bytes(stdout)
    (run_root / "native.stderr.raw").write_bytes(stderr)
    (run_root / "scheduled-events.raw.sse").write_bytes(b"".join(raw_sse_parts))
    try:
        parsed = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        parsed = None
    timing_fields = {}
    if isinstance(parsed, dict):
        timing_fields = {
            key: parsed[key] for key in (
                "time_to_request_ms", "ttft_stream_ms", "first_content_frame_ms",
                "ttft_ms", "first_output_token_ms", "duration_api_ms", "duration_ms",
            ) if key in parsed
        }
    receipt = {
        "native_version": version_text,
        "native_sha256": sha256(binary_bytes),
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "fixture_sha256": sha256(FIXTURE_PATH.read_bytes()),
        "native_path": str(binary),
        "public_entrypoint": "Native 2.1.295 --bare --print --output-format json; one loopback Anthropic Messages SSE response",
        "fixed_inputs": {key: FIXTURE[key] for key in ("session_id", "model", "system_prompt", "prompt", "events")},
        "network_boundary": {"model_endpoint": "127.0.0.1 loopback only", "dummy_api_key_only": True, "isolated_home_config": True, "upstream_requests": 0},
        "timeout_seconds": FIXTURE["timeout_seconds"],
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "native_stdout": {"path": "native.stdout.raw.json", "bytes": len(stdout), "sha256": sha256(stdout)},
        "native_stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
        "native_result": ({key: parsed.get(key) for key in ("type", "subtype", "is_error", "result", "num_turns", "stop_reason", "session_id")} if isinstance(parsed, dict) else None),
        "native_timing_fields_ms": timing_fields,
        "request_trace": request_trace,
        "sse_schedule": FIXTURE["events"],
        "sse_actual_write_trace": event_trace,
        "raw_sse": {"path": "scheduled-events.raw.sse", "bytes": sum(len(part) for part in raw_sse_parts), "sha256": sha256(b"".join(raw_sse_parts))},
        "raw_request": {"path": "model-request.raw.json", "bytes": (run_root / "model-request.raw.json").stat().st_size if (run_root / "model-request.raw.json").exists() else 0,
                         "sha256": sha256((run_root / "model-request.raw.json").read_bytes()) if (run_root / "model-request.raw.json").exists() else None},
        "raw_outputs_relative_path": str(run_root.relative_to(ROOT)),
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    shutil.rmtree(external_cwd, ignore_errors=True)
    print(json.dumps({
        "native_version": version_text, "native_exit_code": exit_code, "timed_out": timed_out,
        "elapsed_seconds": receipt["elapsed_seconds"], "native_result": receipt["native_result"],
        "native_timing_fields_ms": timing_fields, "request_trace": request_trace,
        "sse_actual_write_trace": event_trace, "receipt": str((run_root / "receipt.json").relative_to(ROOT)),
    }, ensure_ascii=False, indent=2))
    return 1 if timed_out or exit_code != 0 or not request_trace else 0


if __name__ == "__main__":
    raise SystemExit(main())
