#!/usr/bin/env python3
"""Capture Native 2.1.295 MCP tool_result JSON bytes for omitted/false/true isError."""

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
SERVER_PATH = ROOT / "stdio_server.py"
OUTPUT = ROOT / "run-output"
DUMMY_KEY = "native-tool-result-presence-295-dummy-key"
EXPECTED_SHA = FIXTURE["native_sha256"]
CASES_BY_ID = {item["tool_use_id"]: item for item in FIXTURE["tool_cases"]}


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


def sse_tool(case: dict, index: int, wire_name: str) -> bytes:
    args = {"case": case["name"]}
    return sse([
        ("message_start", {"type": "message_start", "message": {
            "id": f"msg_native_tool_result_presence_{index}", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "tool_use", "id": case["tool_use_id"],
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
    start = 0
    end = len(text)
    for part in path:
        start, end = (member_span(text, start, part) if isinstance(part, str)
                      else element_span(text, start, part))
    return start, end


def utf8_byte_offset(text: str, char_offset: int) -> int:
    return len(text[:char_offset].encode("utf-8"))


def raw_tool_results(body: bytes) -> list[tuple[dict, bytes]]:
    text = body.decode("utf-8")
    request = json.loads(text)
    found = []
    for message_index, message in enumerate(request.get("messages", [])):
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block_index, block in enumerate(content):
            if not isinstance(block, dict) or block.get("type") != "tool_result":
                continue
            case = CASES_BY_ID.get(block.get("tool_use_id"))
            if case is None:
                continue
            start, end = path_span(text, ["messages", message_index, "content", block_index])
            byte_start, byte_end = utf8_byte_offset(text, start), utf8_byte_offset(text, end)
            found.append((case, body[byte_start:byte_end]))
    return found


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

    output = ROOT / "run-output"
    output.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix="native-mcp-tool-result-presence-295-", dir=output))
    for name in ("home", "config", "tmp", "mcp-trace"):
        (run_root / name).mkdir()
    external_cwd = Path(tempfile.mkdtemp(prefix="native-mcp-tool-result-presence-295-cwd-", dir="/private/tmp"))
    config_path = run_root / "mcp-config.json"
    config_path.write_text(json.dumps({"mcpServers": {FIXTURE["server_name"]: {
        "command": sys.executable,
        "args": [str(SERVER_PATH)],
        "env": {"MCP_RESULT_TRACE_DIR": str(run_root / "mcp-trace"),
                "MCP_RESULT_CASES_PATH": str(FIXTURE_PATH)},
    }}}, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    request_rows: list[dict] = []
    response_rows: list[dict] = []
    observations: dict[str, dict] = {}

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def reply(self, body: bytes) -> None:
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            self.wfile.write(body)

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
            request_path = run_root / f"model-request-{index:02}.raw.json"
            request_path.write_bytes(body)
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
            for case, raw_block in raw_tool_results(body):
                name = case["name"]
                destination = run_root / f"model-tool-result-{name}.raw.json"
                if not destination.exists():
                    destination.write_bytes(raw_block)
                    parsed_block = json.loads(raw_block)
                    observations[name] = {
                        "path": str(destination.relative_to(ROOT)),
                        "bytes": len(raw_block), "sha256": sha(raw_block),
                        "key_order": list(parsed_block.keys()),
                        "is_error_present": "is_error" in parsed_block,
                        "is_error_value": parsed_block.get("is_error"),
                        "tool_use_id": parsed_block.get("tool_use_id"),
                        "content": parsed_block.get("content"),
                    }
            if index <= len(FIXTURE["tool_cases"]):
                case = FIXTURE["tool_cases"][index - 1]
                names = [tool.get("name") for tool in request.get("tools", []) if isinstance(tool, dict)]
                wire_name = next((name for name in names if isinstance(name, str) and case["name"] in name), None)
                row["target_tool_name"] = wire_name
                row["target_tool_present"] = wire_name is not None
                if wire_name is None:
                    response = sse_text(f"MCP_TOOL_NOT_IN_MODEL_CATALOG:{case['name']}",
                                        f"msg_native_tool_missing_{case['name']}")
                    response_rows.append({"index": index, "kind": "tool_not_present", "name": case["name"],
                                          "bytes": len(response), "sha256": sha(response)})
                    self.reply(response)
                    (run_root / f"model-response-{index:02}.raw.sse").write_bytes(response)
                    return
                response = sse_tool(case, index, wire_name)
                response_rows.append({"index": index, "kind": "tool_use", "name": wire_name,
                                      "tool_use_id": case["tool_use_id"], "bytes": len(response), "sha256": sha(response)})
            else:
                row["all_expected_tool_results_present"] = all(case["name"] in observations for case in FIXTURE["tool_cases"])
                response = sse_text(FIXTURE["final_text"], "msg_native_tool_result_presence_final")
                response_rows.append({"index": index, "kind": "final", "bytes": len(response), "sha256": sha(response)})
            self.reply(response)
            (run_root / f"model-response-{index:02}.raw.sse").write_bytes(response)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    env = {
        "PATH": "/usr/bin:/bin", "TMPDIR": str(run_root / "tmp"), "LANG": "C.UTF-8", "TZ": "UTC",
        "TERM": "dumb", "HOME": str(run_root / "home"), "CLAUDE_CONFIG_DIR": str(run_root / "config"),
        "ANTHROPIC_API_KEY": DUMMY_KEY, "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1",
    }
    args = ["--setting-sources", "project", "--strict-mcp-config", "--mcp-config", str(config_path),
            "--print", "--output-format", "json", "--tools", "default", "--max-turns", "5",
            "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"], "--debug", "mcp",
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
        server_thread.join(timeout=2)
    (run_root / "native.stdout.raw.json").write_bytes(stdout)
    (run_root / "native.stderr.raw").write_bytes(stderr)
    try:
        parsed_stdout = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        parsed_stdout = None
    session_paths = sorted((run_root / "config" / "projects").rglob(f"{FIXTURE['session_id']}.jsonl"))
    session = session_paths[0].read_bytes() if len(session_paths) == 1 else b""
    if session:
        (run_root / "native-session.raw.jsonl").write_bytes(session)
    trace = run_root / "mcp-trace"
    incoming = (trace / "mcp-stdio-in.raw.jsonl").read_bytes() if (trace / "mcp-stdio-in.raw.jsonl").exists() else b""
    outgoing = (trace / "mcp-stdio-out.raw.jsonl").read_bytes() if (trace / "mcp-stdio-out.raw.jsonl").exists() else b""
    debug_files = list((run_root / "home").rglob("mcp-logs-*/*.jsonl"))
    debug = debug_files[0].read_bytes() if len(debug_files) == 1 else b""
    if debug:
        (run_root / "native-mcp-debug.raw.jsonl").write_bytes(debug)
    result_summary = ({key: parsed_stdout.get(key) for key in (
        "type", "subtype", "is_error", "result", "num_turns", "stop_reason", "session_id",
    )} if isinstance(parsed_stdout, dict) else None)
    receipt = {
        "native_version": version_text, "native_sha256": sha(binary_bytes),
        "runner_sha256": sha(Path(__file__).read_bytes()), "stdio_server_sha256": sha(SERVER_PATH.read_bytes()),
        "fixture_sha256": sha(FIXTURE_PATH.read_bytes()), "native_path": str(binary),
        "public_entrypoint": "Native 2.1.295 --print --output-format json, strict isolated --mcp-config, local stdio MCP, loopback Messages stub",
        "fixed_inputs": FIXTURE,
        "network_boundary": {"model_endpoint": "127.0.0.1 loopback only", "mcp_transport": "local stdio subprocess only",
                             "dummy_api_key_only": True, "isolated_home_config": True, "no_upstream_relay": True},
        "elapsed_seconds": round(time.monotonic() - started, 3), "timeout_seconds": FIXTURE["timeout_seconds"],
        "timed_out": timed_out, "native_exit_code": exit_code, "native_result": result_summary,
        "native_stdout": {"path": "native.stdout.raw.json", "bytes": len(stdout), "sha256": sha(stdout)},
        "native_stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha(stderr)},
        "mcp": {"incoming": {"path": "mcp-trace/mcp-stdio-in.raw.jsonl", "bytes": len(incoming), "sha256": sha(incoming)},
                "outgoing": {"path": "mcp-trace/mcp-stdio-out.raw.jsonl", "bytes": len(outgoing), "sha256": sha(outgoing)},
                "tools_call_count": sum(line.count(b'"method":"tools/call"') for line in incoming.splitlines())},
        "anthropic_messages_requests": request_rows, "anthropic_messages_responses": response_rows,
        "tool_result_observations": observations,
        "native_session": {"path": "native-session.raw.jsonl", "bytes": len(session), "sha256": sha(session)} if session else None,
        "native_mcp_debug": {"path": "native-mcp-debug.raw.jsonl", "bytes": len(debug), "sha256": sha(debug)} if debug else None,
        "raw_outputs_relative_path": str(run_root.relative_to(ROOT)),
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    try:
        shutil.rmtree(external_cwd)
    except OSError:
        pass
    print(json.dumps({"native_version": version_text, "exit_code": exit_code, "timed_out": timed_out,
                      "native_result": result_summary, "tool_result_observations": observations,
                      "mcp_methods": [json.loads(line).get("method") for line in incoming.splitlines()
                                      if line.startswith(b"{" )],
                      "receipt": str((run_root / "receipt.json").relative_to(ROOT))}, ensure_ascii=False, indent=2))
    return 1 if timed_out else 0


if __name__ == "__main__":
    raise SystemExit(main())
