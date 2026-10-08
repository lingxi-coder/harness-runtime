#!/usr/bin/env python3
"""Probe Native 2.1.293 cold resume of an object-valued tool_result."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
BINARY_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-tool-result-json-resume-dummy-key"
SESSION_ID = FIXTURE["session_id"]
TOOL_USE_ID = FIXTURE["tool_use_id"]
TIME = "2026-10-08T00:00:00.000Z"


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utf16_units(value: str) -> list[int]:
    raw = value.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index : index + 2], "little") for index in range(0, len(raw), 2)]


def sse_text(text: str, message_id: str) -> bytes:
    events = [
        (
            "message_start",
            {
                "type": "message_start",
                "message": {
                    "id": message_id,
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
                "delta": {"type": "text_delta", "text": text},
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
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("utf-8")


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


def build_object_result() -> tuple[dict, str]:
    value = (
        FIXTURE["value_prefix_character"] * FIXTURE["value_prefix_units"]
        + FIXTURE["boundary_character"]
        + FIXTURE["middle_text"]
        + FIXTURE["tail_character"]
    )
    result = {FIXTURE["json_key"]: value}
    serialized = json.dumps(result, ensure_ascii=False, separators=(",", ":"))
    return result, serialized


def append_object_tool_result(session_path: Path, cwd: Path, tool_result: dict) -> bytes:
    lines = session_path.read_bytes().splitlines(keepends=True)
    rows = [json.loads(line) for line in lines]
    last_message = next(
        row for row in reversed(rows) if row.get("type") in ("user", "assistant") and row.get("uuid")
    )
    last_assistant = next(row for row in reversed(rows) if row.get("type") == "assistant")
    assistant_uuid = "00000000-0000-4000-8000-000000002948"
    result_uuid = "00000000-0000-4000-8000-000000002949"
    assistant_row = {
        key: value
        for key, value in last_assistant.items()
        if key not in ("message", "uuid", "parentUuid", "timestamp", "sessionId")
    }
    assistant_row.update(
        {
            "type": "assistant",
            "uuid": assistant_uuid,
            "parentUuid": last_message["uuid"],
            "sessionId": SESSION_ID,
            "timestamp": TIME,
            "cwd": str(cwd),
            "message": {
                "id": "msg_native_json_resume_tool_use",
                "type": "message",
                "role": "assistant",
                "model": "claude-sonnet-4-5-20250929",
                "content": [
                    {
                        "type": "tool_use",
                        "id": TOOL_USE_ID,
                        "name": FIXTURE["tool_use_name"],
                        "input": FIXTURE["tool_input"],
                    }
                ],
                "stop_reason": "tool_use",
                "stop_sequence": None,
                "usage": {"input_tokens": 1, "output_tokens": 1},
            },
        }
    )
    user_template = next(row for row in rows if row.get("type") == "user")
    user_row = {
        key: value
        for key, value in user_template.items()
        if key not in ("message", "uuid", "parentUuid", "timestamp", "sessionId", "toolUseResult", "sourceToolAssistantUUID", "promptId")
    }
    user_row.update(
        {
            "type": "user",
            "uuid": result_uuid,
            "parentUuid": assistant_uuid,
            "sessionId": SESSION_ID,
            "timestamp": TIME,
            "cwd": str(cwd),
            "sourceToolAssistantUUID": assistant_uuid,
            "promptId": "00000000-0000-4000-8000-000000002950",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": TOOL_USE_ID,
                        "content": tool_result,
                    }
                ],
            },
        }
    )
    serialized_rows = [
        json.dumps(row, ensure_ascii=True, separators=(",", ":")).encode("utf-8") + b"\n"
        for row in (assistant_row, user_row)
    ]
    output = bytearray()
    inserted = False
    for raw_line, row in zip(lines, rows):
        if row.get("type") == "last-prompt":
            updated = dict(row)
            updated["leafUuid"] = result_uuid
            output.extend(b"".join(serialized_rows))
            inserted = True
            output.extend(json.dumps(updated, ensure_ascii=True, separators=(",", ":")).encode("utf-8") + b"\n")
        else:
            output.extend(raw_line)
    if not inserted:
        output.extend(b"".join(serialized_rows))
    session_path.write_bytes(output)
    return serialized_rows[-1]


def main() -> None:
    if not BINARY_VALUE:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
    binary = Path(BINARY_VALUE).expanduser().resolve()
    binary_sha = sha256(binary.read_bytes())
    if binary_sha != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {binary_sha}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or not version_text.startswith(FIXTURE["native_version"] + " "):
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    tool_result, serialized_source = build_object_result()
    source_units = utf16_units(serialized_source)
    if len(source_units) != 50_007 or source_units[49_999:50_001] != [0xD83D, 0xDE00]:
        raise SystemExit("fixture JSON.stringify length or boundary changed")

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix="tool-result-json-resume-", dir=OUTPUT))
    for name in ("home", "config", "tmp", "cwd"):
        (run_root / name).mkdir()
    cwd = run_root / "cwd"
    request_records: list[dict] = []
    phase = ["initial"]

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
            request_records.append({"method": "HEAD", "phase": phase[0], "path": self.path, "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            length = int(self.headers.get("content-length", "0"))
            raw = self.rfile.read(length)
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            index = 1 + sum(record.get("method") == "POST" for record in request_records)
            record = {
                "method": "POST",
                "phase": phase[0],
                "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
            }
            request_path = run_root / "requests" / f"request-{index}.full.raw.json"
            request_path.parent.mkdir(exist_ok=True)
            request_path.write_bytes(raw)
            record["raw_request_path"] = str(request_path.relative_to(ROOT))
            request_records.append(record)
            if not accepted:
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401)
                return
            text = "Native seed complete" if phase[0] == "initial" else "Native resume complete"
            self.reply(200, sse_text(text, f"msg_native_json_resume_{phase[0]}_{index}"), "text/event-stream")

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
    common = [
        str(binary), "--setting-sources", "project", "--strict-mcp-config", "--print",
        "--output-format", "json", "--tools", "default", "--dangerously-skip-permissions",
        "--max-turns", "1", "--model", FIXTURE["model"],
    ]
    phases = [
        ("initial", common + ["--session-id", SESSION_ID, FIXTURE["initial_prompt"]]),
    ]
    phase_results = []
    try:
        for phase_name, argv in phases:
            phase[0] = phase_name
            try:
                completed = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=60, check=False)
                stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
            except subprocess.TimeoutExpired as error:
                stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
            (run_root / f"{phase_name}.stdout.raw").write_bytes(stdout)
            (run_root / f"{phase_name}.stderr.raw").write_bytes(stderr)
            phase_results.append({"phase": phase_name, "exit_code": exit_code, "timed_out": timed_out})
            if timed_out or exit_code != 0:
                raise SystemExit(f"Native {phase_name} phase failed: {phase_results[-1]}")

        session_files = list((run_root / "config").glob(f"projects/*/{SESSION_ID}.jsonl"))
        if len(session_files) != 1:
            raise SystemExit(f"expected one Native session JSONL, found {len(session_files)}")
        session_path = session_files[0]
        result_row_raw = append_object_tool_result(session_path, cwd, tool_result)
        (run_root / "resume-input-tool-result-session-row.raw.jsonl").write_bytes(result_row_raw)

        phase[0] = "resume"
        resume_argv = common + ["--resume", SESSION_ID, FIXTURE["resume_prompt"]]
        try:
            completed = subprocess.run(resume_argv, cwd=cwd, env=env, capture_output=True, timeout=60, check=False)
            stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
        (run_root / "resume.stdout.raw").write_bytes(stdout)
        (run_root / "resume.stderr.raw").write_bytes(stderr)
        phase_results.append({"phase": "resume", "exit_code": exit_code, "timed_out": timed_out})
        if timed_out or exit_code != 0:
            raise SystemExit(f"Native resume phase failed: {phase_results[-1]}")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    session_raw = session_path.read_bytes()
    (run_root / "session.full.raw.jsonl").write_bytes(session_raw)
    persisted_tool_rows = []
    for line in session_raw.splitlines(keepends=True):
        try:
            row = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        message = row.get("message", {})
        blocks = message.get("content") if isinstance(message, dict) else None
        if not isinstance(blocks, list):
            continue
        matching_blocks = [
            block
            for block in blocks
            if (
                isinstance(block, dict)
                and block.get("type") == "tool_result"
                and block.get("tool_use_id") == TOOL_USE_ID
            )
        ]
        if matching_blocks:
            persisted_tool_rows.append((line, row, matching_blocks[0]))
    if len(persisted_tool_rows) == 1:
        persisted_session_row_raw, _persisted_session_row, persisted_session_tool_result = persisted_tool_rows[0]
        (run_root / "post-resume-tool-result-session-row.raw.jsonl").write_bytes(
            persisted_session_row_raw
        )
        persisted_content_kind = type(persisted_session_tool_result.get("content")).__name__
    else:
        persisted_session_row_raw = None
        persisted_content_kind = None
    resume_requests = [
        record for record in request_records
        if record.get("phase") == "resume" and record.get("method") == "POST" and record.get("accepted")
    ]
    matches = []
    for record in resume_requests:
        raw = (ROOT / record["raw_request_path"]).read_bytes()
        value = json.loads(raw)
        for message_index, message in enumerate(value.get("messages", [])):
            content = message.get("content")
            if not isinstance(content, list):
                continue
            for block_index, block in enumerate(content):
                if (
                    isinstance(block, dict)
                    and block.get("type") == "tool_result"
                    and block.get("tool_use_id") == TOOL_USE_ID
                ):
                    matches.append((record, raw, value, message_index, block_index, block))
    if len(matches) == 1:
        record, raw, request_value, message_index, block_index, block = matches[0]
        raw_text = raw.decode("utf-8")
        start, end = path_span(raw_text, ["messages", message_index, "content", block_index, "content"])
        model_result_raw = raw_text[start:end].encode("utf-8")
        (run_root / "model-tool-result-content.raw.json").write_bytes(model_result_raw)
        model_result = block.get("content")
    else:
        record = None
        model_result_raw = None
        model_result = None

    if isinstance(model_result, str):
        visible_result = model_result.split("\n\n<system-reminder>", 1)[0]
        omitted = re.search(r"\n\[(\d+) more characters? of this tool result not shown\]$", visible_result)
        result_units = utf16_units(visible_result)
        omitted_count = int(omitted.group(1)) if omitted else None
    else:
        visible_result = None
        result_units = []
        omitted_count = None
    expected_visible_result = (
        serialized_source[:49_999]
        + "\n[6 more characters of this tool result not shown]"
    )
    checks = {
        "synthetic_jsonl_object_tool_result_reached_public_resume": len(matches) == 1,
        "resume_request_was_loopback_dummy_key_only": len(resume_requests) >= 1 and all(record["dummy_key_matched"] for record in resume_requests),
        "native_resume_completed": phase_results[-1]["exit_code"] == 0 and not phase_results[-1]["timed_out"],
        "persisted_session_retains_object_tool_result": len(persisted_tool_rows) == 1 and persisted_content_kind == "dict",
        "native_json_string_cut_and_omitted_count_match": (
            isinstance(model_result, str)
            and visible_result == expected_visible_result
            and omitted_count == 6
            and len(result_units) == 50_049
        ),
    }
    receipt = {
        "native_version": version_text,
        "native_sha256": binary_sha,
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "fixture_sha256": {"cases.json": sha256((ROOT / "cases.json").read_bytes())},
        "fixed_input": {
            "session_id": SESSION_ID,
            "tool_use_id": TOOL_USE_ID,
            "tool_use_name": FIXTURE["tool_use_name"],
            "tool_result_content_kind": "object",
            "json_stringify_utf16_units": len(source_units),
            "json_stringify_units_49999_50000": [f"U+{source_units[49999]:04X}", f"U+{source_units[50000]:04X}"],
            "tail_pair_at": 50003,
        },
        "loopback_only": True,
        "dummy_key_only": True,
        "phase_results": phase_results,
        "requests": request_records,
        "resume_request": {
            "raw_result_content_path": "model-tool-result-content.raw.json" if model_result_raw is not None else None,
            "raw_result_content_bytes": len(model_result_raw) if model_result_raw is not None else None,
            "raw_result_content_sha256": sha256(model_result_raw) if model_result_raw is not None else None,
            "content_kind": type(model_result).__name__ if model_result is not None else None,
            "visible_result_utf16_units": len(result_units) if model_result is not None else None,
            "visible_result_unit_at_49998_49999": [f"U+{result_units[i]:04X}" for i in (49998, 49999) if i < len(result_units)],
            "ends_with_explicit_omitted_count": bool(omitted),
            "omitted_character_count": omitted_count,
            "visible_result_suffix": visible_result[-96:] if visible_result is not None else None,
        },
        "input_jsonl_row": {
            "path": "resume-input-tool-result-session-row.raw.jsonl",
            "bytes": len(result_row_raw),
            "sha256": sha256(result_row_raw),
            "message_content_kind": "object",
        },
        "post_resume_session_row": {
            "path": "post-resume-tool-result-session-row.raw.jsonl" if persisted_session_row_raw is not None else None,
            "bytes": len(persisted_session_row_raw) if persisted_session_row_raw is not None else None,
            "sha256": sha256(persisted_session_row_raw) if persisted_session_row_raw is not None else None,
            "message_content_kind": persisted_content_kind,
        },
        "checks": checks,
        "scope": "Public Native 2.1.293 --resume after a fixed synthetic JSONL object-valued tool_result is inserted into an isolated session created by the public CLI. This tests cold-resume projection only; the tool_result row itself is controlled fixture input, not emitted by a Native tool execution.",
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(receipt, ensure_ascii=True, indent=2))
    if not all(checks.values()):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
