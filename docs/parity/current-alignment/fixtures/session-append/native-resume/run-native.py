#!/usr/bin/env python3
"""Probe Native 2.1.293 cold --resume after a persisted session.append rewrite."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading


ROOT = Path(__file__).resolve().parent
SESSION_FIXTURE = ROOT.parent
OUTPUT = SESSION_FIXTURE / "run-output" / "native-resume"
BINARY_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
CWD_VALUE = os.environ.get("CLAUDE_RESUME_FIXTURE_CWD")
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-session-append-resume-fixture-dummy-key"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
SESSION_ID = FIXTURE["session_id"]
REWRITE_PREFIX = FIXTURE["assistant_rewrite_prefix"]
REWRITE_UNITS = [int(unit) for unit in FIXTURE["assistant_rewrite_units"]]
EXPECTED_SOURCE_UNITS = [
    *[int.from_bytes(REWRITE_PREFIX.encode("utf-16-le")[index:index + 2], "little")
      for index in range(0, len(REWRITE_PREFIX.encode("utf-16-le")), 2)],
    *REWRITE_UNITS,
]
EXPECTED_WELL_FORMED_UNITS = [
    0xFFFD if unit in (0xD800, 0xDC00) else unit for unit in EXPECTED_SOURCE_UNITS
]


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utf16_units(text: str) -> list[int]:
    raw = text.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index:index + 2], "little") for index in range(0, len(raw), 2)]


def sse(text: str, message_id: str) -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [], "stop_reason": None,
            "stop_sequence": None, "usage": {"input_tokens": 5, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": text}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 2}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(body, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, body in events
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


def row_texts(row: object) -> list[str]:
    if not isinstance(row, dict):
        return []
    message = row.get("message")
    content = message.get("content") if isinstance(message, dict) else None
    if not isinstance(content, list):
        return []
    return [
        block["text"]
        for block in content
        if isinstance(block, dict) and block.get("type") == "text" and isinstance(block.get("text"), str)
    ]


def main() -> None:
    if not BINARY_VALUE:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
    if not CWD_VALUE:
        raise SystemExit("Set CLAUDE_RESUME_FIXTURE_CWD to a fixed empty directory outside this repository")
    binary = Path(BINARY_VALUE).expanduser().resolve()
    binary_sha = sha256(binary.read_bytes())
    if binary_sha != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {binary_sha}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or not version_text.startswith("2.1.293 "):
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    repo_root = ROOT.parents[5]
    cwd = Path(CWD_VALUE).expanduser().resolve()
    try:
        cwd.relative_to(repo_root.parents[2])
    except ValueError:
        pass
    else:
        raise SystemExit("CLAUDE_RESUME_FIXTURE_CWD must be outside the LingXi workspace")
    cwd.mkdir(parents=True, exist_ok=True)
    if any(cwd.iterdir()):
        raise SystemExit(f"Refusing a non-empty cold-resume cwd: {cwd}")

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix="resume-", dir=OUTPUT))
    plugin = run_root / "plugin"
    shutil.copytree(ROOT / "plugin", plugin)
    for name in ("home", "config", "tmp"):
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
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            index = sum(record.get("method") == "POST" for record in request_records) + 1
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
                request_records.append(record)
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401)
                return
            request_path = run_root / "requests" / f"request-{index}.full.raw.json"
            request_path.parent.mkdir(parents=True, exist_ok=True)
            request_path.write_bytes(raw)
            record["raw_request_path"] = str(request_path.relative_to(SESSION_FIXTURE))
            request_records.append(record)
            text = (
                FIXTURE["first_model_text"]
                if active_phase[0] == "initial"
                else FIXTURE["resume_model_text"]
            )
            message_id = f"msg_native_session_append_resume_{index}"
            self.reply(200, sse(text, message_id), "text/event-stream")

    active_phase = [None]
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
        str(binary), "--plugin-dir", str(plugin), "--setting-sources", "project",
        "--strict-mcp-config", "--print", "--output-format", "json", "--tools", "",
        "--max-turns", "1", "--model", FIXTURE["model"], "--debug", "hooks",
    ]
    phases = [
        ("initial", common + ["--session-id", SESSION_ID, FIXTURE["first_prompt"]]),
        ("resume", common + ["--resume", SESSION_ID, FIXTURE["resume_prompt"]]),
    ]
    phase_results = []
    try:
        for phase, argv in phases:
            active_phase[0] = phase
            try:
                completed = subprocess.run(
                    argv, cwd=cwd, env=env, capture_output=True, timeout=45, check=False
                )
                stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
            except subprocess.TimeoutExpired as error:
                stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
            (run_root / f"{phase}.stdout.raw").write_bytes(stdout)
            (run_root / f"{phase}.stderr.raw").write_bytes(stderr)
            phase_requests = [record for record in request_records if record.get("phase") == phase and record.get("method") == "POST"]
            phase_results.append({
                "phase": phase,
                "exit_code": exit_code,
                "timed_out": timed_out,
                "stdout_bytes": len(stdout),
                "stdout_sha256": sha256(stdout),
                "stderr_bytes": len(stderr),
                "stderr_sha256": sha256(stderr),
                "post_count": len(phase_requests),
            })
            if timed_out or exit_code != 0 or len(phase_requests) != 1:
                raise SystemExit(f"Native {phase} phase did not make one successful fixed request: {phase_results[-1]}")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    session_files = list((run_root / "config").glob(f"projects/*/{SESSION_ID}.jsonl"))
    if len(session_files) != 1:
        raise SystemExit(f"expected one Native session JSONL, found {len(session_files)}")
    session_raw = session_files[0].read_bytes()
    (run_root / "session.full.raw.jsonl").write_bytes(session_raw)
    selected_rows = []
    for line in session_raw.splitlines(keepends=True):
        try:
            row = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if row.get("type") != "assistant":
            continue
        if any(text.startswith(REWRITE_PREFIX) for text in row_texts(row)):
            selected_rows.append((line, row))
    if len(selected_rows) != 1:
        raise SystemExit(f"expected one rewritten Native assistant row, found {len(selected_rows)}")
    session_row_raw, selected_row = selected_rows[0]
    (run_root / "rewritten-assistant-session-row.raw.jsonl").write_bytes(session_row_raw)
    session_text = next(text for text in row_texts(selected_row) if text.startswith(REWRITE_PREFIX))
    session_units = utf16_units(session_text)

    resume_request_record = next(
        record for record in request_records
        if record.get("phase") == "resume" and record.get("method") == "POST" and record.get("raw_request_path")
    )
    resume_raw = (SESSION_FIXTURE / resume_request_record["raw_request_path"]).read_bytes()
    resume_text = resume_raw.decode("utf-8")
    resume_value = json.loads(resume_raw)
    selected_paths = []
    for message_index, message in enumerate(resume_value.get("messages", [])):
        if message.get("role") != "assistant" or not isinstance(message.get("content"), list):
            continue
        for block_index, block in enumerate(message["content"]):
            if isinstance(block, dict) and block.get("type") == "text" and isinstance(block.get("text"), str) and block["text"].startswith(REWRITE_PREFIX):
                selected_paths.append(["messages", message_index, "content", block_index, "text"])
    if len(selected_paths) != 1:
        raise SystemExit(f"expected one rewritten assistant message in resume request, found {len(selected_paths)}")
    start, end = path_span(resume_text, selected_paths[0])
    resume_assistant_raw = resume_text[start:end].encode("utf-8")
    (run_root / "resume-assistant-text.raw.json").write_bytes(resume_assistant_raw)
    resume_assistant_text = resume_value["messages"][selected_paths[0][1]]["content"][selected_paths[0][3]]["text"]
    resume_units = utf16_units(resume_assistant_text)
    behavior = (
        "preserved_source_units"
        if resume_units == EXPECTED_SOURCE_UNITS
        else "well_formed_replacement_units"
        if resume_units == EXPECTED_WELL_FORMED_UNITS
        else "other_projection"
    )

    meta_path = run_root / "config" / "debug" / f"{SESSION_ID}.txt"
    debug_lines = meta_path.read_bytes().splitlines(keepends=True) if meta_path.exists() else []
    append_meta = [line for line in debug_lines if b"NATIVE_COLD_RESUME_APPEND=" in line]
    if len(append_meta) != 1:
        raise SystemExit(f"expected one Native session.append observation, found {len(append_meta)}")
    (run_root / "append-meta.raw.txt").write_bytes(append_meta[0])

    view_lines = [line for line in debug_lines if b"NATIVE_COLD_RESUME_SESSION_MESSAGES=" in line]
    resume_answer_units = utf16_units(FIXTURE["resume_model_text"])
    resume_views = []
    for line in view_lines:
        payload = line.split(b"NATIVE_COLD_RESUME_SESSION_MESSAGES=", 1)[1]
        observed = json.loads(payload)
        if observed.get("answer_units") == resume_answer_units:
            resume_views.append((line, observed))
    if len(resume_views) != 1:
        raise SystemExit(f"expected one resumed Native session.messages observation, found {len(resume_views)}")
    view_meta_raw, view_meta = resume_views[0]
    (run_root / "session-messages-meta.raw.txt").write_bytes(view_meta_raw)

    def projection(values: object) -> str:
        if not isinstance(values, list) or len(values) != 1:
            return "missing_or_ambiguous"
        if values[0] == EXPECTED_SOURCE_UNITS:
            return "preserved_source_units"
        if values[0] == EXPECTED_WELL_FORMED_UNITS:
            return "well_formed_replacement_units"
        return "other_projection"

    summary_projection = projection(view_meta.get("summary_units"))
    api_projection = projection(view_meta.get("api_units"))

    checks = {
        "native_session_row_keeps_source_units": session_units == EXPECTED_SOURCE_UNITS,
        "cold_resume_request_contains_rewritten_assistant": len(resume_units) == len(EXPECTED_SOURCE_UNITS),
        "cold_resume_uses_known_projection": behavior in ("preserved_source_units", "well_formed_replacement_units"),
        "cold_resume_summary_view_uses_known_projection": summary_projection in ("preserved_source_units", "well_formed_replacement_units"),
        "cold_resume_api_view_uses_known_projection": api_projection in ("preserved_source_units", "well_formed_replacement_units"),
    }
    receipt = {
        "native_version": version_text,
        "native_sha256": binary_sha,
        "runner_sha256": sha256(Path(__file__).read_bytes()),
        "fixture_sha256": {
            "cases.json": sha256((ROOT / "cases.json").read_bytes()),
            "plugin/hooks/register.js": sha256((ROOT / "plugin/hooks/register.js").read_bytes()),
            "plugin/hooks/hooks.json": sha256((ROOT / "plugin/hooks/hooks.json").read_bytes()),
            "plugin/.claude-plugin/plugin.json": sha256((ROOT / "plugin/.claude-plugin/plugin.json").read_bytes()),
        },
        "fixed_inputs": {
            "session_id": SESSION_ID,
            "cwd": str(cwd),
            "model": FIXTURE["model"],
            "first_prompt": FIXTURE["first_prompt"],
            "resume_prompt": FIXTURE["resume_prompt"],
            "assistant_rewrite_prefix": REWRITE_PREFIX,
            "assistant_rewrite_units": REWRITE_UNITS,
        },
        "loopback_only": True,
        "dummy_key_only": True,
        "phase_results": phase_results,
        "requests": request_records,
        "session_row": {
            "path": "rewritten-assistant-session-row.raw.jsonl",
            "bytes": len(session_row_raw),
            "sha256": sha256(session_row_raw),
            "source_utf16_units": session_units,
        },
        "resume_assistant_text": {
            "path": "resume-assistant-text.raw.json",
            "bytes": len(resume_assistant_raw),
            "sha256": sha256(resume_assistant_raw),
            "utf16_units": resume_units,
            "projection": behavior,
        },
        "append_meta": {"path": "append-meta.raw.txt", "sha256": sha256(append_meta[0])},
        "cold_resume_session_messages": {
            "path": "session-messages-meta.raw.txt",
            "sha256": sha256(view_meta_raw),
            "answer_units": view_meta.get("answer_units"),
            "summary_units": view_meta.get("summary_units"),
            "summary_projection": summary_projection,
            "api_units": view_meta.get("api_units"),
            "api_projection": api_projection,
        },
        "checks": checks,
        "scope": "Two Native public CLI processes; first persists a session.append rewritten assistant row, second uses --resume. The resumed turn.complete hook queries public $.session.messages() and $.session.messages({as: 'api'}). Raw model request bodies remain ignored because they contain Native system prompt/context. This is not a Harness result.",
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(receipt, ensure_ascii=True, indent=2))
    if not all(checks.values()):
        raise SystemExit(1)


if __name__ == "__main__":
    main()
