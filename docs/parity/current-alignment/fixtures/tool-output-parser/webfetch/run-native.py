#!/usr/bin/env python3
"""Capture Native 2.1.293 WebFetch output parsing via the public tool.call hook."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
RUN_NAME = os.environ.get("NATIVE_WEBFETCH_RUN_NAME", "webfetch-nested")
if not RUN_NAME or any(not (char.isalnum() or char in "-_") for char in RUN_NAME):
    raise SystemExit("NATIVE_WEBFETCH_RUN_NAME must contain only letters, digits, '-' or '_'")
OUTPUT = ROOT.parent / "run-output" / RUN_NAME
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
DUMMY_KEY = "native-webfetch-output-fixture-dummy-key"
MARKER = b"NATIVE_WEBFETCH_OUTPUT="
SELECTED_CASE = os.environ.get("NATIVE_WEBFETCH_CASE")
SELECTED_CASES = os.environ.get("NATIVE_WEBFETCH_CASES")
if SELECTED_CASE and SELECTED_CASES:
    raise SystemExit("Set only one of NATIVE_WEBFETCH_CASE or NATIVE_WEBFETCH_CASES")
CASE_IDS = (
    [SELECTED_CASE] if SELECTED_CASE else
    [item for item in SELECTED_CASES.split(",") if item] if SELECTED_CASES else
    FIXTURE["cases"]
)
if any(case_id not in FIXTURE["cases"] for case_id in CASE_IDS):
    raise SystemExit(f"unknown NATIVE_WEBFETCH_CASE: {SELECTED_CASE}")


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def skip_ws(text: str, position: int) -> int:
    while position < len(text) and text[position] in " \t\r\n":
        position += 1
    return position


def field_span(text: str, object_start: int, wanted: str) -> tuple[object, int, int]:
    decoder = json.JSONDecoder()
    position = skip_ws(text, object_start)
    if text[position] != "{":
        raise ValueError("expected JSON object")
    position += 1
    while True:
        position = skip_ws(text, position)
        if text[position] == "}":
            raise KeyError(wanted)
        key, position = decoder.raw_decode(text, position)
        position = skip_ws(text, position)
        if text[position] != ":":
            raise ValueError("object member is missing colon")
        value_start = skip_ws(text, position + 1)
        value, value_end = decoder.raw_decode(text, value_start)
        if key == wanted:
            return value, value_start, value_end
        position = skip_ws(text, value_end)
        if text[position] == ",":
            position += 1
        elif text[position] != "}":
            raise ValueError("object member has invalid separator")


def array_spans(text: str, array_start: int):
    decoder = json.JSONDecoder()
    position = skip_ws(text, array_start)
    if text[position] != "[":
        raise ValueError("expected JSON array")
    position += 1
    while True:
        position = skip_ws(text, position)
        if text[position] == "]":
            return
        value_start = position
        value, value_end = decoder.raw_decode(text, value_start)
        yield value, value_start, value_end
        position = skip_ws(text, value_end)
        if text[position] == ",":
            position += 1
        elif text[position] != "]":
            raise ValueError("array item has invalid separator")


def raw_top_member(raw: bytes, wanted: str) -> bytes:
    text = raw.decode("utf-8")
    _value, start, end = field_span(text, 0, wanted)
    return text[start:end].encode("utf-8")


def raw_webfetch_catalog_item(raw: bytes) -> bytes | None:
    text = raw.decode("utf-8")
    _tools, tools_start, _tools_end = field_span(text, 0, "tools")
    for tool, start, end in array_spans(text, tools_start):
        if isinstance(tool, dict) and tool.get("name") == "WebFetch":
            return text[start:end].encode("utf-8")
    return None


def raw_model_tool_result(raw: bytes, tool_use_id: str) -> bytes | None:
    text = raw.decode("utf-8")
    _messages, messages_start, _messages_end = field_span(text, 0, "messages")
    for message, message_start, _message_end in array_spans(text, messages_start):
        if not isinstance(message, dict):
            continue
        try:
            content, content_start, _content_end = field_span(text, message_start, "content")
        except KeyError:
            continue
        if not isinstance(content, list):
            continue
        for block, block_start, _block_end in array_spans(text, content_start):
            if (isinstance(block, dict) and block.get("type") == "tool_result"
                    and block.get("tool_use_id") == tool_use_id):
                _value, start, end = field_span(text, block_start, "content")
                return text[start:end].encode("utf-8")
    return None


def sse(events: list[tuple[str, dict]]) -> bytes:
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


def model_response(case_id: str, request_index: int, resource_port: int) -> bytes:
    if request_index == 1:
        url = f"http://127.0.0.1:{resource_port}/fixture/{case_id}"
        input_value = {"url": url, "prompt": FIXTURE["tool_input_prompt"]}
        events = [
            ("message_start", {"type": "message_start", "message": {
                "id": "msg_native_webfetch_nested_call", "type": "message",
                "role": "assistant", "model": "claude-sonnet-4-5-20250929",
                "content": [], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 5, "output_tokens": 0},
            }}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": FIXTURE["tool_use_id"],
                    "name": "WebFetch", "input": {}}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta",
                    "partial_json": json.dumps(input_value, separators=(",", ":"))}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                "usage": {"output_tokens": 1}}),
            ("message_stop", {"type": "message_stop"}),
        ]
    else:
        events = [
            ("message_start", {"type": "message_start", "message": {
                "id": "msg_native_webfetch_nested_done", "type": "message",
                "role": "assistant", "model": "claude-sonnet-4-5-20250929",
                "content": [], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 5, "output_tokens": 0},
            }}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": "fixed webfetch parser response"}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "fixed webfetch parser response"}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                "usage": {"output_tokens": 2}}),
            ("message_stop", {"type": "message_stop"}),
        ]
    return sse(events)


binary_text = os.environ.get("CLAUDE_NATIVE_BINARY")
if not binary_text:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
binary = Path(binary_text).expanduser().resolve()
binary_sha = sha256(binary.read_bytes())
if binary_sha != FIXTURE["native_sha256"]:
    raise SystemExit(f"Native SHA mismatch: {binary_sha}")
version = subprocess.run([str(binary), "--version"], capture_output=True, check=False)
version_text = (version.stdout + version.stderr).decode("utf-8", errors="replace").strip()
if FIXTURE["native_version"] not in version_text:
    raise SystemExit(f"Native version mismatch: {version_text!r}")

if OUTPUT.exists():
    shutil.rmtree(OUTPUT)
OUTPUT.mkdir(parents=True)
resource_requests: list[dict] = []


class ResourceHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_HEAD(self) -> None:
        accepted = self.client_address[0] == "127.0.0.1" and self.path.startswith("/fixture/")
        resource_requests.append({"method": "HEAD", "path": self.path, "accepted": accepted})
        self.send_response(200 if accepted else 404)
        self.send_header("Content-Length", str(len(b"fixed loopback WebFetch document\n")))
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.end_headers()

    def do_GET(self) -> None:
        accepted = self.client_address[0] == "127.0.0.1" and self.path.startswith("/fixture/")
        body = b"fixed loopback WebFetch document\n" if accepted else b"not found\n"
        resource_requests.append({"method": "GET", "path": self.path, "accepted": accepted,
                                 "bytes": len(body), "sha256": sha256(body)})
        self.send_response(200 if accepted else 404)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.end_headers()
        self.wfile.write(body)


resource_server = HTTPServer(("127.0.0.1", FIXTURE["resource_port"]), ResourceHandler)
resource_thread = threading.Thread(target=resource_server.serve_forever, daemon=True)
resource_thread.start()

case_results: list[dict] = []
try:
    for case_id in CASE_IDS:
        case_index = FIXTURE["cases"].index(case_id) + 1
        resource_start = len(resource_requests)
        case_root = OUTPUT / case_id
        run_root = case_root / "run"
        config = run_root / "config"
        cwd = run_root / "cwd"
        temp = run_root / "tmp"
        for path in (config, cwd, temp, run_root / "home"):
            path.mkdir(parents=True, exist_ok=True)
        plugin = case_root / "plugin"
        shutil.copytree(ROOT / "plugin", plugin)
        hook_path = plugin / "hooks" / "register.js"
        tool_url = f"http://127.0.0.1:{FIXTURE['resource_port']}/fixture/{case_id}"
        request_records: list[dict] = []
        response_records: list[dict] = []

        class ModelHandler(BaseHTTPRequestHandler):
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
                request_index = len(request_records) + 1
                record = {"index": request_index, "path": self.path,
                          "accepted": accepted,
                          "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                          "bytes": len(raw), "sha256": sha256(raw)}
                request_records.append(record)
                if accepted:
                    request_path = case_root / f"request-{request_index:02}.raw.json"
                    request_path.write_bytes(raw)
                    record["raw_file"] = request_path.name
                    try:
                        request_obj = json.loads(raw)
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        request_obj = {}
                    tool_names = [item.get("name") for item in request_obj.get("tools", [])
                                  if isinstance(item, dict)]
                    record["tools"] = tool_names
                    body = model_response(case_id, request_index, FIXTURE["resource_port"])
                    response_path = case_root / f"response-{request_index:02}.raw.sse"
                    response_path.write_bytes(body)
                    response_records.append({"path": response_path.name,
                                             "bytes": len(body), "sha256": sha256(body)})
                    status = 200
                else:
                    body, status = b"{}", 404
                self.send_response(status)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Content-Type", "text/event-stream" if accepted else "application/json")
                self.end_headers()
                self.wfile.write(body)

        model_server = HTTPServer(("127.0.0.1", 0), ModelHandler)
        model_thread = threading.Thread(target=model_server.serve_forever, daemon=True)
        model_thread.start()
        session_id = f"00000000-0000-4000-8000-{0x2f30 + case_index:012d}"
        url = tool_url
        prompt = f"{FIXTURE['prompt']} {url}"
        env = {
            "PATH": "/usr/bin:/bin",
            "TMPDIR": str(temp),
            "LANG": "C.UTF-8",
            "TZ": "UTC",
            "TERM": "dumb",
            "HOME": str(run_root / "home"),
            "CLAUDE_CONFIG_DIR": str(config),
            "ANTHROPIC_API_KEY": DUMMY_KEY,
            "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{model_server.server_port}",
            "ANTHROPIC_MAX_RETRIES": "0",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_TELEMETRY": "1",
            "DO_NOT_TRACK": "1",
        }
        argv = [
            str(binary), "--plugin-dir", str(plugin), "--setting-sources", "project",
            "--strict-mcp-config", "--print", "--tools", "default,WebFetch",
            "--dangerously-skip-permissions", "--debug", "hooks", "--max-turns", "2",
            "--model", FIXTURE["model"], "--session-id", session_id, prompt,
        ]
        try:
            completed = subprocess.run(argv, cwd=cwd, env=env, capture_output=True,
                                       timeout=60, check=False)
            stdout, stderr, exit_code, timed_out = (
                completed.stdout, completed.stderr, completed.returncode, False
            )
        except subprocess.TimeoutExpired as error:
            stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
        finally:
            model_server.shutdown()
            model_server.server_close()
            model_thread.join(timeout=2)

        (case_root / "native.stdout.raw").write_bytes(stdout)
        (case_root / "native.stderr.raw").write_bytes(stderr)
        first_request_raw = (case_root / "request-01.raw.json").read_bytes() if len(request_records) >= 1 else b""
        second_request_raw = (case_root / "request-02.raw.json").read_bytes() if len(request_records) >= 2 else b""
        catalog_item = raw_webfetch_catalog_item(first_request_raw) if first_request_raw else None
        if catalog_item is not None:
            (case_root / "tool-catalog-webfetch.raw.json").write_bytes(catalog_item)
        model_result_raw = raw_model_tool_result(second_request_raw, FIXTURE["tool_use_id"])
        if model_result_raw is not None:
            (case_root / "model-tool-result-content.raw.json").write_bytes(model_result_raw)

        session_files = list(config.rglob(f"{session_id}.jsonl"))
        session_rows: list[bytes] = []
        result_values: list[bytes] = []
        if session_files:
            for line in session_files[0].read_bytes().splitlines(keepends=True):
                try:
                    row = json.loads(line)
                except (UnicodeDecodeError, json.JSONDecodeError):
                    continue
                if row.get("toolUseResult") is not None:
                    session_rows.append(line)
                    result_values.append(raw_top_member(line.rstrip(b"\r\n"), "toolUseResult"))
        if session_rows:
            (case_root / "tool-use-result-session-rows.raw.jsonl").write_bytes(b"".join(session_rows))
        if result_values:
            (case_root / "tool-use-result.raw.json").write_bytes(result_values[0])

        marker_lines = [line for data in (stdout, stderr) for line in data.splitlines()
                        if MARKER in line]
        for debug_path in (config / "debug").glob("*.txt"):
            marker_lines.extend(line for line in debug_path.read_bytes().splitlines()
                                if MARKER in line)
        if marker_lines:
            (case_root / "hook-marker.raw.txt").write_bytes(b"\n".join(marker_lines) + b"\n")

        case_results.append({
            "case": case_id,
            "session_id": session_id,
            "fixed_tool_url": url,
            "argv": ["$CLAUDE_NATIVE_BINARY", "--plugin-dir", "<fixture-plugin>",
                     "--setting-sources", "project", "--strict-mcp-config", "--print",
                     "--tools", "default,WebFetch", "--dangerously-skip-permissions",
                     "--debug", "hooks", "--max-turns", "2", "--model", FIXTURE["model"],
                     "--session-id", session_id, prompt],
            "timed_out": timed_out,
            "exit_code": exit_code,
            "requests": request_records,
            "responses": response_records,
            "webfetch_catalog_item": {"path": "tool-catalog-webfetch.raw.json",
                                      "bytes": len(catalog_item), "sha256": sha256(catalog_item)}
                                     if catalog_item is not None else None,
            "hook_marker_lines": [line.decode("utf-8", errors="backslashreplace")
                                  for line in marker_lines],
            "model_tool_result_content_raw": {"path": "model-tool-result-content.raw.json",
                                               "bytes": len(model_result_raw),
                                               "sha256": sha256(model_result_raw)}
                                              if model_result_raw is not None else None,
            "tool_use_result_session_rows": {"path": "tool-use-result-session-rows.raw.jsonl",
                                              "rows": len(session_rows),
                                              "sha256": sha256(b"".join(session_rows))}
                                             if session_rows else None,
            "tool_use_result_raw": {"path": "tool-use-result.raw.json",
                                    "bytes": len(result_values[0]),
                                    "sha256": sha256(result_values[0])}
                                   if result_values else None,
            "hook_sha256": sha256(hook_path.read_bytes()),
            "native_stdout": {"path": "native.stdout.raw", "bytes": len(stdout),
                              "sha256": sha256(stdout)},
            "native_stderr": {"path": "native.stderr.raw", "bytes": len(stderr),
                              "sha256": sha256(stderr)},
            "resource_requests": resource_requests[resource_start:],
            "notes": "tool.call awaits Native next(event), then returns a fixed parser input; only the fixed loopback resource and model stubs are configured",
        })
finally:
    resource_server.shutdown()
    resource_server.server_close()
    resource_thread.join(timeout=2)

receipt = {
    "native_version": version_text,
    "native_sha256": binary_sha,
    "runner_sha256": sha256(Path(__file__).read_bytes()),
    "hook_sha256": sha256((ROOT / "plugin" / "hooks" / "register.js").read_bytes()),
    "fixed_resource": {"host": "127.0.0.1", "port": FIXTURE["resource_port"],
                       "body": "fixed loopback WebFetch document\\n"},
    "dummy_model_key_only": True,
    "no_external_model_or_webfetch_destination": True,
    "selected_cases": CASE_IDS,
    "cases": case_results,
    "scope": "Native 2.1.293 public WebFetch tool.call continuation-wrapper output parser observations; not a candidate Harness pass",
}
(OUTPUT / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n",
                                            encoding="utf-8")
print(json.dumps(receipt, ensure_ascii=True, indent=2))
if any(case["timed_out"] or case["exit_code"] != 0 or not case["hook_marker_lines"]
       for case in case_results):
    raise SystemExit(1)
