#!/usr/bin/env python3
"""Probe Native 2.1.293's public tool.call hook for built-in Glob."""

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
DUMMY_KEY = "native-builtin-glob-output-fixture-dummy-key"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def raw_top_level_member(raw: bytes, wanted: str) -> bytes:
    text = raw.decode("utf-8")
    decoder = json.JSONDecoder()
    position = 0
    while position < len(text) and text[position] in " \t\r\n":
        position += 1
    if text[position] != "{":
        raise ValueError("request JSON root is not an object")
    position += 1
    while True:
        while position < len(text) and text[position] in " \t\r\n":
            position += 1
        if text[position] == "}":
            break
        key, key_end = decoder.raw_decode(text, position)
        position = key_end
        while text[position] in " \t\r\n":
            position += 1
        if text[position] != ":":
            raise ValueError("request JSON member is missing colon")
        position += 1
        while text[position] in " \t\r\n":
            position += 1
        value_start = position
        _value, value_end = decoder.raw_decode(text, value_start)
        if key == wanted:
            return text[value_start:value_end].encode("utf-8")
        position = value_end
        while text[position] in " \t\r\n":
            position += 1
        if text[position] == ",":
            position += 1
        elif text[position] != "}":
            raise ValueError("request JSON member has invalid separator")
    raise KeyError(wanted)


def sse(events: list[tuple[str, dict]]) -> bytes:
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


def model_response(tool_call: bool) -> bytes:
    if tool_call:
        events = [
            ("message_start", {"type": "message_start", "message": {
                "id": "msg_native_glob_output_probe_call", "type": "message",
                "role": "assistant", "model": "claude-sonnet-4-5-20250929",
                "content": [], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 5, "output_tokens": 0},
            }}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": FIXTURE["tool_use_id"],
                    "name": "Glob", "input": {}}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": json.dumps(
                    FIXTURE["tool_input"], separators=(",", ":"))}}),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            ("message_delta", {"type": "message_delta",
                "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                "usage": {"output_tokens": 1}}),
            ("message_stop", {"type": "message_stop"}),
        ]
    else:
        events = [
            ("message_start", {"type": "message_start", "message": {
                "id": "msg_native_glob_output_probe_done", "type": "message",
                "role": "assistant", "model": "claude-sonnet-4-5-20250929",
                "content": [], "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 5, "output_tokens": 0},
            }}),
            ("content_block_start", {"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": "probe.fixture"}}),
            ("content_block_delta", {"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": "probe.fixture"}}),
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
case_name = os.environ.get("NATIVE_BUILTIN_CASE", "override")
case_duration = {
    "override": "1",
    "unknown-top": "1",
    "duration-1e400": "1e400",
    "duration-infinity": "Infinity",
    "duration-negative-infinity": "-Infinity",
    "duration-nan": "NaN",
    "duration-wrong-type": "1",
    "duration-missing": "1",
    "filenames-item-wrong-type": "1",
    "known-reversed": "1",
}
if case_name not in case_duration:
    raise SystemExit(f"unknown NATIVE_BUILTIN_CASE: {case_name}")
tools_setting = os.environ.get("CLAUDE_NATIVE_TOOLS", "default,Glob")
run_name = os.environ.get("NATIVE_BUILTIN_RUN_NAME", f"glob-{case_name}")
binary = Path(binary_text).expanduser().resolve()
binary_sha = sha256(binary.read_bytes())
if binary_sha != FIXTURE["native_sha256"]:
    raise SystemExit(f"Native SHA mismatch: {binary_sha}")
version = subprocess.run([str(binary), "--version"], capture_output=True, check=False)
version_text = (version.stdout + version.stderr).decode("utf-8", errors="replace").strip()
if FIXTURE["native_version"] not in version_text:
    raise SystemExit(f"Native version mismatch: {version_text!r}")

OUTPUT.mkdir(parents=True, exist_ok=True)
run_root = OUTPUT / run_name
if run_root.exists():
    shutil.rmtree(run_root)
run_root.mkdir()
plugin = run_root / "plugin"
shutil.copytree(ROOT / "plugin", plugin)
hook_path = plugin / "hooks" / "register.js"
hook_source = hook_path.read_text(encoding="utf-8")
hook_source = hook_source.replace("durationMs: 1,", f"durationMs: {case_duration[case_name]},")
if case_name == "known-reversed":
    hook_source = hook_source.replace(
        '''const result = {
      durationMs: 1,
      numFiles: 1,
      filenames: ["hook-return.fixture"],
      truncated: false,
      totalMatches: 1,
      countIsComplete: true,
    };''',
        '''const result = {
      countIsComplete: true,
      totalMatches: 1,
      truncated: false,
      filenames: ["hook-return.fixture"],
      numFiles: 1,
      durationMs: 1,
    };''',
    )
elif case_name == "unknown-top":
    hook_source = hook_source.replace(
        "countIsComplete: true,\n    };",
        'countIsComplete: true,\n      unknownTop: { nestedUnknown: "discard-at-top" },\n    };',
    )
elif case_name == "duration-wrong-type":
    hook_source = hook_source.replace('durationMs: 1,', 'durationMs: "wrong",')
elif case_name == "duration-missing":
    hook_source = hook_source.replace("durationMs: 1,\n", "")
elif case_name == "filenames-item-wrong-type":
    hook_source = hook_source.replace(
        'filenames: ["hook-return.fixture"],',
        'filenames: ["hook-return.fixture", 13],',
    )
hook_path.write_text(hook_source, encoding="utf-8")
for name in ("home", "config", "tmp", "cwd"):
    (run_root / name).mkdir()
(run_root / "cwd" / "probe.fixture").write_text("fixed read-only glob fixture\n", encoding="utf-8")
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
        if not accepted:
            body, status = b"{}", 404
        else:
            request_path = run_root / f"request-{index:02}.raw.json"
            request_path.write_bytes(raw)
            record["raw_request"] = request_path.name
            try:
                body_value = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                body_value = {}
            tool_names = [tool.get("name") for tool in body_value.get("tools", [])
                          if isinstance(tool, dict)]
            record["advertised_glob"] = "Glob" in tool_names
            record["tool_names"] = tool_names
            body, status = model_response(index == 1), 200
            response_path = run_root / f"response-{index:02}.raw.sse"
            response_path.write_bytes(body)
            stub_responses.append({"path": response_path.name, "bytes": len(body),
                                   "sha256": sha256(body)})
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Type", "text/event-stream" if accepted else "application/json")
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
    "--strict-mcp-config", "--print", "--tools", tools_setting,
    "--dangerously-skip-permissions", "--debug", "hooks", "--max-turns", "2",
    "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"],
    FIXTURE["prompt"],
]
try:
    completed = subprocess.run(argv, cwd=run_root / "cwd", env=env,
                               capture_output=True, timeout=60, check=False)
    stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
except subprocess.TimeoutExpired as error:
    stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)

(run_root / "native.stdout.raw").write_bytes(stdout)
(run_root / "native.stderr.raw").write_bytes(stderr)
if request_records and request_records[0].get("raw_request"):
    first_request_raw = (run_root / request_records[0]["raw_request"]).read_bytes()
    tools_raw = raw_top_level_member(first_request_raw, "tools")
    (run_root / "tool-catalog.raw.json").write_bytes(tools_raw)
else:
    tools_raw = None
combined = stdout + stderr
marker_lines = [line for line in combined.splitlines() if b"NATIVE_GLOB_TOOL_CALL_HOOK=" in line]
second_tool_result = None
if len(request_records) >= 2:
    second_path = run_root / request_records[1]["raw_request"]
    second_raw = second_path.read_bytes()
    second = json.loads(second_raw)
    for message_index, message in enumerate(second.get("messages", [])):
        content = message.get("content")
        if isinstance(content, list):
            for block_index, block in enumerate(content):
                if (isinstance(block, dict) and block.get("type") == "tool_result"
                        and block.get("tool_use_id") == FIXTURE["tool_use_id"]):
                    second_tool_result = block.get("content")
                    (run_root / "model-tool-result-content.decoded.json").write_text(
                        json.dumps(second_tool_result, ensure_ascii=True, separators=(",", ":")) + "\n",
                        encoding="utf-8",
                    )

debug_files = list((run_root / "config" / "debug").glob("*.txt"))
debug_marker_lines = []
for path in debug_files:
    debug_marker_lines.extend(line for line in path.read_bytes().splitlines()
                               if b"NATIVE_GLOB_TOOL_CALL_HOOK=" in line)
tool_result_rows = []
tool_use_result_raw_values = []
for transcript in (run_root / "config").rglob("*.jsonl"):
    for line in transcript.read_bytes().splitlines(keepends=True):
        try:
            row = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if row.get("toolUseResult") is not None:
            tool_result_rows.append(line)
            tool_use_result_raw_values.append(
                raw_top_level_member(line.rstrip(b"\r\n"), "toolUseResult")
            )
if tool_result_rows:
    (run_root / "tool-use-result-session-rows.raw.jsonl").write_bytes(b"".join(tool_result_rows))
if tool_use_result_raw_values:
    (run_root / "tool-use-result.raw.json").write_bytes(tool_use_result_raw_values[0])
if isinstance(second_tool_result, str):
    (run_root / "model-tool-result-content.raw.txt").write_bytes(second_tool_result.encode("utf-8"))
if debug_marker_lines:
    (run_root / "hook-marker.raw.txt").write_bytes(b"\n".join(debug_marker_lines) + b"\n")
expected_result = dict(FIXTURE["expected_result"])
if case_name in {"duration-1e400", "duration-infinity", "duration-negative-infinity", "duration-nan"}:
    expected_result["durationMs"] = {
        "js_expression": case_duration[case_name],
        "runtime_string": (
            "NaN" if case_name == "duration-nan"
            else "-Infinity" if case_name == "duration-negative-infinity"
            else "Infinity"
        ),
    }
if case_name == "duration-wrong-type":
    expected_result["durationMs"] = "wrong"
if case_name == "duration-missing":
    expected_result.pop("durationMs", None)
if case_name == "filenames-item-wrong-type":
    expected_result["filenames"] = ["hook-return.fixture", 13]
if case_name == "unknown-top":
    expected_result["unknownTop"] = {"nestedUnknown": "discard-at-top"}
parsed_results = [json.loads(line).get("toolUseResult") for line in tool_result_rows]
explicit_glob = tools_setting != "default"
checks = {
    "catalog_glob_availability_matches_cli_profile": bool(request_records)
    and request_records[0].get("advertised_glob") is explicit_glob,
    "hook_marker_matches_tool_availability": bool(debug_marker_lines) is explicit_glob,
    "two_loopback_requests": len(request_records) == 2
    and all(item.get("accepted") and item.get("dummy_key_matched") for item in request_records),
}
if tools_setting == "default":
    checks["default_profile_reports_glob_unavailable"] = (
        isinstance(second_tool_result, str) and "No such tool available: Glob" in second_tool_result
    )
elif case_name in {"override", "unknown-top"}:
    parsed = parsed_results[0] if parsed_results else None
    checks["native_parsed_hook_wrapper_result"] = (
        isinstance(parsed, dict)
        and parsed.get("filenames") == ["hook-return.fixture"]
        and parsed.get("durationMs") == 1
    )
    if case_name == "unknown-top":
        checks["native_strips_unknown_top_level_key"] = (
            isinstance(parsed, dict) and "unknownTop" not in parsed
        )
elif case_name in {"duration-1e400", "duration-infinity", "duration-negative-infinity", "duration-nan"}:
    checks["native_rejects_non_finite_number"] = (
        isinstance(second_tool_result, str)
        and "tool.call step resolved Glob with a result that does not match its output shape" in second_tool_result
        and "expected number" in second_tool_result
        and any(label in second_tool_result for label in ("received Infinity", "received -Infinity", "received NaN"))
    )
elif case_name in {"duration-wrong-type", "duration-missing", "filenames-item-wrong-type"}:
    checks["native_rejects_output_type_mismatch"] = (
        isinstance(second_tool_result, str)
        and "tool.call step resolved Glob with a result that does not match its output shape" in second_tool_result
    )
    expected_issues = {
        "duration-wrong-type": [{
            "expected": "number", "code": "invalid_type", "path": ["durationMs"],
            "message": "Invalid input: expected number, received string",
        }],
        "duration-missing": [{
            "expected": "number", "code": "invalid_type", "path": ["durationMs"],
            "message": "Invalid input: expected number, received undefined",
        }],
        "filenames-item-wrong-type": [{
            "expected": "string", "code": "invalid_type", "path": ["filenames", 1],
            "message": "Invalid input: expected string, received number",
        }],
    }[case_name]
    issue_list = None
    if isinstance(second_tool_result, str):
        marker = "tool.call step resolved Glob with a result that does not match its output shape:"
        marker_index = second_tool_result.find(marker)
        if marker_index >= 0:
            json_start = second_tool_result.find("[", marker_index + len(marker))
            if json_start >= 0:
                try:
                    issue_list, _end = json.JSONDecoder().raw_decode(second_tool_result, json_start)
                except json.JSONDecodeError:
                    issue_list = None
    checks["native_issue_path_and_message_match"] = issue_list == expected_issues
elif case_name == "known-reversed":
    parsed = parsed_results[0] if parsed_results else None
    raw_order = None
    if tool_use_result_raw_values:
        raw_result_value = json.loads(tool_use_result_raw_values[0])
        if isinstance(raw_result_value, dict):
            raw_order = list(raw_result_value.keys())
    checks["native_parses_reversed_known_fields"] = (
        isinstance(parsed, dict)
        and parsed.get("filenames") == ["hook-return.fixture"]
        and parsed.get("durationMs") == 1
    )
    checks["native_raw_tool_use_result_uses_schema_key_order"] = raw_order == [
        "durationMs", "numFiles", "filenames", "truncated", "totalMatches", "countIsComplete"
    ]
receipt = {
    "native_version": version_text,
    "native_sha256": binary_sha,
    "case": case_name,
    "runner_sha256": sha256(Path(__file__).read_bytes()),
    "hook_sha256": sha256((ROOT / "plugin" / "hooks" / "register.js").read_bytes()),
    "argv": ["$CLAUDE_NATIVE_BINARY", "--plugin-dir", "<isolated-plugin>",
             "--setting-sources", "project", "--strict-mcp-config", "--print",
             "--tools", tools_setting, "--dangerously-skip-permissions", "--debug", "hooks",
             "--max-turns", "2", "--model", FIXTURE["model"],
             "--session-id", FIXTURE["session_id"], FIXTURE["prompt"]],
    "loopback_only": True,
    "dummy_key_only": True,
    "tools_setting": tools_setting,
    "run_name": run_name,
    "timed_out": timed_out,
    "exit_code": exit_code,
    "requests": request_records,
    "stub_responses": stub_responses,
    "tool_catalog_raw": {"path": "tool-catalog.raw.json", "bytes": len(tools_raw),
                          "sha256": sha256(tools_raw)} if tools_raw is not None else None,
    "tool_use_result_session_rows": {"path": "tool-use-result-session-rows.raw.jsonl",
                                      "rows": len(tool_result_rows),
                                      "sha256": sha256(b"".join(tool_result_rows))}
                                     if tool_result_rows else None,
    "tool_use_result_raw_json": {
        "path": "tool-use-result.raw.json",
        "bytes": len(tool_use_result_raw_values[0]),
        "sha256": sha256(tool_use_result_raw_values[0]),
        "top_level_key_order": list(json.loads(tool_use_result_raw_values[0]).keys())
        if isinstance(json.loads(tool_use_result_raw_values[0]), dict) else None,
    } if tool_use_result_raw_values else None,
    "hook_marker_stdout_or_stderr": [line.decode("utf-8", errors="backslashreplace")
                                      for line in marker_lines],
    "hook_marker_debug_log": [line.decode("utf-8", errors="backslashreplace")
                               for line in debug_marker_lines],
    "model_tool_result_content": second_tool_result,
    "parsed_tool_use_results": [json.loads(line).get("toolUseResult")
                                 for line in tool_result_rows],
    "checks": checks,
    "expected_return_object": expected_result,
    "hook_input_key_order": [
        "countIsComplete", "totalMatches", "truncated", "filenames", "numFiles", "durationMs"
    ] if case_name == "known-reversed" else None,
    "model_tool_result_content_utf8": {
        "path": "model-tool-result-content.raw.txt",
        "bytes": len(second_tool_result.encode("utf-8")),
        "sha256": sha256(second_tool_result.encode("utf-8")),
    } if isinstance(second_tool_result, str) else None,
    "hook_marker_raw": {
        "path": "hook-marker.raw.txt",
        "bytes": len(b"\n".join(debug_marker_lines) + b"\n"),
        "sha256": sha256(b"\n".join(debug_marker_lines) + b"\n"),
    } if debug_marker_lines else None,
    "stdout": {"path": "native.stdout.raw", "bytes": len(stdout), "sha256": sha256(stdout)},
    "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
    "run_output_dir": str(run_root.relative_to(ROOT)),
    "scope": "Native 2.1.293 public CLI tool.call continuation-wrapper output parser probe for built-in Glob; all model and file inputs are local and fixed",
}
(run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n",
                                       encoding="utf-8")
print(json.dumps(receipt, ensure_ascii=True, indent=2))
if timed_out or exit_code != 0 or not request_records or not all(checks.values()):
    raise SystemExit(1)
