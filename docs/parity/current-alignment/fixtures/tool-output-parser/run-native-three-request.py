#!/usr/bin/env python3
"""Capture a fixed three-request Native 2.1.293 Glob tool loop over loopback."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
DUMMY_KEY = "native-builtin-glob-three-request-dummy-key"
RUN_NAME = "glob-three-requests"
TOOL_IDS = [
    "toolu_native_glob_three_request_01",
    "toolu_native_glob_three_request_02",
]
USAGES = [(5, 1), (7, 2), (11, 3)]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sse(events: list[tuple[str, dict]]) -> bytes:
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


def model_response(request_index: int) -> bytes:
    input_tokens, output_tokens = USAGES[request_index - 1]
    message_id = f"msg_native_glob_three_request_{request_index:02}"
    events = [(
        "message_start", {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": input_tokens, "output_tokens": 0},
        }},
    )]
    if request_index <= 2:
        tool_id = TOOL_IDS[request_index - 1]
        events.extend([
            ("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": tool_id,
                    "name": "Glob", "input": {}}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": json.dumps(
                    FIXTURE["tool_input"], separators=(",", ":"))}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                "usage": {"output_tokens": output_tokens}}),
            ("message_stop", {"type": "message_stop"}),
        ])
    else:
        final_text = "three-request-probe-complete"
        events.extend([
            ("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": final_text}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": final_text}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                "usage": {"output_tokens": output_tokens}}),
            ("message_stop", {"type": "message_stop"}),
        ])
    return sse(events)


def _skip_ws(text: str, pos: int) -> int:
    while pos < len(text) and text[pos] in " \t\r\n":
        pos += 1
    return pos


def json_path_range(text: str, path: list[object]) -> tuple[int, int]:
    """Locate a JSON value's source character range without reserializing it."""
    decoder = json.JSONDecoder()
    start = _skip_ws(text, 0)
    value, end = decoder.raw_decode(text, start)
    for component in path:
        if isinstance(component, str):
            if not isinstance(value, dict) or component not in value:
                raise KeyError(path)
            pos = _skip_ws(text, start)
            if text[pos] != "{":
                raise ValueError("expected object while resolving JSON path")
            pos += 1
            while True:
                pos = _skip_ws(text, pos)
                if text[pos] == "}":
                    raise KeyError(path)
                key, pos = decoder.raw_decode(text, pos)
                pos = _skip_ws(text, pos)
                if text[pos] != ":":
                    raise ValueError("JSON object member missing colon")
                member_start = _skip_ws(text, pos + 1)
                member_value, member_end = decoder.raw_decode(text, member_start)
                if key == component:
                    start, end, value = member_start, member_end, member_value
                    break
                pos = _skip_ws(text, member_end)
                if text[pos] == ",":
                    pos += 1
                elif text[pos] == "}":
                    raise KeyError(path)
                else:
                    raise ValueError("invalid JSON object separator")
        elif isinstance(component, int):
            if not isinstance(value, list) or component < 0 or component >= len(value):
                raise IndexError(path)
            pos = _skip_ws(text, start)
            if text[pos] != "[":
                raise ValueError("expected array while resolving JSON path")
            pos += 1
            for index in range(len(value)):
                pos = _skip_ws(text, pos)
                item_start = pos
                item_value, item_end = decoder.raw_decode(text, item_start)
                if index == component:
                    start, end, value = item_start, item_end, item_value
                    break
                pos = _skip_ws(text, item_end)
                if text[pos] == ",":
                    pos += 1
        else:
            raise TypeError(f"unsupported JSON path component {component!r}")
    return start, end


def raw_path_slice(raw: bytes, path: list[object]) -> tuple[bytes, int, int]:
    text = raw.decode("utf-8")
    char_start, char_end = json_path_range(text, path)
    byte_start = len(text[:char_start].encode("utf-8"))
    byte_end = len(text[:char_end].encode("utf-8"))
    return raw[byte_start:byte_end], byte_start, byte_end


def raw_top_level_member(raw: bytes, wanted: str) -> tuple[bytes, int, int]:
    return raw_path_slice(raw, [wanted])


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

OUTPUT.mkdir(parents=True, exist_ok=True)
run_root = OUTPUT / RUN_NAME
if run_root.exists():
    shutil.rmtree(run_root)
run_root.mkdir()
plugin = run_root / "plugin"
shutil.copytree(ROOT / "plugin", plugin)
for name in ("home", "config", "tmp", "cwd"):
    (run_root / name).mkdir()
(run_root / "cwd" / "probe.fixture").write_text(
    "fixed read-only glob fixture\n", encoding="utf-8")
request_records: list[dict] = []
stub_responses: list[dict] = []


class Handler(BaseHTTPRequestHandler):
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
        index = len(request_records) + 1
        record = {"method": "POST", "path": self.path, "accepted": accepted,
                  "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                  "request_bytes": len(raw), "request_sha256": sha256(raw)}
        request_records.append(record)
        if accepted and index <= 3:
            request_path = run_root / f"request-{index:02}.raw.json"
            request_path.write_bytes(raw)
            record["raw_request"] = request_path.name
            body_value = json.loads(raw)
            tool_names = [tool.get("name") for tool in body_value.get("tools", [])
                          if isinstance(tool, dict)]
            record["advertised_glob"] = "Glob" in tool_names
            record["tool_names"] = tool_names
            body, status = model_response(index), 200
            response_path = run_root / f"response-{index:02}.raw.sse"
            response_path.write_bytes(body)
            stub_responses.append({"path": response_path.name, "bytes": len(body),
                                   "sha256": sha256(body), "request_index": index,
                                   "usage": {"input_tokens": USAGES[index - 1][0],
                                             "output_tokens": USAGES[index - 1][1]}})
        else:
            body, status = b"{}", 404
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "text/event-stream" if status == 200
                         else "application/json")
        self.end_headers()
        self.wfile.write(body)


server = HTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
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
argv = [
    str(binary), "--plugin-dir", str(plugin), "--setting-sources", "project",
    "--strict-mcp-config", "--print", "--tools", "default,Glob",
    "--dangerously-skip-permissions", "--debug", "hooks", "--max-turns", "3",
    "--model", FIXTURE["model"], "--session-id",
    "00000000-0000-4000-8000-000000002f11",
    "Find the file matching *.fixture twice and report its path.",
]
try:
    completed = subprocess.run(argv, cwd=run_root / "cwd", env=env,
                               capture_output=True, timeout=45, check=False)
    stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
except subprocess.TimeoutExpired as error:
    stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)

(run_root / "native.stdout.raw").write_bytes(stdout)
(run_root / "native.stderr.raw").write_bytes(stderr)
tool_result_rows: list[bytes] = []
for transcript in sorted((run_root / "config").rglob("*.jsonl")):
    for line in transcript.read_bytes().splitlines(keepends=True):
        try:
            row = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if row.get("toolUseResult") is not None:
            tool_result_rows.append(line)
if tool_result_rows:
    (run_root / "tool-use-result-session-rows.raw.jsonl").write_bytes(
        b"".join(tool_result_rows))
    for index, line in enumerate(tool_result_rows, 1):
        (run_root / f"tool-use-result-row-{index:02}.raw.jsonl").write_bytes(line)
        value, start, end = raw_top_level_member(line.rstrip(b"\r\n"), "toolUseResult")
        (run_root / f"tool-use-result-{index:02}.raw.json").write_bytes(value)

request03_raw = (run_root / "request-03.raw.json").read_bytes() if len(request_records) >= 3 else b""
request03 = json.loads(request03_raw) if request03_raw else {}
request03_tool_results = []
reminders = []
for message_index, message in enumerate(request03.get("messages", [])):
    content = message.get("content")
    if not isinstance(content, list):
        continue
    for block_index, block in enumerate(content):
        if not isinstance(block, dict):
            continue
        block_path = ["messages", message_index, "content", block_index]
        if block.get("type") == "tool_result":
            raw_block, block_start, block_end = raw_path_slice(request03_raw, block_path)
            result_path = run_root / f"request-03-tool-result-{len(request03_tool_results) + 1:02}.raw.json"
            result_path.write_bytes(raw_block)
            item = {"message_index": message_index, "content_index": block_index,
                    "tool_use_id": block.get("tool_use_id"),
                    "is_error": block.get("is_error", False),
                    "raw_path": result_path.name,
                    "raw_byte_range": [block_start, block_end],
                    "raw_bytes": len(raw_block), "raw_sha256": sha256(raw_block)}
            request03_tool_results.append(item)
        if isinstance(block.get("text"), str):
            content_path = block_path + ["text"]
        elif isinstance(block.get("content"), str):
            content_path = block_path + ["content"]
        else:
            continue
        raw_content, content_start, _content_end = raw_path_slice(request03_raw, content_path)
        cursor = 0
        while True:
            tag = raw_content.find(b"<total_tokens>", cursor)
            if tag < 0:
                break
            value_start = tag + len(b"<total_tokens>")
            value_end = raw_content.find(b" tokens left</total_tokens>", value_start)
            if value_end < 0:
                break
            value = raw_content[value_start:value_end].decode("ascii", errors="strict")
            reminders.append({
                "message_index": message_index,
                "content_index": block_index,
                "block_type": block.get("type"),
                "field": content_path[-1],
                "value": value,
                "raw_byte_range": [content_start + tag,
                                   content_start + value_end + len(b" tokens left</total_tokens>")],
            })
            cursor = value_end + 1
checks = {
    "native_three_loopback_requests": len(request_records) == 3
    and all(item.get("accepted") and item.get("dummy_key_matched") for item in request_records),
    "two_native_glob_tool_results_persisted": len(tool_result_rows) == 2,
    "third_request_has_two_typed_tool_result_blocks": len(request03_tool_results) == 2
    and [item["tool_use_id"] for item in request03_tool_results] == TOOL_IDS,
    "third_request_has_at_least_prior_and_current_token_reminders": len(reminders) >= 2,
}
receipt = {
    "native_version": version_text,
    "native_sha256": binary_sha,
    "runner_sha256": sha256(Path(__file__).read_bytes()),
    "hook_sha256": sha256((ROOT / "plugin" / "hooks" / "register.js").read_bytes()),
    "loopback_only": True,
    "dummy_key_only": True,
    "isolated_home_and_config": True,
    "retry_limit": 0,
    "timeout_seconds": 45,
    "argv": ["$CLAUDE_NATIVE_BINARY", "--plugin-dir", "<isolated-plugin>",
             "--setting-sources", "project", "--strict-mcp-config", "--print",
             "--tools", "default,Glob", "--dangerously-skip-permissions", "--debug",
             "hooks", "--max-turns", "3", "--model", FIXTURE["model"],
             "--session-id", "00000000-0000-4000-8000-000000002f11",
             "Find the file matching *.fixture twice and report its path."],
    "fixed_sse_usage": [
        {"request": index, "input_tokens": usage[0], "output_tokens": usage[1],
         "response": "tool_use" if index <= 2 else "end_turn"}
        for index, usage in enumerate(USAGES, 1)
    ],
    "requests": request_records,
    "stub_responses": stub_responses,
    "request03_tool_result_blocks": request03_tool_results,
    "request03_total_tokens_occurrences": reminders,
    "tool_result_jsonl_rows": [
        {
            "path": f"tool-use-result-row-{index:02}.raw.jsonl",
            "bytes": len(line), "sha256": sha256(line),
            "typed_attachment": json.loads(line).get("message", {}).get("content", []),
            "bare_toolUseResult": json.loads(line).get("toolUseResult"),
            "bare_toolUseResult_has_tool_use_id": (
                isinstance(json.loads(line).get("toolUseResult"), dict)
                and "tool_use_id" in json.loads(line).get("toolUseResult", {})),
        }
        for index, line in enumerate(tool_result_rows, 1)
    ],
    "checks": checks,
    "timed_out": timed_out,
    "exit_code": exit_code,
    "stdout": {"path": "native.stdout.raw", "bytes": len(stdout), "sha256": sha256(stdout)},
    "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
    "run_output_dir": str(run_root.relative_to(ROOT)),
    "scope": "Native 2.1.293 public CLI two-Glob-call then final-answer loop; all requests served by fixed loopback SSE, no upstream API",
}
(run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n",
                                       encoding="utf-8")
print(json.dumps(receipt, ensure_ascii=True, indent=2))
if timed_out or exit_code != 0 or not all(checks.values()):
    raise SystemExit(1)
