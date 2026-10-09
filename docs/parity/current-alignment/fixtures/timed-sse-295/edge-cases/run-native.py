#!/usr/bin/env python3
"""Capture Native 2.1.295 timing through fallback and retry request paths."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parent
FIXTURE_ROOT = ROOT.parent
FIXTURE_PATH = ROOT / "cases.json"
FIXTURE = json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))
CASE_BY_ID = {case["id"]: case for case in FIXTURE["cases"]}
OUTPUT = FIXTURE_ROOT / "run-output"
OUTPUT.mkdir(parents=True, exist_ok=True)
DEFAULT_BINARY = Path("/Users/luolingfeng/.local/share/claude/versions/2.1.295")
DUMMY_KEY = "native-timed-sse-edge-295-dummy-key"


def digest(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def sse_bytes(case: dict) -> tuple[list[tuple[str, bytes]], list[dict]]:
    message_id = f"msg_native_{case['id']}"
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 0,
                      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
        }}, case["message_start_delay_ms"]),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "text", "text": ""}},
         case["content_block_start_delay_ms"]),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": case["success_text"]}},
         case["text_delta_delay_ms"]),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}, 0),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 5, "cache_creation_input_tokens": 0,
                                      "cache_read_input_tokens": 0}}, 0),
        ("message_stop", {"type": "message_stop"}, 0),
    ]
    parts = []
    trace = []
    for index, (name, data, delay_ms) in enumerate(events, start=1):
        chunk = f"event: {name}\ndata: {json.dumps(data, ensure_ascii=True, separators=(',', ':'))}\n\n".encode("ascii")
        parts.append((name, chunk))
        trace.append({"index": index, "event": data["type"], "delay_before_ms": delay_ms,
                      "chunk_bytes": len(chunk), "chunk_sha256": digest(chunk)})
    return parts, trace


def send_fixed_response(handler: BaseHTTPRequestHandler, status: int, body: bytes,
                        content_type: str, headers: list[tuple[str, str]] | None = None) -> bytes:
    handler.send_response(status)
    handler.send_header("Content-Type", content_type)
    handler.send_header("Content-Length", str(len(body)))
    for name, value in headers or []:
        handler.send_header(name, value)
    handler.end_headers()
    handler.wfile.write(body)
    handler.wfile.flush()
    return body


def read_session(config_root: Path, session_id: str) -> bytes:
    paths = sorted(config_root.rglob(f"{session_id}.jsonl"))
    if len(paths) != 1:
        return b""
    return paths[0].read_bytes()


def run_case(case: dict, binary: Path) -> dict:
    run_root = Path(tempfile.mkdtemp(prefix=f"native-timed-edge-295-{case['id']}-", dir=OUTPUT))
    dirs = {name: run_root / name for name in ("home", "config", "tmp", "cwd")}
    for path in dirs.values():
        path.mkdir()
    requests: list[dict] = []
    responses: list[dict] = []
    response_event_trace: list[dict] = []
    process_start_ns = [0]
    event_lock = threading.Lock()
    error_body_name = "http-404-error.raw.json" if case["mode"] == "http404-fallback" else "http-500-error.raw.json"
    error_body = (ROOT / error_body_name).read_bytes()
    success_body = json.dumps({
        "id": f"msg_native_{case['id']}_nonstream",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{"type": "text", "text": case["success_text"]}],
        "stop_reason": "end_turn",
        "stop_sequence": None,
        "usage": {"input_tokens": 10, "output_tokens": 5,
                  "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
    }, ensure_ascii=True, separators=(",", ":")).encode("ascii")
    sse_parts, sse_schedule = sse_bytes(case) if case["mode"] == "http500-retry" else ([], [])

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def _record_request(self) -> tuple[int, bytes, dict, int]:
            received_ns = time.monotonic_ns()
            body = self.rfile.read(int(self.headers.get("content-length", "0")))
            try:
                parsed = json.loads(body)
            except (UnicodeDecodeError, json.JSONDecodeError):
                parsed = {}
            sequence = len(requests) + 1
            path = f"model-request-{sequence:02}.raw.json"
            (run_root / path).write_bytes(body)
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            record = {
                "sequence": sequence, "accepted_loopback_dummy_key": accepted,
                "method": "POST", "path": self.path, "stream": parsed.get("stream"),
                "model": parsed.get("model"), "request_bytes": len(body),
                "request_sha256": digest(body), "request_path": path,
                "headers_in_order": list(self.headers.raw_items()),
                "request_received_since_native_spawn_ms": round((received_ns - process_start_ns[0]) / 1_000_000, 3),
            }
            requests.append(record)
            return sequence, body, record, received_ns

        def do_POST(self) -> None:
            sequence, _body, record, request_received_ns = self._record_request()
            if not record["accepted_loopback_dummy_key"]:
                response = b"{}"
                raw = send_fixed_response(self, 404, response, "application/json")
                responses.append({"sequence": sequence, "status": 404, "kind": "unexpected",
                                  "bytes": len(raw), "sha256": digest(raw)})
                return

            if case["mode"] == "http404-fallback":
                if sequence == 1:
                    delay = case["first_response_delay_ms"]
                    time.sleep(delay / 1000)
                    raw = send_fixed_response(self, 404, error_body, "application/json")
                    response_path = f"http-response-{sequence:02}.raw.json"
                    (run_root / response_path).write_bytes(raw)
                    responses.append({"sequence": sequence, "status": 404, "kind": "error", "delay_before_response_ms": delay,
                                      "path": response_path, "bytes": len(raw), "sha256": digest(raw),
                                      "headers": [["Content-Type", "application/json"]]})
                    return
                delay = case["fallback_response_delay_ms"]
                time.sleep(delay / 1000)
                raw = send_fixed_response(self, 200, success_body, "application/json")
                response_path = f"http-response-{sequence:02}.raw.json"
                (run_root / response_path).write_bytes(raw)
                responses.append({"sequence": sequence, "status": 200, "kind": "nonstream_anthropic_message",
                                  "delay_before_response_ms": delay, "path": response_path,
                                  "bytes": len(raw), "sha256": digest(raw),
                                  "headers": [["Content-Type", "application/json"]]})
                return

            if sequence == 1:
                delay = case["first_response_delay_ms"]
                time.sleep(delay / 1000)
                raw = send_fixed_response(self, 500, error_body, "application/json",
                                          [("x-should-retry", "true"), ("retry-after", "0")])
                response_path = f"http-response-{sequence:02}.raw.json"
                (run_root / response_path).write_bytes(raw)
                responses.append({"sequence": sequence, "status": 500, "kind": "retryable_error",
                                  "delay_before_response_ms": delay, "path": response_path,
                                  "bytes": len(raw), "sha256": digest(raw),
                                  "headers": [["Content-Type", "application/json"],
                                              ["x-should-retry", "true"], ["retry-after", "0"]]})
                return

            before_headers = case["retry_response_headers_delay_ms"]
            time.sleep(before_headers / 1000)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "keep-alive")
            self.end_headers()
            headers_ns = time.monotonic_ns()
            response_path = f"http-response-{sequence:02}.raw.sse"
            raw_parts = []
            for index, (event_name, chunk) in enumerate(sse_parts, start=1):
                schedule = sse_schedule[index - 1]
                time.sleep(schedule["delay_before_ms"] / 1000)
                self.wfile.write(f"{len(chunk):X}\r\n".encode("ascii"))
                self.wfile.write(chunk)
                self.wfile.write(b"\r\n")
                self.wfile.flush()
                raw_parts.append(chunk)
                written_ns = time.monotonic_ns()
                response_event_trace.append({
                    "request_sequence": sequence, "index": index, "event": event_name,
                    "delay_before_ms": schedule["delay_before_ms"],
                    "write_since_headers_ms": round((written_ns - headers_ns) / 1_000_000, 3),
                    "write_since_request_received_ms": round((written_ns - request_received_ns) / 1_000_000, 3),
                    "write_since_native_spawn_ms": round((written_ns - process_start_ns[0]) / 1_000_000, 3),
                    "chunk_bytes": len(chunk), "chunk_sha256": digest(chunk),
                })
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            raw = b"".join(raw_parts)
            (run_root / response_path).write_bytes(raw)
            responses.append({"sequence": sequence, "status": 200, "kind": "streaming_anthropic_sse",
                              "delay_before_headers_ms": before_headers, "path": response_path,
                              "bytes": len(raw), "sha256": digest(raw),
                              "headers": [["Content-Type", "text/event-stream"],
                                          ["Transfer-Encoding", "chunked"], ["Connection", "keep-alive"]]})

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = {
        "PATH": "/usr/bin:/bin", "TMPDIR": str(dirs["tmp"]), "LANG": "C.UTF-8", "TZ": "UTC",
        "TERM": "dumb", "HOME": str(dirs["home"]), "CLAUDE_CONFIG_DIR": str(dirs["config"]),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": str(case["max_retries"]),
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1", "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1",
    }
    argv = [str(binary), "--bare", "--print", "--output-format", "json", "--tools", "",
            "--permission-mode", "dontAsk", "--permission-prompts", "none", "--max-turns", "1",
            "--max-budget-usd", "1", "--model", FIXTURE["model"], "--session-id", case["session_id"],
            "--system-prompt", FIXTURE["system_prompt"], case["prompt"]]
    process_start_ns[0] = time.monotonic_ns()
    started = time.monotonic()
    timed_out = False
    try:
        try:
            completed = subprocess.run(argv, cwd=dirs["cwd"], env=env, capture_output=True,
                                       timeout=FIXTURE["timeout_seconds"], check=False)
            stdout, stderr, exit_code = completed.stdout, completed.stderr, completed.returncode
        except subprocess.TimeoutExpired as error:
            stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    (run_root / "native.stdout.raw.json").write_bytes(stdout)
    (run_root / "native.stderr.raw").write_bytes(stderr)
    try:
        parsed = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        parsed = None
    timing_names = ("time_to_request_ms", "ttft_stream_ms", "first_content_frame_ms", "ttft_ms",
                    "first_output_token_ms", "duration_api_ms", "duration_ms")
    timing_fields = ({key: parsed[key] for key in timing_names if key in parsed}
                     if isinstance(parsed, dict) else {})
    receipt = {
        "native_version": binary_version_text,
        "native_sha256": digest(binary.read_bytes()),
        "runner_sha256": digest(Path(__file__).read_bytes()),
        "fixture_sha256": digest(FIXTURE_PATH.read_bytes()),
        "native_path": str(binary), "case": case, "argv": argv,
        "public_entrypoint": "Native 2.1.295 --bare --print --output-format json against loopback Anthropic stub",
        "network_boundary": {"model_endpoint": "127.0.0.1 loopback only", "dummy_key_only": True,
                             "isolated_home_config": True, "upstream_requests": 0},
        "retry_budget": case["max_retries"], "timeout_seconds": FIXTURE["timeout_seconds"],
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "native_exit_code": exit_code, "timed_out": timed_out,
        "native_stdout": {"path": "native.stdout.raw.json", "bytes": len(stdout), "sha256": digest(stdout)},
        "native_stdout_top_level_key_order": list(parsed.keys()) if isinstance(parsed, dict) else None,
        "native_result": ({key: parsed.get(key) for key in ("type", "subtype", "is_error", "result",
                                                               "num_turns", "stop_reason", "session_id")}
                          if isinstance(parsed, dict) else None),
        "native_timing_fields_ms": timing_fields,
        "native_stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": digest(stderr)},
        "requests": requests, "responses": responses, "response_event_trace": response_event_trace,
        "success_nonstream_body": {"path": "planned-nonstream-message.raw.json",
                                    "bytes": len(success_body), "sha256": digest(success_body)}
                                   if case["mode"] == "http404-fallback" else None,
        "input_error_body": {"path": error_body_name, "bytes": len(error_body), "sha256": digest(error_body)},
        "sse_schedule": sse_schedule,
        "scope": "This receipt records one bounded 404 nonstream-fallback case or one 500 retry-then-SSE case; it does not claim timing behavior for other status/error families.",
    }
    if case["mode"] == "http404-fallback":
        (run_root / "planned-nonstream-message.raw.json").write_bytes(success_body)
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"case": case["id"], "exit_code": exit_code, "timed_out": timed_out,
                      "request_stream_flags": [item["stream"] for item in requests],
                      "request_offsets_ms": [item["request_received_since_native_spawn_ms"] for item in requests],
                      "timing_fields_ms": timing_fields, "result": receipt["native_result"],
                      "receipt": str((run_root / "receipt.json").relative_to(FIXTURE_ROOT))},
                     ensure_ascii=True, indent=2), flush=True)
    shutil.rmtree(dirs["cwd"], ignore_errors=True)
    return 1 if timed_out or exit_code != 0 or not requests else 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--case", choices=sorted(CASE_BY_ID))
    parser.add_argument("--binary", default=os.environ.get(
        "CLAUDE_NATIVE_BINARY", str(DEFAULT_BINARY)))
    args = parser.parse_args()
    binary = Path(args.binary).expanduser().resolve()
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    global binary_version_text
    binary_version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or binary_version_text != "2.1.295 (Claude Code)":
        raise SystemExit(f"Native version mismatch: {binary_version_text!r}")
    if digest(binary.read_bytes()) != FIXTURE["native_sha256"]:
        raise SystemExit(f"Native SHA mismatch: {digest(binary.read_bytes())}")
    selected = [CASE_BY_ID[args.case]] if args.case else FIXTURE["cases"]
    return 1 if any(run_case(case, binary) != 0 for case in selected) else 0


if __name__ == "__main__":
    raise SystemExit(main())
