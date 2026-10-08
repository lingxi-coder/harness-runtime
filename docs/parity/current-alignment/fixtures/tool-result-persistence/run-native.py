#!/usr/bin/env python3
"""Capture Native 2.1.293 public registered-tool persistence behavior."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
BINARY_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-tool-result-persist-fixture-dummy-key"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
CASE_NAME = os.environ.get("NATIVE_TOOL_RESULT_CASE", "lone-unit")
FIXTURE_DOCUMENT = FIXTURE
if CASE_NAME == "lone-unit":
    FIXTURE = {key: value for key, value in FIXTURE_DOCUMENT.items() if key != "unicode_boundary"}
elif CASE_NAME == "unicode-boundary":
    FIXTURE = {
        **{key: value for key, value in FIXTURE_DOCUMENT.items() if key != "unicode_boundary"},
        **FIXTURE_DOCUMENT["unicode_boundary"],
    }
else:
    raise SystemExit(f"unknown NATIVE_TOOL_RESULT_CASE: {CASE_NAME}")
SESSION_ID = FIXTURE["session_id"]
TOOL_USE_ID = FIXTURE["tool_use_id"]
TOOL_API_NAME = f"mcp__{FIXTURE['plugin_name']}__{FIXTURE['tool_name']}"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utf16_units(value: str) -> list[int]:
    raw = value.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index : index + 2], "little") for index in range(0, len(raw), 2)]


def expected_tool_result() -> str:
    if CASE_NAME == "lone-unit":
        return (
            FIXTURE["result_prefix"]
            + chr(FIXTURE["lone_unit"])
            + FIXTURE["padding_character"] * FIXTURE["padding_units"]
        )
    return (
        FIXTURE["result_prefix_character"] * FIXTURE["result_prefix_units"]
        + FIXTURE["boundary_character"]
        + FIXTURE["middle_text"]
        + FIXTURE["tail_character"]
        + FIXTURE["padding_character"] * FIXTURE["padding_units"]
    )


def sse(events: list[tuple[str, dict]]) -> bytes:
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


def response_stream(first_request: bool) -> bytes:
    if first_request:
        events = [
            (
                "message_start",
                {
                    "type": "message_start",
                    "message": {
                        "id": "msg_native_tool_result_fixture_call",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-sonnet-4-5-20250929",
                        "content": [],
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": {"input_tokens": 5, "output_tokens": 0},
                    },
                },
            ),
            (
                "content_block_start",
                {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": TOOL_USE_ID,
                        "name": TOOL_API_NAME,
                        "input": {},
                    },
                },
            ),
            (
                "content_block_delta",
                {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "{}"},
                },
            ),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            (
                "message_delta",
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                    "usage": {"output_tokens": 1},
                },
            ),
            ("message_stop", {"type": "message_stop"}),
        ]
    else:
        events = [
            (
                "message_start",
                {
                    "type": "message_start",
                    "message": {
                        "id": "msg_native_tool_result_fixture_done",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-sonnet-4-5-20250929",
                        "content": [],
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": {"input_tokens": 5, "output_tokens": 0},
                    },
                },
            ),
            (
                "content_block_start",
                {
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""},
                },
            ),
            (
                "content_block_delta",
                {
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "fixture complete"},
                },
            ),
            ("content_block_stop", {"type": "content_block_stop", "index": 0}),
            (
                "message_delta",
                {
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                    "usage": {"output_tokens": 2},
                },
            ),
            ("message_stop", {"type": "message_stop"}),
        ]
    return sse(events)


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
    start, end = 0, len(text)
    for part in path:
        start, end = (
            member_span(text, start, part)
            if isinstance(part, str)
            else element_span(text, start, part)
        )
    return start, end


def strings(value):
    if isinstance(value, str):
        yield value
    elif isinstance(value, dict):
        for child in value.values():
            yield from strings(child)
    elif isinstance(value, list):
        for child in value:
            yield from strings(child)


def find_envelope(value) -> str | None:
    for candidate in strings(value):
        if "<persisted-output>" in candidate:
            return candidate
    return None


def preview_from_envelope(envelope: str) -> str:
    preview_header = "Preview (first 2KB):\n"
    preview_start = envelope.index(preview_header) + len(preview_header)
    preview_end = envelope.index("\n...\n</persisted-output>", preview_start)
    return envelope[preview_start:preview_end]


def main() -> None:
    fixed_contract = {
        "plugin_name": "native-tool-result-persist-fixture",
        "tool_name": "probe",
    }
    if any(FIXTURE.get(key) != value for key, value in fixed_contract.items()):
        raise SystemExit("cases.json and the fixed plugin module no longer describe the same fixture")
    if CASE_NAME == "lone-unit" and any(
        FIXTURE.get(key) != value
        for key, value in {
            "result_prefix": "begin",
            "lone_unit": 0xD800,
            "padding_character": "z",
            "padding_units": 50_001,
        }.items()
    ):
        raise SystemExit("cases.json and the lone-unit plugin no longer describe the same fixture")
    if CASE_NAME == "unicode-boundary" and any(
        FIXTURE.get(key) != value
        for key, value in {
            "result_prefix_units": 1999,
            "result_prefix_character": "a",
            "boundary_character": "😀",
            "middle_text": "TAIL",
            "tail_character": "😀",
            "padding_character": "z",
            "padding_units": 48_000,
        }.items()
    ):
        raise SystemExit("cases.json and the Unicode-boundary plugin no longer describe the same fixture")
    if not BINARY_VALUE:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
    binary = Path(BINARY_VALUE).expanduser().resolve()
    binary_sha = sha256(binary.read_bytes())
    if binary_sha != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {binary_sha}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or not version_text.startswith("2.1.293 "):
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix=f"tool-result-native-{CASE_NAME}-", dir=OUTPUT))
    plugin = run_root / "plugin"
    shutil.copytree(ROOT / "plugin", plugin)
    plugin_hook_source = ROOT / "plugin" / "hooks" / (
        "register.js" if CASE_NAME == "lone-unit" else "register-unicode-boundary.js"
    )
    if CASE_NAME == "unicode-boundary":
        shutil.copyfile(plugin_hook_source, plugin / "hooks" / "register.js")
    for name in ("home", "config", "tmp", "cwd"):
        (run_root / name).mkdir()
    request_records: list[dict] = []

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
            request_records.append({"method": "HEAD", "path": self.path, "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            length = int(self.headers.get("content-length", "0"))
            raw = self.rfile.read(length)
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            record = {
                "method": "POST",
                "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
            }
            if not accepted:
                request_records.append(record)
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401)
                return
            index = sum(item.get("method") == "POST" for item in request_records) + 1
            request_path = run_root / "requests" / f"request-{index}.full.raw.json"
            request_path.parent.mkdir(exist_ok=True)
            request_path.write_bytes(raw)
            record["raw_request_path"] = str(request_path.relative_to(ROOT))
            request_records.append(record)
            try:
                request_value = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                self.reply(400)
                return
            registered = [
                item.get("name")
                for item in request_value.get("tools", [])
                if isinstance(item, dict)
            ]
            if TOOL_API_NAME not in registered:
                self.reply(400)
                return
            self.reply(200, response_stream(index == 1), "text/event-stream")

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
        "--strict-mcp-config", "--print", "--tools", "default",
        "--dangerously-skip-permissions", "--debug", "hooks", "--max-turns", "2",
        "--model", FIXTURE["model"], "--session-id", SESSION_ID, FIXTURE["prompt"],
    ]
    try:
        result = subprocess.run(
            argv, cwd=run_root / "cwd", env=env, capture_output=True, timeout=60, check=False
        )
        timed_out = False
    except subprocess.TimeoutExpired as error:
        result = None
        timed_out = True
        stdout, stderr = error.stdout or b"", error.stderr or b""
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    if result is not None:
        stdout, stderr = result.stdout, result.stderr
        exit_code = result.returncode
    else:
        exit_code = None
    (run_root / "native.stdout.raw").write_bytes(stdout)
    (run_root / "native.stderr.raw").write_bytes(stderr)

    request_values = []
    for record in request_records:
        relative = record.get("raw_request_path")
        if not relative:
            continue
        raw = (ROOT / relative).read_bytes()
        decoded = json.loads(raw)
        request_values.append(decoded)
    if len(request_values) >= 2:
        second_raw_path = request_records[1]["raw_request_path"]
        second_raw = (ROOT / second_raw_path).read_bytes()
        second_text = second_raw.decode("utf-8")
        second = request_values[1]
        selected_paths = []
        for message_index, message in enumerate(second.get("messages", [])):
            content = message.get("content")
            if not isinstance(content, list):
                continue
            for content_index, block in enumerate(content):
                if isinstance(block, dict) and block.get("type") == "tool_result" and block.get("tool_use_id") == TOOL_USE_ID:
                    selected_paths.append(["messages", message_index, "content", content_index, "content"])
        if len(selected_paths) != 1:
            raise SystemExit(f"expected one Native tool_result, found {len(selected_paths)}")
        start, end = path_span(second_text, selected_paths[0])
        tool_result_content_raw = second_text[start:end].encode("utf-8")
        tool_result_content_path = run_root / "tool-result-content.raw.json"
        tool_result_content_path.write_bytes(tool_result_content_raw)
        decoded_tool_result = second["messages"][selected_paths[0][1]]["content"][selected_paths[0][3]]["content"]
        wire_preview = preview_from_envelope(decoded_tool_result)
    else:
        tool_result_content_path = None
        tool_result_content_raw = None
        decoded_tool_result = None
        wire_preview = None

    session_files = list((run_root / "config").glob(f"projects/*/{SESSION_ID}.jsonl"))
    session_row_raw = None
    session_preview = None
    if len(session_files) == 1:
        session_raw = session_files[0].read_bytes()
        (run_root / "session.full.raw.jsonl").write_bytes(session_raw)
        selected_session_rows = []
        for line in session_raw.splitlines(keepends=True):
            try:
                row = json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            envelope = find_envelope(row)
            if envelope is not None:
                selected_session_rows.append((line, envelope))
        if len(selected_session_rows) != 1:
            raise SystemExit(f"expected one persisted tool result row, found {len(selected_session_rows)}")
        session_row_raw, session_envelope = selected_session_rows[0]
        (run_root / "tool-result-session-row.raw.jsonl").write_bytes(session_row_raw)
        session_preview = preview_from_envelope(session_envelope)

    persisted_paths = list((run_root / "config").rglob(f"tool-results/{TOOL_USE_ID}.txt"))
    if len(persisted_paths) == 1:
        persisted_raw = persisted_paths[0].read_bytes()
        (run_root / "persisted-tool-result.raw").write_bytes(persisted_raw)
    else:
        persisted_raw = None

    debug_path = run_root / "config" / "debug" / f"{SESSION_ID}.txt"
    debug_raw = debug_path.read_bytes() if debug_path.exists() else b""
    tool_meta_lines = [
        line for line in debug_raw.splitlines(keepends=True)
        if b"NATIVE_TOOL_RESULT_META=" in line
    ]
    if len(tool_meta_lines) == 1:
        tool_meta_raw = tool_meta_lines[0]
        (run_root / "tool-call-meta.raw.txt").write_bytes(tool_meta_raw)
        tool_meta_json = json.loads(tool_meta_raw.split(b"NATIVE_TOOL_RESULT_META=", 1)[1])
    else:
        tool_meta_raw = None
        tool_meta_json = None

    expected_source = expected_tool_result()
    expected_source_units = utf16_units(expected_source)
    if CASE_NAME == "lone-unit":
        expected_file = (
            FIXTURE["result_prefix"].encode("utf-8")
            + b"\xef\xbf\xbd"
            + (FIXTURE["padding_character"] * FIXTURE["padding_units"]).encode("utf-8")
        )
    else:
        expected_file = expected_source.encode("utf-8")
    session_units = utf16_units(session_preview) if session_preview is not None else []
    wire_units = utf16_units(wire_preview) if wire_preview is not None else []
    checks = {
        "registered_tool_in_first_request": bool(request_values) and any(
            isinstance(tool, dict) and tool.get("name") == TOOL_API_NAME
            for tool in request_values[0].get("tools", [])
        ),
        "two_fixed_requests": len(request_values) == 2 and len(request_records) == 2,
        "tool_result_envelope_in_request": decoded_tool_result is not None and "<persisted-output>" in decoded_tool_result,
        "native_tool_call_hook_executed": isinstance(tool_meta_json, dict) and tool_meta_json.get("tool") == TOOL_API_NAME and tool_meta_json.get("tool_use_id") == TOOL_USE_ID and tool_meta_json.get("result_units") == len(expected_source_units),
        "native_preview_has_2000_utf16_units": len(session_units) == 2_000 and len(wire_units) == 2_000,
        "persisted_file_matches_expected_utf8_boundary": persisted_raw == expected_file,
    }
    if CASE_NAME == "lone-unit":
        checks["session_preview_keeps_lone_unit"] = (
            len(session_units) == 2_000 and session_units[5] == FIXTURE["lone_unit"]
        )
        checks["request_preview_uses_replacement_unit"] = (
            len(wire_units) == 2_000
            and wire_units[5] == 0xFFFD
            and FIXTURE["lone_unit"] not in wire_units
        )
    else:
        checks["source_contains_boundary_and_tail_non_bmp"] = (
            expected_source_units[1999:2001] == [0xD83D, 0xDE00]
            and expected_source_units[2005:2007] == [0xD83D, 0xDE00]
        )
    session_envelope_text = find_envelope(json.loads(session_row_raw)) if session_row_raw else None
    preview_envelope_text = decoded_tool_result if isinstance(decoded_tool_result, str) else None
    omission_pattern = re.compile(r"\[\d+ more character\(s\)\.\.\.\]")
    boundary_indices = [1998, 1999, 2000, 2001, 2004, 2005, 2006]
    receipt = {
        "native_version": version_text,
        "native_sha256": binary_sha,
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "fixture_sha256": {
            "cases.json": sha256((ROOT / "cases.json").read_bytes()),
            "plugin/hooks/register.js": sha256((ROOT / "plugin/hooks/register.js").read_bytes()),
            "plugin/hooks/register-unicode-boundary.js": sha256((ROOT / "plugin/hooks/register-unicode-boundary.js").read_bytes()),
            "plugin/hooks/hooks.json": sha256((ROOT / "plugin/hooks/hooks.json").read_bytes()),
            "plugin/.claude-plugin/plugin.json": sha256((ROOT / "plugin/.claude-plugin/plugin.json").read_bytes()),
        },
        "active_plugin_hook": str(plugin_hook_source.relative_to(ROOT)),
        "argv": ["$CLAUDE_NATIVE_BINARY", "--plugin-dir", "<isolated-plugin>", "--setting-sources", "project", "--strict-mcp-config", "--print", "--tools", "default", "--dangerously-skip-permissions", "--debug", "hooks", "--max-turns", "2", "--model", FIXTURE["model"], "--session-id", SESSION_ID, FIXTURE["prompt"]],
        "run_output_dir": str(run_root.relative_to(ROOT)),
        "case_name": CASE_NAME,
        "loopback_only": True,
        "dummy_key_only": True,
        "timed_out": timed_out,
        "exit_code": exit_code,
        "requests": request_records,
        "stdout": {"path":"native.stdout.raw", "bytes":len(stdout), "sha256":sha256(stdout)},
        "stderr": {"path":"native.stderr.raw", "bytes":len(stderr), "sha256":sha256(stderr)},
        "tool_result_source": {
            "utf16_units": len(expected_source_units),
            "utf16_units_around_preview_boundary": {
                str(index): f"U+{expected_source_units[index]:04X}"
                for index in boundary_indices
                if index < len(expected_source_units)
            },
        },
        "native_tool_call_meta": {"path":"tool-call-meta.raw.txt", "bytes":len(tool_meta_raw) if tool_meta_raw is not None else None, "sha256":sha256(tool_meta_raw) if tool_meta_raw is not None else None, "observed":tool_meta_json},
        "session_preview": {
            "raw_row_path":"tool-result-session-row.raw.jsonl",
            "bytes":len(session_row_raw) if session_row_raw is not None else None,
            "sha256":sha256(session_row_raw) if session_row_raw is not None else None,
            "utf16_units":len(session_units),
            "units_around_preview_boundary": {
                str(index): f"U+{session_units[index]:04X}"
                for index in boundary_indices
                if index < len(session_units)
            },
            "ends_with_high_surrogate": bool(session_units and 0xD800 <= session_units[-1] <= 0xDBFF),
            "omission_count_marker_present": bool(session_envelope_text and omission_pattern.search(session_envelope_text)),
        },
        "request_preview": {
            "raw_content_path":"tool-result-content.raw.json",
            "bytes":len(tool_result_content_raw) if tool_result_content_path is not None else None,
            "sha256":sha256(tool_result_content_raw) if tool_result_content_raw is not None else None,
            "utf16_units":len(wire_units),
            "units_around_preview_boundary": {
                str(index): f"U+{wire_units[index]:04X}"
                for index in boundary_indices
                if index < len(wire_units)
            },
            "ends_with_high_surrogate": bool(wire_units and 0xD800 <= wire_units[-1] <= 0xDBFF),
            "omission_count_marker_present": bool(preview_envelope_text and omission_pattern.search(preview_envelope_text)),
        },
        "persisted_file": {"path":"persisted-tool-result.raw", "bytes":len(persisted_raw) if persisted_raw is not None else None, "sha256":sha256(persisted_raw) if persisted_raw is not None else None, "utf8_replacement_count":persisted_raw.count(b"\xef\xbf\xbd") if persisted_raw is not None else None},
        "checks": checks,
        "scope": "Public Native CLI + $.tool.register + tool.call; source preview, Anthropic request serialization, session JSONL, and persisted file are separate surfaces. No Harness comparison.",
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(receipt, ensure_ascii=True, indent=2))
    if not all(checks.values()) or timed_out or exit_code != 0:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
