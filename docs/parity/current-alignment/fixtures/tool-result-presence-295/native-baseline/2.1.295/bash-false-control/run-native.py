#!/usr/bin/env python3
"""Capture Native 2.1.295 Bash successful tool_result bytes."""

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
OUTPUT = ROOT / "run-output"
DUMMY_KEY = "native-bash-tool-result-false-295-dummy-key"
EXPECTED_SHA = FIXTURE["native_sha256"]
TOOL_USE_ID = FIXTURE["tool_use_id"]


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sse(events: list[tuple[str, dict]]) -> bytes:
    return "".join(
        f"event: {name}\ndata: {json.dumps(data, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, data in events
    ).encode("utf-8")


def sse_text(text: str, message_id: str) -> bytes:
    return sse([
        ("message_start", {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": text}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ])


def sse_tool(wire_name: str) -> bytes:
    args = {"command": FIXTURE["command"], "description": "Fixed harmless printf control."}
    return sse([
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_native_bash_tool_result_false", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "tool_use", "id": TOOL_USE_ID,
                                                    "name": wire_name, "input": {}}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "input_json_delta",
                                            "partial_json": json.dumps(args, separators=(",", ":"))}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                            "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ])


def skip_space(text: str, position: int) -> int:
    while position < len(text) and text[position] in " \t\r\n":
        position += 1
    return position


def member_span(text: str, start: int, wanted: str) -> tuple[int, int]:
    decoder = json.JSONDecoder()
    position = skip_space(text, start)
    if text[position] != "{":
        raise ValueError("expected object")
    position += 1
    while True:
        position = skip_space(text, position)
        if text[position] == "}":
            break
        key, key_end = decoder.raw_decode(text, position)
        position = skip_space(text, key_end)
        if text[position] != ":":
            raise ValueError("expected colon")
        value_start = skip_space(text, position + 1)
        _value, value_end = decoder.raw_decode(text, value_start)
        if key == wanted:
            return value_start, value_end
        position = skip_space(text, value_end)
        if text[position] == ",":
            position += 1
        elif text[position] != "}":
            raise ValueError("expected comma or object end")
    raise KeyError(wanted)


def element_span(text: str, start: int, wanted_index: int) -> tuple[int, int]:
    decoder = json.JSONDecoder()
    position = skip_space(text, start)
    if text[position] != "[":
        raise ValueError("expected array")
    position += 1
    index = 0
    while True:
        position = skip_space(text, position)
        if text[position] == "]":
            break
        value_start = position
        _value, value_end = decoder.raw_decode(text, value_start)
        if index == wanted_index:
            return value_start, value_end
        index += 1
        position = skip_space(text, value_end)
        if text[position] == ",":
            position += 1
        elif text[position] != "]":
            raise ValueError("expected comma or array end")
    raise IndexError(wanted_index)


def path_span(text: str, path: list[str | int]) -> tuple[int, int]:
    start, end = 0, len(text)
    for part in path:
        start, end = (member_span(text, start, part) if isinstance(part, str)
                      else element_span(text, start, part))
    return start, end


def byte_offset(text: str, char_index: int) -> int:
    return len(text[:char_index].encode("utf-8"))


def find_tool_result(body: bytes) -> bytes | None:
    text = body.decode("utf-8")
    request = json.loads(text)
    for message_index, message in enumerate(request.get("messages", [])):
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block_index, block in enumerate(content):
            if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("tool_use_id") == TOOL_USE_ID:
                start, end = path_span(text, ["messages", message_index, "content", block_index])
                return body[byte_offset(text, start):byte_offset(text, end)]
    return None


def main() -> int:
    binary_value = os.environ.get("CLAUDE_NATIVE_BINARY")
    if not binary_value:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the pinned Native 2.1.295 executable")
    binary = Path(binary_value).expanduser().resolve()
    binary_bytes = binary.read_bytes()
    if sha(binary_bytes) != EXPECTED_SHA:
        raise SystemExit(f"Native SHA mismatch: {sha(binary_bytes)}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or version_text != "2.1.295 (Claude Code)":
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix="native-bash-tool-result-false-295-", dir=OUTPUT))
    for name in ("home", "config", "tmp"):
        (run_root / name).mkdir()
    external_cwd = Path(tempfile.mkdtemp(prefix="native-bash-tool-result-false-295-cwd-", dir="/private/tmp"))
    request_rows: list[dict] = []
    response_rows: list[dict] = []
    observations: list[dict] = []

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def do_POST(self) -> None:
            body = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (self.client_address[0] == "127.0.0.1"
                        and self.path == "/v1/messages?beta=true"
                        and self.headers.get("x-api-key") == DUMMY_KEY)
            row = {"method": "POST", "path": self.path, "accepted": accepted,
                   "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                   "request_bytes": len(body), "request_sha256": sha(body)}
            request_rows.append(row)
            index = len(request_rows)
            (run_root / f"model-request-{index:02}.raw.json").write_bytes(body)
            if not accepted:
                self.send_response(401 if self.path == "/v1/messages?beta=true" else 404)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")
                return
            try:
                request = json.loads(body)
            except (UnicodeDecodeError, json.JSONDecodeError):
                self.send_response(400)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")
                return
            row["model"] = request.get("model")
            row["stream"] = request.get("stream")
            result = find_tool_result(body)
            if result is not None and not observations:
                (run_root / "model-tool-result-bash.raw.json").write_bytes(result)
                parsed = json.loads(result)
                observations.append({"path": "model-tool-result-bash.raw.json", "bytes": len(result),
                                     "sha256": sha(result), "key_order": list(parsed.keys()),
                                     "is_error_present": "is_error" in parsed,
                                     "is_error_value": parsed.get("is_error"),
                                     "tool_use_id": parsed.get("tool_use_id"), "content": parsed.get("content")})
            if index == 1:
                tools = [tool for tool in request.get("tools", []) if isinstance(tool, dict)]
                tool = next((item for item in tools if item.get("name") == FIXTURE["bash_tool_name"]), None)
                row["bash_tool_present"] = tool is not None
                if tool is None:
                    response = sse_text("BASH_TOOL_NOT_IN_MODEL_CATALOG", "msg_native_bash_tool_missing")
                    kind = "tool_not_present"
                else:
                    schema = tool.get("input_schema", tool.get("inputSchema"))
                    row["bash_input_schema"] = schema
                    response = sse_tool(tool["name"])
                    kind = "bash_tool_use"
                self.send_response(200)
                self.send_header("Content-Length", str(len(response)))
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self.wfile.write(response)
                (run_root / "model-response-01.raw.sse").write_bytes(response)
                response_rows.append({"index": 1, "kind": kind, "bytes": len(response), "sha256": sha(response)})
                return

            row["tool_result_present"] = result is not None
            response = sse_text(FIXTURE["final_text"], "msg_native_bash_tool_result_false_final")
            self.send_response(200)
            self.send_header("Content-Length", str(len(response)))
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(response)
            (run_root / f"model-response-{index:02}.raw.sse").write_bytes(response)
            response_rows.append({"index": index, "kind": "final", "bytes": len(response), "sha256": sha(response)})

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = {"PATH": "/usr/bin:/bin", "TMPDIR": str(run_root / "tmp"), "LANG": "C.UTF-8", "TZ": "UTC",
           "TERM": "dumb", "HOME": str(run_root / "home"), "CLAUDE_CONFIG_DIR": str(run_root / "config"),
           "ANTHROPIC_API_KEY": DUMMY_KEY, "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
           "ANTHROPIC_MAX_RETRIES": "0", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
           "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1"}
    args = ["--print", "--output-format", "json", "--tools", "default", "--max-turns", "2",
            "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"],
            "--dangerously-skip-permissions", "--allow-dangerously-skip-permissions", FIXTURE["prompt"]]
    started = time.monotonic()
    timed_out = False
    try:
        completed = subprocess.run([str(binary), *args], cwd=external_cwd, env=env,
                                   capture_output=True, timeout=FIXTURE["timeout_seconds"], check=False)
        stdout, stderr, exit_code = completed.stdout, completed.stderr, completed.returncode
    except subprocess.TimeoutExpired as error:
        timed_out = True
        stdout, stderr, exit_code = error.stdout or b"", error.stderr or b"", None
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
    session_paths = sorted((run_root / "config" / "projects").rglob(f"{FIXTURE['session_id']}.jsonl"))
    session = session_paths[0].read_bytes() if len(session_paths) == 1 else b""
    if session:
        (run_root / "native-session.raw.jsonl").write_bytes(session)
    result_summary = ({key: parsed.get(key) for key in (
        "type", "subtype", "is_error", "result", "num_turns", "stop_reason", "session_id",
    )} if isinstance(parsed, dict) else None)
    receipt = {
        "native_version": version_text, "native_sha256": sha(binary_bytes),
        "runner_sha256": sha(Path(__file__).read_bytes()), "fixture_sha256": sha(FIXTURE_PATH.read_bytes()),
        "native_path": str(binary), "public_entrypoint": "Native 2.1.295 --print --output-format json, local Bash builtin, loopback Messages stub",
        "fixed_inputs": FIXTURE,
        "network_boundary": {"model_endpoint": "127.0.0.1 loopback only", "dummy_api_key_only": True,
                             "isolated_home_config": True, "working_directory": "fresh empty /private/tmp directory",
                             "command": FIXTURE["command"], "external_requests": 0},
        "elapsed_seconds": round(time.monotonic() - started, 3), "timed_out": timed_out,
        "native_exit_code": exit_code, "native_result": result_summary,
        "anthropic_messages_requests": request_rows, "anthropic_messages_responses": response_rows,
        "tool_result_observation": observations[0] if observations else None,
        "native_session": {"path": "native-session.raw.jsonl", "bytes": len(session), "sha256": sha(session)} if session else None,
        "native_stdout": {"path": "native.stdout.raw.json", "bytes": len(stdout), "sha256": sha(stdout)},
        "native_stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha(stderr)},
        "raw_outputs_relative_path": str(run_root.relative_to(ROOT)),
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    try:
        shutil.rmtree(external_cwd)
    except OSError:
        pass
    print(json.dumps({"native_version": version_text, "exit_code": exit_code, "timed_out": timed_out,
                      "native_result": result_summary, "tool_result_observation": receipt["tool_result_observation"],
                      "receipt": str((run_root / "receipt.json").relative_to(ROOT))}, ensure_ascii=False, indent=2))
    return 1 if timed_out else 0


if __name__ == "__main__":
    raise SystemExit(main())
