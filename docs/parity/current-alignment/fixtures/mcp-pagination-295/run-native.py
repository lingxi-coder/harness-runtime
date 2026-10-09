#!/usr/bin/env python3
"""Run one bounded Native 2.1.295 MCP repeated-cursor probe."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
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
EXPECTED_SHA256 = FIXTURE["native_sha256"]
DUMMY_KEY = "native-mcp-pagination-295-dummy-key"
TOOL_USE_ID = "toolu_native_mcp_pagination_295"
OUTPUT = ROOT / "run-output"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def json_span_member(text: str, key: str) -> tuple[int, int]:
    decoder = json.JSONDecoder()
    position = 0
    while position < len(text) and text[position].isspace():
        position += 1
    if text[position] != "{":
        raise ValueError("expected JSON object")
    position += 1
    while True:
        while position < len(text) and text[position].isspace():
            position += 1
        if text[position] == "}":
            break
        actual_key, key_end = decoder.raw_decode(text, position)
        position = key_end
        while text[position].isspace():
            position += 1
        if text[position] != ":":
            raise ValueError("expected JSON colon")
        position += 1
        while text[position].isspace():
            position += 1
        value_start = position
        _value, value_end = decoder.raw_decode(text, value_start)
        if actual_key == key:
            return value_start, value_end
        position = value_end
        while text[position].isspace():
            position += 1
        if text[position] == ",":
            position += 1
        elif text[position] != "}":
            raise ValueError("expected JSON comma or object end")
    raise KeyError(key)


def json_span_array_item(text: str, start: int, target_index: int) -> tuple[int, int]:
    decoder = json.JSONDecoder()
    position = start
    while text[position].isspace():
        position += 1
    if text[position] != "[":
        raise ValueError("expected JSON array")
    position += 1
    index = 0
    while True:
        while text[position].isspace():
            position += 1
        if text[position] == "]":
            break
        value_start = position
        _value, value_end = decoder.raw_decode(text, value_start)
        if index == target_index:
            return value_start, value_end
        index += 1
        position = value_end
        while text[position].isspace():
            position += 1
        if text[position] == ",":
            position += 1
        elif text[position] != "]":
            raise ValueError("expected JSON comma or array end")
    raise IndexError(target_index)


def json_span_path(text: str, path: list[str | int]) -> tuple[int, int]:
    start = 0
    end = len(text)
    for part in path:
        if isinstance(part, str):
            relative_start, relative_end = json_span_member(text[start:end], part)
            start, end = start + relative_start, start + relative_end
        else:
            start, end = json_span_array_item(text, start, part)
    return start, end


def sse_text(text: str, message_id: str) -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": text}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


def sse_tool(name: str, page: int) -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_native_mcp_pagination_tool", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "tool_use", "id": TOOL_USE_ID, "name": name, "input": {}}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "input_json_delta", "partial_json": json.dumps({"page": page}, separators=(",", ":"))}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                            "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {event}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for event, value in events
    ).encode("utf-8")


def message_tool(name: str, page: int) -> bytes:
    return json.dumps({
        "id": "msg_native_mcp_pagination_tool", "type": "message", "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{"type": "tool_use", "id": TOOL_USE_ID, "name": name, "input": {"page": page}}],
        "stop_reason": "tool_use", "stop_sequence": None,
        "usage": {"input_tokens": 10, "output_tokens": 5},
    }, ensure_ascii=True, separators=(",", ":")).encode()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--discovery-mode", choices=("fallback", "modern"), default="fallback",
        help="fallback returns -32601; modern returns a complete 2026-07-28 discovery result",
    )
    options = parser.parse_args()
    discovery_mode = options.discovery_mode
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

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix=f"native-mcp-pagination-295-{discovery_mode}-", dir=OUTPUT))
    for dirname in ("home", "config", "tmp", "mcp-trace"):
        (run_root / dirname).mkdir()
    external_cwd = Path(tempfile.mkdtemp(prefix="native-mcp-pagination-295-cwd-", dir="/private/tmp"))
    mcp_config = {
        "mcpServers": {
            FIXTURE["server_name"]: {
                "command": sys.executable,
                "args": [str(SERVER_PATH)],
                "env": {
                    "MCP_PAGINATION_TRACE_DIR": str(run_root / "mcp-trace"),
                    "MCP_PAGINATION_CASES_PATH": str(FIXTURE_PATH),
                    "MCP_PAGINATION_DISCOVERY_MODE": discovery_mode,
                },
            }
        }
    }
    mcp_config_path = run_root / "mcp-config.json"
    mcp_config_path.write_text(json.dumps(mcp_config, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")

    records: list[dict] = []
    response_index = [0]
    model_response_count = [0]
    selected_tool: list[tuple[str, int] | None] = [None]

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def reply(self, status: int, body: bytes, content_type: str) -> None:
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Content-Type", content_type)
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self) -> None:
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
            }
            records.append(record)
            if not accepted:
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401, b"{}", "application/json")
                return

            model_response_count[0] += 1
            request_path = run_root / f"model-request-{model_response_count[0]:02}.raw.json"
            request_path.write_bytes(body)
            record["raw_request_path"] = str(request_path.relative_to(ROOT))
            try:
                request_text = body.decode("utf-8")
                request = json.loads(request_text)
            except (UnicodeDecodeError, json.JSONDecodeError):
                self.reply(400, b"{}", "application/json")
                return
            record["model"] = request.get("model")
            record["stream"] = request.get("stream")
            if model_response_count[0] == 1:
                tools = request.get("tools", [])
                try:
                    tools_start, tools_end = json_span_member(request_text, "tools")
                    (run_root / "model-tools.raw.json").write_bytes(body[tools_start:tools_end])
                except KeyError:
                    pass
                mcp_tools = [
                    t for t in tools if isinstance(t, dict)
                    and isinstance(t.get("name"), str)
                    and "page_tool_" in t["name"]
                ]
                record["mcp_tools"] = [t["name"] for t in mcp_tools]
                page_tools = []
                for tool in mcp_tools:
                    match = re.search(r"page_tool_(\d+)", tool["name"])
                    if match:
                        page_tools.append((int(match.group(1)), tool["name"]))
                if page_tools:
                    page, name = max(page_tools)
                    selected_tool[0] = (name, page)
                    record["selected_tool"] = name
                    record["selected_page"] = page
                    response = sse_tool(name, page) if request.get("stream") is True else message_tool(name, page)
                    content_type = "text/event-stream" if request.get("stream") is True else "application/json"
                    self.reply(200, response, content_type)
                    (run_root / "model-response-01.raw").write_bytes(response)
                    return
                record["mcp_tool_missing_from_model_catalog"] = True
                final = sse_text(FIXTURE["final_text"], "msg_native_mcp_pagination_no_tool")
                self.reply(200, final, "text/event-stream")
                (run_root / "model-response-01.raw.sse").write_bytes(final)
                return

            result_block = None
            result_path = None
            for message_index, message in enumerate(request.get("messages", [])):
                content = message.get("content")
                if isinstance(content, list):
                    for block_index, block in enumerate(content):
                        if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("tool_use_id") == TOOL_USE_ID:
                            result_block = block
                            result_path = ["messages", message_index, "content", block_index]
            record["fixture_tool_result_present"] = result_block is not None
            if result_block is not None and result_path is not None:
                request_text = body.decode("utf-8")
                result_start, result_end = json_span_path(request_text, result_path)
                result_bytes = body[result_start:result_end]
                result_artifact_path = run_root / "model-tool-result.raw.json"
                result_artifact_path.write_bytes(result_bytes)
                record["tool_result_path"] = str(result_artifact_path.relative_to(ROOT))
                record["tool_result_sha256"] = sha256(result_bytes)
            final = sse_text(FIXTURE["final_text"], "msg_native_mcp_pagination_final")
            self.reply(200, final, "text/event-stream")
            (run_root / "model-response-02.raw.sse").write_bytes(final)

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
    args = [
        "--setting-sources", "project", "--strict-mcp-config", "--mcp-config", str(mcp_config_path),
        "--print", "--output-format", "json", "--tools", "default", "--max-turns", "2",
        "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"], "--debug", "mcp",
        "--dangerously-skip-permissions", "--allow-dangerously-skip-permissions", FIXTURE["prompt"],
    ]
    started = time.monotonic()
    try:
        completed = subprocess.run([str(binary), *args], cwd=external_cwd, env=env,
                                   capture_output=True, timeout=FIXTURE["timeout_seconds"], check=False)
        stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
    except subprocess.TimeoutExpired as error:
        stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=2)

    (run_root / "native.stdout.raw.json").write_bytes(stdout)
    (run_root / "native.stderr.raw").write_bytes(stderr)
    try:
        stdout_json = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        stdout_json = None

    trace = run_root / "mcp-trace"
    mcp_in = (trace / "mcp-stdio-in.raw.jsonl").read_bytes() if (trace / "mcp-stdio-in.raw.jsonl").exists() else b""
    mcp_out = (trace / "mcp-stdio-out.raw.jsonl").read_bytes() if (trace / "mcp-stdio-out.raw.jsonl").exists() else b""
    debug_logs = list((run_root / "home").rglob("mcp-logs-*/*.jsonl"))
    debug_log_bytes = debug_logs[0].read_bytes() if len(debug_logs) == 1 else b""
    if debug_log_bytes:
        (run_root / "native-mcp-debug.raw.jsonl").write_bytes(debug_log_bytes)
    incoming = []
    for line in mcp_in.splitlines():
        try:
            incoming.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    outgoing = []
    for line in mcp_out.splitlines():
        try:
            outgoing.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    responses_by_id = {item.get("id"): item for item in outgoing if isinstance(item, dict) and "id" in item}
    list_observations = []
    for index, message in enumerate(incoming, 1):
        method = message.get("method")
        if method not in ("tools/list", "prompts/list", "resources/list"):
            continue
        response = responses_by_id.get(message.get("id"), {})
        result = response.get("result", {}) if isinstance(response, dict) else {}
        key = {"tools/list": "tools", "prompts/list": "prompts", "resources/list": "resources"}[method]
        entries = result.get(key, []) if isinstance(result, dict) else []
        list_observations.append({
            "method": method, "id": message.get("id"), "params": message.get("params", {}),
            "cursor": (message.get("params") or {}).get("cursor") if isinstance(message.get("params"), dict) else None,
            "response_error": response.get("error") if isinstance(response, dict) else None,
            "entry_names": [entry.get("name", entry.get("uri")) for entry in entries if isinstance(entry, dict)],
            "next_cursor": result.get("nextCursor") if isinstance(result, dict) else None,
        })
    calls = [m for m in incoming if m.get("method") == "tools/call"]
    list_counts = {method: sum(item.get("method") == method for item in incoming)
                   for method in ("tools/list", "prompts/list", "resources/list")}
    trace_summary = {
        "incoming": {"bytes": len(mcp_in), "sha256": sha256(mcp_in), "path": "mcp-trace/mcp-stdio-in.raw.jsonl"},
        "outgoing": {"bytes": len(mcp_out), "sha256": sha256(mcp_out), "path": "mcp-trace/mcp-stdio-out.raw.jsonl"},
        "methods": [m.get("method") for m in incoming],
        "list_requests_by_method": list_counts,
        "list_observations": list_observations,
        "tools_call_requests": [{"params": m.get("params"), "id": m.get("id")} for m in calls],
    }
    result_summary = None
    if isinstance(stdout_json, dict):
        result_summary = {key: stdout_json.get(key) for key in ("type", "subtype", "is_error", "result", "num_turns", "stop_reason", "session_id")}
    receipt = {
        "native_version": version_text,
        "native_sha256": sha256(binary_bytes),
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "stdio_server_sha256": sha256(SERVER_PATH.read_bytes()),
        "fixture_sha256": sha256(FIXTURE_PATH.read_bytes()),
        "native_path": str(binary),
        "public_entrypoint": "Native 2.1.295 --print --output-format json, isolated --mcp-config, stdio MCP, local Messages stub, --tools default",
        "discovery_mode_fixture": discovery_mode,
        "fixed_inputs": {key: FIXTURE[key] for key in ("session_id", "model", "prompt", "server_name", "repeated_cursor", "cursor_page_responses", "error_after_repeated_cursor_requests")},
        "network_boundary": {"model_endpoint": "127.0.0.1 loopback only", "mcp_transport": "local stdio subprocess only", "dummy_api_key_only": True, "isolated_home_config": True, "working_directory": str(external_cwd), "no_upstream_relay": True},
        "timeout_seconds": FIXTURE["timeout_seconds"],
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "stdout": {"path": "native.stdout.raw.json", "bytes": len(stdout), "sha256": sha256(stdout)},
        "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
        "mcp_debug_log": ({"path": "native-mcp-debug.raw.jsonl", "bytes": len(debug_log_bytes),
                           "sha256": sha256(debug_log_bytes)} if debug_log_bytes else None),
        "native_result": result_summary,
        "anthropic_messages_requests": records,
        "mcp": trace_summary,
        "selected_model_tool": selected_tool[0][0] if selected_tool[0] else None,
        "selected_model_tool_page": selected_tool[0][1] if selected_tool[0] else None,
        "native_tool_call_observed": len(calls) == 1,
        "artifacts": sorted(p.name for p in run_root.iterdir() if p.is_file()),
        "raw_outputs_relative_path": str(run_root.relative_to(ROOT)),
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    try:
        shutil.rmtree(external_cwd)
    except OSError:
        pass
    print(json.dumps({
        "native_version": version_text, "native_exit_code": exit_code, "timed_out": timed_out,
        "native_result": result_summary, "list_requests_by_method": list_counts,
        "list_observations": list_observations,
        "tools_call_requests": trace_summary["tools_call_requests"],
        "selected_model_tool": receipt["selected_model_tool"],
        "anthropic_messages_request_count": len(records),
        "discovery_mode_fixture": discovery_mode,
        "receipt": str((run_root / "receipt.json").relative_to(ROOT)),
    }, ensure_ascii=False, indent=2))
    return 1 if timed_out else 0


if __name__ == "__main__":
    raise SystemExit(main())
