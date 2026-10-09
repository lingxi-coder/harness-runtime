#!/usr/bin/env python3
"""Run a bounded Native 2.1.295 MCP resource-blob/MIME/Read probe."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
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
DUMMY_KEY = "native-mcp-mime-295-dummy-key"
OUTPUT = ROOT / "run-output"
OUTPUT.mkdir(parents=True, exist_ok=True)

EXTENSIONS = {
    "image/x-icon": "ico",
    "image/vnd.microsoft.icon": "ico",
    "text/css": "css",
    "text/javascript": "js",
    "application/javascript": "js",
    "application/xml": "xml",
    "text/xml": "xml",
    "application/manifest+json": "webmanifest",
    "font/woff2": "woff2",
    "application/font-woff2": "woff2",
    "font/woff": "woff",
    "application/font-woff": "woff",
    "font/ttf": "ttf",
    "font/otf": "otf",
    "application/wasm": "wasm",
    "application/x-native-fixture": "bin",
}


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
        while text[position].isspace():
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


def raw_slice_from_text_offsets(body: bytes, text: str, start: int, end: int) -> bytes:
    """Map JSON source character offsets back to exact UTF-8 request bytes."""
    byte_start = len(text[:start].encode("utf-8"))
    byte_end = len(text[:end].encode("utf-8"))
    return body[byte_start:byte_end]


def sse_tool(name: str, tool_use_id: str, input_value: dict, message_id: str) -> bytes:
    encoded = json.dumps(input_value, ensure_ascii=True, separators=(",", ":"))
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None, "usage": {"input_tokens": 20, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "tool_use", "id": tool_use_id, "name": name, "input": {}}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "input_json_delta", "partial_json": encoded}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                            "usage": {"output_tokens": 4}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {event}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for event, value in events
    ).encode("utf-8")


def sse_text(text: str, message_id: str) -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None, "usage": {"input_tokens": 20, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": text}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 3}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {event}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for event, value in events
    ).encode("utf-8")


def find_tool_result(request: dict, tool_use_id: str) -> tuple[dict | None, list[str | int] | None]:
    for message_index, message in enumerate(request.get("messages", [])):
        content = message.get("content")
        if not isinstance(content, list):
            continue
        for block_index, block in enumerate(content):
            if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("tool_use_id") == tool_use_id:
                return block, ["messages", message_index, "content", block_index]
    return None, None


def tool_result_text(block: dict) -> list[str]:
    content = block.get("content")
    if isinstance(content, str):
        return [content]
    if not isinstance(content, list):
        return []
    return [
        item["text"] for item in content
        if isinstance(item, dict) and item.get("type") == "text" and isinstance(item.get("text"), str)
    ]


def path_from_result(text: str, extension: str) -> str | None:
    candidates = re.findall(r"/(?:[^\s\"'<>]+)", text)
    for candidate in candidates:
        candidate = candidate.rstrip(".,;:)]}")
        if PurePosixPath(candidate).suffix.lower() == f".{extension}":
            return candidate
    return None


def extract_resource_paths(block: dict) -> dict[str, str]:
    found: dict[str, str] = {}
    texts = tool_result_text(block)
    for item in FIXTURE["resources"]:
        extension = EXTENSIONS[item["mime"]]
        for text in texts:
            marker_start = text.find(item["uri"])
            if marker_start < 0:
                continue
            marker_end = marker_start + len(item["uri"])
            later_markers = [
                start for other in FIXTURE["resources"]
                if other["id"] != item["id"]
                if (start := text.find(other["uri"], marker_end)) >= 0
            ]
            segment_end = min(later_markers) if later_markers else len(text)
            path = path_from_result(text[marker_start:segment_end], extension)
            if path is not None:
                found[item["id"]] = path
                break
    return found


def read_tool_name(tools: list) -> tuple[str | None, dict | None]:
    for tool in tools:
        if isinstance(tool, dict) and tool.get("name") == "Read":
            return "Read", tool.get("input_schema", tool.get("inputSchema"))
    return None, None


def read_result_text(block: dict) -> str:
    return "\n".join(tool_result_text(block))


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

    run_root = Path(tempfile.mkdtemp(prefix="native-mcp-mime-295-", dir=OUTPUT))
    for dirname in ("home", "config", "tmp", "mcp-trace", "saved-blobs", "model-requests", "model-responses", "tool-results"):
        (run_root / dirname).mkdir()
    external_cwd = Path(tempfile.mkdtemp(prefix="native-mcp-mime-295-cwd-", dir="/private/tmp"))
    mcp_config = {
        "mcpServers": {
            FIXTURE["server_name"]: {
                "command": sys.executable,
                "args": [str(SERVER_PATH)],
                "env": {
                    "MCP_MIME_TRACE_DIR": str(run_root / "mcp-trace"),
                    "MCP_MIME_CASES_PATH": str(FIXTURE_PATH),
                },
            }
        }
    }
    mcp_config_path = run_root / "mcp-config.json"
    mcp_config_path.write_text(json.dumps(mcp_config, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")

    requests: list[dict] = []
    model_responses: list[dict] = []
    tool_result_artifacts: list[dict] = []
    tool_definition_artifacts: list[dict] = []
    mcp_calls: list[dict] = []
    saved_paths: dict[str, str] = {}
    read_tool_calls: list[dict] = []
    read_results: list[dict] = []
    failed_controls: list[str] = []
    finished_response_count = [0]
    mcp_tool_use_seen = [False]
    read_issue_index = [0]

    def capture_read_results(request: dict, request_text: str, body: bytes) -> None:
        recorded_ids = {item["tool_use_id"] for item in read_results}
        for pending in read_tool_calls:
            if pending["tool_use_id"] in recorded_ids:
                continue
            read_block, read_path = find_tool_result(request, pending["tool_use_id"])
            if read_block is None or read_path is None:
                continue
            try:
                result_start, result_end = json_span_path(request_text, read_path)
                result_raw = raw_slice_from_text_offsets(body, request_text, result_start, result_end)
                result_file = run_root / "tool-results" / f"read-{pending['resource_id']}.raw.json"
                result_file.write_bytes(result_raw)
                read_text = read_result_text(read_block)
                resource = next(item for item in FIXTURE["resources"] if item["id"] == pending["resource_id"])
                marker_visible = resource.get("read_marker", "") in read_text
                read_results.append({
                    **pending,
                    "result_path": str(result_file.relative_to(ROOT)),
                    "result_bytes": len(result_raw),
                    "result_sha256": sha256(result_raw),
                    "result_text": read_text,
                    "fixture_text_marker_visible": marker_visible,
                })
            except (ValueError, KeyError, IndexError, StopIteration) as error:
                failed_controls.append(f"Could not capture Native Read result for {pending['resource_id']}: {error}")

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
            request_index = len(requests) + 1
            record = {
                "index": request_index,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "path": self.path,
                "request_bytes": len(body),
                "request_sha256": sha256(body),
                "raw_path": f"model-requests/request-{request_index:02}.raw.json",
            }
            requests.append(record)
            (run_root / record["raw_path"]).write_bytes(body)
            if not accepted:
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401, b"{}", "application/json")
                return

            try:
                request_text = body.decode("utf-8")
                request = json.loads(request_text)
            except (UnicodeDecodeError, json.JSONDecodeError):
                self.reply(400, b"{}", "application/json")
                return
            record["model"] = request.get("model")
            record["stream"] = request.get("stream")
            record["message_count"] = len(request.get("messages", []))
            response: bytes
            response_type: str

            if not mcp_tool_use_seen[0]:
                tools = request.get("tools", [])
                try:
                    tools_start, tools_end = json_span_member(request_text, "tools")
                    (run_root / "model-request-tools.raw.json").write_bytes(
                        raw_slice_from_text_offsets(body, request_text, tools_start, tools_end)
                    )
                except KeyError:
                    tools = []
                matching = [
                    item for item in tools if isinstance(item, dict)
                    and isinstance(item.get("name"), str)
                    and FIXTURE["tool_name"] in item["name"]
                ]
                if len(matching) != 1:
                    record["fixture_tool_advertised"] = False
                    final = sse_text("MCP MIME fixture failed to expose the fixture tool.", "msg_native_mcp_mime_no_tool")
                    response, response_type = final, "text/event-stream"
                else:
                    name = matching[0]["name"]
                    for tool_name, output_name in (("Read", "model-request-read-tool.raw.json"), (name, "model-request-mcp-tool.raw.json")):
                        tool_index = next((index for index, item in enumerate(tools) if isinstance(item, dict) and item.get("name") == tool_name), None)
                        if tool_index is None:
                            continue
                        definition_start, definition_end = json_span_array_item(request_text, tools_start, tool_index)
                        definition_raw = raw_slice_from_text_offsets(body, request_text, definition_start, definition_end)
                        definition_path = run_root / output_name
                        definition_path.write_bytes(definition_raw)
                        tool_definition_artifacts.append({
                            "name": tool_name,
                            "path": str(definition_path.relative_to(ROOT)),
                            "bytes": len(definition_raw),
                            "sha256": sha256(definition_raw),
                        })
                    record["fixture_tool_advertised"] = True
                    record["fixture_tool_name"] = name
                    mcp_tool_use_seen[0] = True
                    response = sse_tool(name, FIXTURE["tool_use_id"], {}, "msg_native_mcp_mime_tool")
                    response_type = "text/event-stream"
            else:
                capture_read_results(request, request_text, body)
                mcp_result, mcp_result_path = find_tool_result(request, FIXTURE["tool_use_id"])
                if mcp_result is not None and mcp_result_path is not None and not tool_result_artifacts:
                    try:
                        result_start, result_end = json_span_path(request_text, mcp_result_path)
                        result_raw = raw_slice_from_text_offsets(body, request_text, result_start, result_end)
                        result_file = run_root / "tool-results" / "mcp-blob-tool-result.raw.json"
                        result_file.write_bytes(result_raw)
                        artifact = {
                            "kind": "mcp-tool-result",
                            "path": str(result_file.relative_to(ROOT)),
                            "bytes": len(result_raw),
                            "sha256": sha256(result_raw),
                        }
                        tool_result_artifacts.append(artifact)
                        (run_root / "tool-results" / "mcp-blob-tool-result.input.raw.json").write_bytes(result_raw)
                        saved_paths.update(extract_resource_paths(mcp_result))
                        record["mcp_tool_result_present"] = True
                        record["mcp_saved_path_count"] = len(saved_paths)
                    except (ValueError, KeyError, IndexError) as error:
                        record["mcp_result_raw_slice_error"] = str(error)
                else:
                    record["mcp_tool_result_present"] = False

                tools = request.get("tools", [])
                read_name, read_schema = read_tool_name(tools if isinstance(tools, list) else [])
                if read_issue_index[0] < len(FIXTURE["read_targets"]):
                    mime = FIXTURE["read_targets"][read_issue_index[0]]
                    resource = next((item for item in FIXTURE["resources"] if item["mime"] == mime), None)
                    if read_name is not None and resource is not None and resource["id"] in saved_paths:
                        tool_use_id = FIXTURE["read_tool_use_prefix"] + resource["id"].replace("-", "_")
                        read_path = saved_paths[resource["id"]]
                        read_tool_calls.append({"tool_use_id": tool_use_id, "mime": mime, "resource_id": resource["id"], "path": read_path})
                        record["read_tool_available"] = True
                        record["read_tool_schema"] = read_schema
                        record["read_path_selected"] = read_path
                        response = sse_tool(
                            read_name,
                            tool_use_id,
                            {"file_path": read_path},
                            f"msg_native_mcp_mime_read_{read_issue_index[0] + 1}",
                        )
                        read_issue_index[0] += 1
                        response_type = "text/event-stream"
                    else:
                        record["read_tool_available"] = read_name is not None
                        record["read_tool_schema"] = read_schema
                        record["read_target_missing_from_tool_result"] = None if resource is None else resource["id"]
                        failed_controls.append(f"Read unavailable or saved path missing for {mime}")
                        response = sse_text("MCP MIME fixture could not complete its Read checks.", "msg_native_mcp_mime_read_unavailable")
                        response_type = "text/event-stream"
                        read_issue_index[0] = len(FIXTURE["read_targets"])
                else:
                    response = sse_text(FIXTURE["final_text"], "msg_native_mcp_mime_final")
                    response_type = "text/event-stream"

            finished_response_count[0] += 1
            response_path = run_root / "model-responses" / f"response-{finished_response_count[0]:02}.raw"
            response_path.write_bytes(response)
            model_responses.append({
                "index": finished_response_count[0],
                "path": str(response_path.relative_to(ROOT)),
                "bytes": len(response),
                "sha256": sha256(response),
                "content_type": response_type,
            })
            self.reply(200, response, response_type)

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
        "--print", "--output-format", "json", "--tools", "default", "--max-turns", "8",
        "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"], "--debug", "mcp",
        "--dangerously-skip-permissions", "--allow-dangerously-skip-permissions", FIXTURE["prompt"],
    ]
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
        server_thread.join(timeout=2)

    (run_root / "native.stdout.raw.json").write_bytes(stdout)
    (run_root / "native.stderr.raw").write_bytes(stderr)
    try:
        stdout_json = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        stdout_json = None

    for item in FIXTURE["resources"]:
        path = saved_paths.get(item["id"])
        saved_record = {"id": item["id"], "mime": item["mime"], "uri": item["uri"],
                        "expected_extension_from_embedded_table": EXTENSIONS[item["mime"]],
                        "actual_path_from_model_tool_result": path}
        if path is None:
            saved_record["saved"] = False
            failed_controls.append(f"No saved path for {item['id']} ({item['mime']})")
        else:
            disk_path = Path(path)
            saved_record["actual_filename"] = disk_path.name
            saved_record["actual_extension"] = disk_path.suffix.lstrip(".").lower()
            saved_record["file_exists"] = disk_path.is_file()
            saved_record["saved"] = disk_path.is_file()
            expected = bytes.fromhex(item["bytes_hex"])
            if disk_path.is_file():
                actual = disk_path.read_bytes()
                saved_record["actual_bytes"] = len(actual)
                saved_record["actual_sha256"] = sha256(actual)
                saved_record["actual_bytes_equal_fixture"] = actual == expected
                saved_record["fixture_bytes"] = len(expected)
                if actual == expected:
                    copy_path = run_root / "saved-blobs" / disk_path.name
                    copy_path.write_bytes(actual)
                    saved_record["curated_raw_path"] = str(copy_path.relative_to(ROOT))
                else:
                    failed_controls.append(f"Saved bytes differ for {item['id']}")
            else:
                failed_controls.append(f"Saved file missing for {item['id']}: {path}")
        saved_record["actual_extension_matches_embedded_table"] = (
            saved_record.get("actual_extension") == saved_record["expected_extension_from_embedded_table"]
        ) if saved_record.get("actual_extension") is not None else False
        saved_record["uri_extension_is_unrelated"] = not item["uri"].endswith("." + saved_record["expected_extension_from_embedded_table"])
        saved_record["uri_suffix"] = item["uri"].rsplit(".", 1)[-1]
        if saved_record["saved"] and not saved_record["actual_extension_matches_embedded_table"]:
            failed_controls.append(f"MIME extension mismatch for {item['id']}")
        saved_paths[item["id"]] = saved_record

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

    receipt = {
        "native_version": version_text,
        "native_sha256": sha256(binary_bytes),
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "stdio_server_sha256": sha256(SERVER_PATH.read_bytes()),
        "fixture_sha256": sha256(FIXTURE_PATH.read_bytes()),
        "native_path": str(binary),
        "public_entrypoint": "Native 2.1.295 --print --output-format json, isolated --mcp-config, local stdio MCP, loopback-only Messages stub, --tools default",
        "fixed_inputs": {key: FIXTURE[key] for key in ("session_id", "model", "prompt", "server_name", "tool_name", "read_targets")},
        "network_boundary": {"model_endpoint": "127.0.0.1 loopback only", "mcp_transport": "local stdio subprocess only", "dummy_api_key_only": True, "isolated_home_config": True, "external_api_requests": 0},
        "timeout_seconds": FIXTURE["timeout_seconds"],
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "native_stdout": {"path": "native.stdout.raw.json", "bytes": len(stdout), "sha256": sha256(stdout)},
        "native_stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
        "native_result": ({key: stdout_json.get(key) for key in ("type", "subtype", "is_error", "result", "num_turns", "stop_reason", "session_id")} if isinstance(stdout_json, dict) else None),
        "messages_requests": requests,
        "model_responses": model_responses,
        "model_tool_definition_artifacts": tool_definition_artifacts,
        "mcp_methods": [item.get("method") for item in incoming],
        "mcp_tools_call_requests": [item for item in incoming if item.get("method") == "tools/call"],
        "mcp_raw": {
            "stdin_path": "mcp-trace/mcp-stdio-in.raw.jsonl", "stdin_bytes": len(mcp_in), "stdin_sha256": sha256(mcp_in),
            "stdout_path": "mcp-trace/mcp-stdio-out.raw.jsonl", "stdout_bytes": len(mcp_out), "stdout_sha256": sha256(mcp_out),
        },
        "mcp_tool_result_artifacts": tool_result_artifacts,
        "saved_blob_observations_by_resource": list(saved_paths.values()),
        "read_calls": read_tool_calls,
        "read_results": read_results,
        "read_call_count": len(read_tool_calls),
        "read_content_marker_pass_count": sum(item["fixture_text_marker_visible"] for item in read_results),
        "failed_checks": failed_controls,
        "raw_outputs_relative_path": str(run_root.relative_to(ROOT)),
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    try:
        shutil.rmtree(external_cwd)
    except OSError:
        pass
    print(json.dumps({
        "native_version": version_text,
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "resource_count": len(saved_paths),
        "saved_paths": [{key: item.get(key) for key in ("id", "mime", "actual_filename", "actual_extension", "actual_bytes_equal_fixture", "actual_extension_matches_embedded_table", "actual_path_from_model_tool_result")} for item in saved_paths.values()],
        "read_call_count": len(read_tool_calls),
        "read_content_marker_pass_count": sum(item["fixture_text_marker_visible"] for item in read_results),
        "native_result": receipt["native_result"],
        "failed_checks": failed_controls,
        "receipt": str((run_root / "receipt.json").relative_to(ROOT)),
    }, ensure_ascii=False, indent=2))
    return 1 if timed_out else 0


if __name__ == "__main__":
    raise SystemExit(main())
