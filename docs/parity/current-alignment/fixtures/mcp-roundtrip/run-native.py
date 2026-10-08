#!/usr/bin/env python3
"""Native 2.1.293 public CLI → stdio MCP → model tool_result loopback fixture."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
BINARY_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
CWD_VALUE = os.environ.get("CLAUDE_MCP_FIXTURE_CWD")
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-mcp-roundtrip-fixture-dummy-key"
TOOL_USE_ID = "toolu_native_mcp_fixture_1"
FINAL_TEXT = FIXTURE["model_final_text"]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utf16_units(text: str) -> list[int]:
    raw = text.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index:index + 2], "little") for index in range(0, len(raw), 2)]


def sse_message(text: str, message_id: str) -> bytes:
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
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(body, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, body in events
    ).encode("utf-8")


def sse_tool_use(tool_name: str) -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_native_mcp_fixture_tool_use", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "tool_use", "id": TOOL_USE_ID, "name": tool_name, "input": {}}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "input_json_delta", "partial_json": json.dumps(FIXTURE["mcp_tool_input"], ensure_ascii=True, separators=(",", ":"))}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                            "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(body, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, body in events
    ).encode("utf-8")


def tool_use_message(tool_name: str) -> dict:
    return {
        "id": "msg_native_mcp_fixture_tool_use",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5-20250929",
        "content": [{
            "type": "tool_use",
            "id": TOOL_USE_ID,
            "name": tool_name,
            "input": FIXTURE["mcp_tool_input"],
        }],
        "stop_reason": "tool_use",
        "stop_sequence": None,
        "usage": {"input_tokens": 10, "output_tokens": 5},
    }


def skip_space(text: str, position: int) -> int:
    while position < len(text) and text[position] in " \t\r\n":
        position += 1
    return position


def member_span(text: str, start: int, wanted: str) -> tuple[int, int]:
    decoder = json.JSONDecoder()
    position = skip_space(text, start)
    if text[position] != "{":
        raise ValueError("expected JSON object")
    position += 1
    while True:
        position = skip_space(text, position)
        if text[position] == "}":
            break
        key, key_end = decoder.raw_decode(text, position)
        position = skip_space(text, key_end)
        if text[position] != ":":
            raise ValueError("expected JSON member colon")
        value_start = skip_space(text, position + 1)
        _value, value_end = decoder.raw_decode(text, value_start)
        if key == wanted:
            return value_start, value_end
        position = skip_space(text, value_end)
        if text[position] == ",":
            position += 1
        elif text[position] != "}":
            raise ValueError("invalid JSON member separator")
    raise KeyError(wanted)


def element_span(text: str, start: int, wanted_index: int) -> tuple[int, int]:
    decoder = json.JSONDecoder()
    position = skip_space(text, start)
    if text[position] != "[":
        raise ValueError("expected JSON array")
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
            raise ValueError("invalid JSON array separator")
    raise IndexError(wanted_index)


def path_span(text: str, path: list[str | int]) -> tuple[int, int]:
    start = 0
    end = len(text)
    for part in path:
        start, end = (
            member_span(text, start, part)
            if isinstance(part, str)
            else element_span(text, start, part)
        )
    return start, end


def find_tool_result(request_text: str) -> tuple[dict | None, bytes | None]:
    request = json.loads(request_text)
    selected: list[tuple[dict, list[str | int]]] = []
    for message_index, message in enumerate(request.get("messages", [])):
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block_index, block in enumerate(content):
            if (
                isinstance(block, dict)
                and block.get("type") == "tool_result"
                and block.get("tool_use_id") == TOOL_USE_ID
            ):
                selected.append((block, ["messages", message_index, "content", block_index]))
    if len(selected) != 1:
        return None, None
    block, path = selected[0]
    start, end = path_span(request_text, path)
    return block, request_text[start:end].encode("utf-8")


def tool_result_texts(block: dict | None) -> list[str]:
    if block is None:
        return []
    content = block.get("content")
    if isinstance(content, str):
        return [content]
    if isinstance(content, list):
        return [
            item["text"]
            for item in content
            if isinstance(item, dict) and item.get("type") == "text" and isinstance(item.get("text"), str)
        ]
    return []


def main() -> None:
    if not BINARY_VALUE:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the pinned Native executable")
    if not CWD_VALUE:
        raise SystemExit("Set CLAUDE_MCP_FIXTURE_CWD to a fixed empty directory outside this repository")
    binary = Path(BINARY_VALUE).expanduser().resolve()
    binary_sha = sha256(binary.read_bytes())
    if binary_sha != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {binary_sha}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or not version_text.startswith("2.1.293 "):
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    repo_root = ROOT.parents[4]
    cwd = Path(CWD_VALUE).expanduser().resolve()
    try:
        cwd.relative_to(repo_root.parents[2])
    except ValueError:
        pass
    else:
        raise SystemExit("CLAUDE_MCP_FIXTURE_CWD must be outside the LingXi workspace")
    cwd.mkdir(parents=True, exist_ok=True)
    if any(cwd.iterdir()):
        raise SystemExit(f"Refusing non-empty MCP fixture CWD: {cwd}")

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix="mcp-roundtrip-", dir=OUTPUT))
    for name in ("home", "config", "tmp", "mcp-trace"):
        (run_root / name).mkdir()
    mcp_config_path = run_root / "mcp-config.json"
    mcp_config = {
        "mcpServers": {
            FIXTURE["mcp_server_name"]: {
                "command": sys.executable,
                "args": [str(ROOT / "stdio_server.py")],
                "env": {
                    "MCP_FIXTURE_TRACE_DIR": str(run_root / "mcp-trace"),
                    "MCP_FIXTURE_CASES_PATH": str(ROOT / "cases.json"),
                },
            }
        }
    }
    mcp_config_path.write_text(json.dumps(mcp_config, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    records: list[dict] = []
    active_phase = ["initial"]
    discovered_tool_names: list[str] = []
    first_tools_raw_path: Path | None = None
    model_tool_result_path: Path | None = None
    model_tool_result: dict | None = None

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.0"

        def log_message(self, _format: str, *_args: object) -> None:
            return

        def reply(self, status: int, body: bytes = b"", content_type: str = "application/json") -> None:
            self.send_response(status)
            self.send_header("Content-Length", str(len(body)))
            if body:
                self.send_header("Content-Type", content_type)
            self.end_headers()
            if body:
                self.wfile.write(body)

        def do_HEAD(self) -> None:
            accepted = self.client_address[0] == "127.0.0.1" and self.path == "/api/hello"
            records.append({"method": "HEAD", "path": self.path, "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            nonlocal first_tools_raw_path, model_tool_result_path, model_tool_result
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            index = sum(record.get("method") == "POST" for record in records) + 1
            record = {
                "method": "POST",
                "phase": active_phase[0],
                "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
            }
            if not accepted:
                records.append(record)
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401)
                return

            request_path = run_root / f"request-{index}.full.raw.json"
            request_path.write_bytes(raw)
            record["raw_request_path"] = str(request_path.relative_to(ROOT))
            records.append(record)
            try:
                request = json.loads(raw)
            except json.JSONDecodeError:
                self.reply(400, b"{}")
                return
            record["model"] = request.get("model") if isinstance(request, dict) else None
            record["stream"] = request.get("stream") if isinstance(request, dict) else None
            if index == 1:
                tools = request.get("tools", [])
                discovered_tool_names.extend(
                    tool.get("name", "")
                    for tool in tools
                    if isinstance(tool, dict) and isinstance(tool.get("name"), str)
                )
                first_tools_raw_path = run_root / "first-request-tools.raw.json"
                tools_span = path_span(raw.decode("utf-8"), ["tools"])
                tools_text = raw.decode("utf-8")[tools_span[0]:tools_span[1]].encode("utf-8")
                first_tools_raw_path.write_bytes(tools_text)
                matched = [
                    tool for tool in tools
                    if isinstance(tool, dict)
                    and ("emit_payload" in tool.get("name", "") or "lone UTF-16" in tool.get("description", ""))
                ]
                record["mcp_tool_candidates"] = [tool.get("name") for tool in matched]
                if len(matched) == 1:
                    tool_name = matched[0]["name"]
                    if request.get("stream") is True:
                        body = sse_tool_use(tool_name)
                        self.reply(200, body, "text/event-stream")
                        (run_root / "model-response-1.raw.sse").write_bytes(body)
                    else:
                        body = json.dumps(tool_use_message(tool_name), ensure_ascii=True, separators=(",", ":")).encode("utf-8")
                        self.reply(200, body)
                        (run_root / "model-response-1.raw.json").write_bytes(body)
                    active_phase[0] = "after-mcp-tool"
                    return
                record["mcp_tool_discovery_failed"] = True
                final = {
                    "id": "msg_native_mcp_fixture_no_tool",
                    "type": "message", "role": "assistant", "model": "claude-sonnet-4-5-20250929",
                    "content": [{"type": "text", "text": "MCP tool discovery failed."}],
                    "stop_reason": "end_turn", "stop_sequence": None,
                    "usage": {"input_tokens": 10, "output_tokens": 4},
                }
                body = json.dumps(final, ensure_ascii=True, separators=(",", ":")).encode("utf-8")
                self.reply(200, body)
                (run_root / "model-response-1.raw.json").write_bytes(body)
                return

            tool_result, tool_result_raw = find_tool_result(raw.decode("utf-8"))
            if tool_result is not None and tool_result_raw is not None:
                model_tool_result = tool_result
                model_tool_result_path = run_root / "model-tool-result.raw.json"
                model_tool_result_path.write_bytes(tool_result_raw)
                record["tool_result_block_bytes"] = len(tool_result_raw)
                record["tool_result_block_sha256"] = sha256(tool_result_raw)
            final = {
                "id": f"msg_native_mcp_fixture_final_{index}",
                "type": "message", "role": "assistant", "model": "claude-sonnet-4-5-20250929",
                "content": [{"type": "text", "text": FINAL_TEXT}],
                "stop_reason": "end_turn", "stop_sequence": None,
                "usage": {"input_tokens": 15, "output_tokens": 5},
            }
            body = json.dumps(final, ensure_ascii=True, separators=(",", ":")).encode("utf-8")
            if request.get("stream") is True:
                body = sse_message(FINAL_TEXT, f"msg_native_mcp_fixture_final_{index}")
                self.reply(200, body, "text/event-stream")
                (run_root / f"model-response-{index}.raw.sse").write_bytes(body)
            else:
                self.reply(200, body)
                (run_root / f"model-response-{index}.raw.json").write_bytes(body)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    env = {
        "PATH": "/usr/bin:/bin",
        "TMPDIR": str(run_root / "tmp"),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "HOME": str(run_root / "home"),
        "CLAUDE_CONFIG_DIR": str(run_root / "config"),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    args = [
        "--setting-sources", "project", "--strict-mcp-config", "--mcp-config", str(mcp_config_path),
        "--print", "--output-format", "json", "--tools", "", "--max-turns", "2",
        "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"],
        "--debug", "mcp", "--dangerously-skip-permissions", "--allow-dangerously-skip-permissions",
        FIXTURE["prompt"],
    ]
    started = time.monotonic()
    try:
        completed = subprocess.run(
            [str(binary), *args], cwd=cwd, env=env, capture_output=True, timeout=90, check=False
        )
        stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
    except subprocess.TimeoutExpired as error:
        stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=2)

    (run_root / "native.stdout.raw").write_bytes(stdout)
    (run_root / "native.stderr.raw").write_bytes(stderr)
    stdout_json = None
    try:
        stdout_json = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        pass

    session_files = list((run_root / "config").glob(f"projects/*/{FIXTURE['session_id']}.jsonl"))
    if len(session_files) > 1:
        raise SystemExit(f"expected at most one fixed Native session file, found {len(session_files)}")
    session_raw = session_files[0].read_bytes() if session_files else b""
    if session_files:
        (run_root / "session.full.raw.jsonl").write_bytes(session_raw)
    tool_rows = []
    session_rows = []
    for index, line in enumerate(session_raw.splitlines(keepends=True)):
        try:
            row = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        message = row.get("message") if isinstance(row, dict) else None
        message_json = json.dumps(message, ensure_ascii=True, separators=(",", ":")) if message is not None else ""
        contains_tool_id = TOOL_USE_ID in message_json
        session_rows.append({
            "index": index,
            "type": row.get("type"),
            "role": message.get("role") if isinstance(message, dict) else None,
            "contains_fixture_tool_use_id": contains_tool_id,
        })
        if contains_tool_id:
            tool_rows.append(line)
    tool_rows_raw = b"".join(tool_rows)
    if tool_rows_raw:
        (run_root / "tool-related-session-rows.raw.jsonl").write_bytes(tool_rows_raw)

    mcp_trace_dir = run_root / "mcp-trace"
    mcp_in = (mcp_trace_dir / "stdio-in.raw.jsonl").read_bytes() if (mcp_trace_dir / "stdio-in.raw.jsonl").exists() else b""
    mcp_out = (mcp_trace_dir / "stdio-out.raw.jsonl").read_bytes() if (mcp_trace_dir / "stdio-out.raw.jsonl").exists() else b""
    call_request = (mcp_trace_dir / "tools-call-request.raw.jsonrpc").read_bytes() if (mcp_trace_dir / "tools-call-request.raw.jsonrpc").exists() else b""
    call_response = (mcp_trace_dir / "tools-call-response.raw.jsonrpc").read_bytes() if (mcp_trace_dir / "tools-call-response.raw.jsonrpc").exists() else b""
    tools_list_response = (mcp_trace_dir / "tools-list-response.raw.jsonrpc").read_bytes() if (mcp_trace_dir / "tools-list-response.raw.jsonrpc").exists() else b""
    first_tools_raw = (run_root / "first-request-tools.raw.json").read_bytes() if (run_root / "first-request-tools.raw.json").exists() else b""
    mcp_methods = []
    mcp_call_requests = []
    for line in mcp_in.splitlines():
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        mcp_methods.append(message.get("method"))
        if message.get("method") == "tools/call":
            mcp_call_requests.append(message)
    mcp_output_messages = []
    for line in mcp_out.splitlines():
        try:
            mcp_output_messages.append(json.loads(line))
        except json.JSONDecodeError:
            continue
    mcp_error_responses = [
        {"id": message.get("id"), "code": message.get("error", {}).get("code"), "message": message.get("error", {}).get("message")}
        for message in mcp_output_messages
        if isinstance(message, dict) and isinstance(message.get("error"), dict)
    ]
    mcp_result = None
    try:
        decoded_call_response = json.loads(call_response)
        mcp_result = decoded_call_response.get("result")
    except (UnicodeDecodeError, json.JSONDecodeError):
        pass

    result_texts = tool_result_texts(model_tool_result)
    result_units = [utf16_units(text) for text in result_texts]
    expected_source = [
        *utf16_units(FIXTURE["mcp_primary_text_prefix"]),
        *[int(unit) for unit in FIXTURE["mcp_primary_text_units_suffix"]],
    ]
    primary_source_text = FIXTURE["mcp_primary_text_prefix"] + "".join(
        chr(int(unit)) for unit in FIXTURE["mcp_primary_text_units_suffix"]
    )
    expected_replaced = [0xFFFD if unit in (0xD800, 0xDC00) else unit for unit in expected_source]
    primary_content_projection = (
        "source_units" if expected_source in result_units
        else "well_formed_replacement_units" if expected_replaced in result_units
        else "not_present_as_direct_text"
    )
    model_tool_result_first_line = result_texts[0].splitlines()[0] if result_texts else ""
    try:
        structured_from_model_text = json.loads(model_tool_result_first_line)
    except json.JSONDecodeError:
        structured_from_model_text = None
    structured_json_text_matches = (
        isinstance(mcp_result, dict)
        and structured_from_model_text == mcp_result.get("structuredContent")
    )
    structured_label_units = (
        utf16_units(structured_from_model_text.get("label", ""))
        if isinstance(structured_from_model_text, dict) and isinstance(structured_from_model_text.get("label"), str)
        else []
    )
    tool_names = [name for name in discovered_tool_names if name]
    tool_name = None
    if mcp_call_requests:
        tool_name = mcp_call_requests[0].get("params", {}).get("name")
    fixture_tool_advertised = any(
        FIXTURE["mcp_tool_name"] in name or FIXTURE["mcp_server_name"] in name
        for name in tool_names
    )

    checks = {
        "native_process_succeeded": not timed_out and exit_code == 0,
        "mcp_initialize_observed": "initialize" in mcp_methods,
        "mcp_tools_list_response_observed": bool(tools_list_response),
        "mcp_tools_list_discovered_fixture_tool": fixture_tool_advertised,
        "mcp_tools_call_observed_once": len(mcp_call_requests) == 1,
        "native_called_fixture_tool": isinstance(tool_name, str) and FIXTURE["mcp_tool_name"] in tool_name,
        "mcp_tool_call_response_has_multiple_content_blocks": isinstance(mcp_result, dict) and len(mcp_result.get("content", [])) == 2,
        "mcp_tool_call_response_has_structured_content": isinstance(mcp_result, dict) and isinstance(mcp_result.get("structuredContent"), dict),
        "mcp_raw_call_response_contains_lone_unit_escape": b"\\ud800" in call_response.lower(),
        "mcp_structured_content_retains_lone_unit": isinstance(mcp_result, dict) and isinstance(mcp_result.get("structuredContent"), dict) and utf16_units(mcp_result["structuredContent"].get("label", "")) == utf16_units(FIXTURE["structured_content"]["label"]),
        "model_followup_request_contains_tool_result": model_tool_result is not None,
        "model_tool_result_first_line_is_structured_content_json": structured_json_text_matches,
        "model_tool_result_does_not_expose_primary_content_text": all(primary_source_text not in text and FIXTURE["mcp_secondary_text"] not in text for text in result_texts),
        "native_final_text_matches_fixed_response": isinstance(stdout_json, dict) and stdout_json.get("result") == FINAL_TEXT,
        "native_session_captured_tool_related_rows": bool(tool_rows_raw),
        "two_model_requests_observed": sum(record.get("method") == "POST" for record in records) == 2,
        "both_model_requests_use_fixed_model_route": sum(record.get("method") == "POST" for record in records) == 2 and all(record.get("model") == FIXTURE["model"] for record in records if record.get("method") == "POST"),
    }
    receipt = {
        "native_version": version_text,
        "native_sha256": binary_sha,
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "server_sha256": sha256((ROOT / "stdio_server.py").read_bytes()),
        "fixture_sha256": sha256((ROOT / "cases.json").read_bytes()),
        "fixed_inputs": {key: FIXTURE[key] for key in ("session_id", "model", "prompt", "mcp_server_name", "mcp_tool_name", "mcp_tool_input")},
        "mcp_fixed_tool_payload": {
            "primary_text_utf16_units": utf16_units("primary-" + chr(0xD800) + "Ω"),
            "secondary_text_utf16_units": utf16_units(FIXTURE["mcp_secondary_text"]),
            "structured_content_keys": sorted((mcp_result or {}).get("structuredContent", {}).keys()) if isinstance(mcp_result, dict) else [],
            "structured_content_primary_text_utf16_units": utf16_units("structured-" + chr(0xD800) + "Ω"),
        },
        "network_boundary": {
            "anthropic_messages_base_url": "127.0.0.1 loopback only",
            "mcp_transport": "local stdio subprocess only",
            "dummy_api_key_only": True,
            "user_home_and_config_isolated": True,
            "project_cwd_fixed_and_empty": str(cwd),
            "no_upstream_relay": True,
        },
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "stdout": {"path": "native.stdout.raw", "bytes": len(stdout), "sha256": sha256(stdout)},
        "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
        "native_result": {
            key: stdout_json.get(key)
            for key in ("type", "is_error", "terminal_reason", "subtype", "result", "session_id", "num_turns")
        } if isinstance(stdout_json, dict) else None,
        "requests": records,
        "mcp": {
            "methods": mcp_methods,
            "server_discover_probe_error": mcp_error_responses,
            "tool_names_advertised_to_model": tool_names,
            "tool_name_called": tool_name,
            "stdio_in": {"path": "mcp-trace/stdio-in.raw.jsonl", "bytes": len(mcp_in), "sha256": sha256(mcp_in)},
            "stdio_out": {"path": "mcp-trace/stdio-out.raw.jsonl", "bytes": len(mcp_out), "sha256": sha256(mcp_out)},
            "tools_call_request": {"path": "mcp-trace/tools-call-request.raw.jsonrpc", "bytes": len(call_request), "sha256": sha256(call_request)},
            "tools_list_response": {"path": "mcp-trace/tools-list-response.raw.jsonrpc", "bytes": len(tools_list_response), "sha256": sha256(tools_list_response)},
            "tools_call_response": {"path": "mcp-trace/tools-call-response.raw.jsonrpc", "bytes": len(call_response), "sha256": sha256(call_response)},
            "first_request_tools_slice": {"path": "first-request-tools.raw.json", "bytes": len(first_tools_raw), "sha256": sha256(first_tools_raw)},
            "result_content_text_utf16_units": [
                utf16_units(item["text"])
                for item in (mcp_result or {}).get("content", [])
                if isinstance(item, dict) and isinstance(item.get("text"), str)
            ] if isinstance(mcp_result, dict) else [],
            "structured_content": (mcp_result or {}).get("structuredContent") if isinstance(mcp_result, dict) else None,
        },
        "model_tool_result": {
            "path": "model-tool-result.raw.json" if model_tool_result_path else None,
            "bytes": model_tool_result_path.stat().st_size if model_tool_result_path else 0,
            "sha256": sha256(model_tool_result_path.read_bytes()) if model_tool_result_path else sha256(b""),
            "content_kind": type((model_tool_result or {}).get("content")).__name__ if isinstance(model_tool_result, dict) else None,
            "content_text_utf16_units": result_units,
            "primary_content_text_projection": primary_content_projection,
            "first_line_literal_json_text": model_tool_result_first_line,
            "first_line_parsed_structured_content": structured_from_model_text,
            "structured_label_utf16_units": structured_label_units,
            "system_reminder_present": any("<total_tokens>" in text for text in result_texts),
            "is_error": (model_tool_result or {}).get("is_error") if isinstance(model_tool_result, dict) else None,
        },
        "dynamic_fields": [
            "Native-generated JSON-RPC IDs and request sequence numbers",
            "Native-generated session/assistant/message UUIDs and timestamps",
            "system-reminder total_tokens counter appended to tool_result text",
        ],
        "session": {
            "present": bool(session_files),
            "ignored_full_jsonl_path": str((session_files[0] if session_files else run_root / "session.full.raw.jsonl").relative_to(ROOT)),
            "bytes": len(session_raw),
            "sha256": sha256(session_raw),
            "tool_related_rows_path": "tool-related-session-rows.raw.jsonl" if tool_rows_raw else None,
            "tool_related_rows_bytes": len(tool_rows_raw),
            "row_summary": session_rows,
        },
        "checks": checks,
        "scope": "One Native 2.1.293 public --print CLI process; isolated mcp-config launches one local stdio server; loopback-only Anthropic stub returns one tool_use then one final answer. Captures exact stdio JSON-RPC frames, raw model tool_result JSON slice, Native stdout/stderr, and full session JSONL under ignored run-output. No real account, credentials, or upstream MCP service. This is a scoped MCP oracle, not Harness parity.",
    }
    (run_root / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(receipt, ensure_ascii=True, indent=2))
    if not all(checks.values()):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
