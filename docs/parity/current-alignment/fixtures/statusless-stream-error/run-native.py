#!/usr/bin/env python3
"""Capture public Native CLI behavior for statusless Anthropic SSE errors."""
from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
OUTPUT = ROOT / "run-output"
NATIVE_PATH = os.environ.get("CLAUDE_NATIVE_BINARY")
if not NATIVE_PATH:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
BINARY = Path(NATIVE_PATH).expanduser().resolve()
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-statusless-stream-error-fixture-dummy-key"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utf16_units(text: str) -> list[int]:
    raw = text.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index:index + 2], "little")
            for index in range(0, len(raw), 2)]


def strings_in(value: object) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        return [item for child in value for item in strings_in(child)]
    if isinstance(value, dict):
        return [item for child in value.values() for item in strings_in(child)]
    return []


def parse_sse_events(body: bytes) -> list[dict]:
    events = []
    event_name = None
    data_lines = []
    for line in body.splitlines() + [b""]:
        if not line:
            if event_name is not None or data_lines:
                payload = None
                if data_lines:
                    try:
                        payload = json.loads(b"\n".join(data_lines))
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        pass
                events.append({"event": event_name, "data": payload})
            event_name = None
            data_lines = []
        elif line.startswith(b"event:"):
            event_name = line[len(b"event:"):].strip().decode("ascii", errors="replace")
        elif line.startswith(b"data:"):
            data_lines.append(line[len(b"data:"):].lstrip(b" "))
    return events


for fixture_case in FIXTURE["cases"]:
    body = bytes.fromhex(fixture_case["response_body_hex"])
    events = parse_sse_events(body)
    event_sequence = [item["event"] for item in events]
    expected_sequence = fixture_case.get("expected_event_sequence", [fixture_case["event_type"]])
    if event_sequence != expected_sequence:
        raise SystemExit(f"SSE event sequence mismatch: {fixture_case['id']}: {event_sequence!r}")
    last_event = events[-1] if events else {}
    event, payload = last_event.get("event"), last_event.get("data")
    if event != fixture_case["event_type"]:
        raise SystemExit(f"SSE event mismatch: {fixture_case['id']}: {event!r}")
    if payload is None or payload.get("type") != fixture_case["data_type"]:
        raise SystemExit(f"SSE data.type mismatch: {fixture_case['id']}")
    error = payload.get("error")
    if not isinstance(error, dict) or error.get("type") != fixture_case["error_type"]:
        raise SystemExit(f"SSE nested error.type mismatch: {fixture_case['id']}")
    message_units_hex = "".join(f"{unit:04x}" for unit in utf16_units(error["message"]))
    if message_units_hex != fixture_case["error_message_utf16_units_hex"]:
        raise SystemExit(f"SSE error.message UTF-16 mismatch: {fixture_case['id']}")
    if "pre_error_text" in fixture_case:
        delta = next((item.get("data", {}) for item in events
                      if item.get("event") == "content_block_delta"), None)
        if not isinstance(delta, dict) or delta.get("delta", {}).get("text") != fixture_case["pre_error_text"]:
            raise SystemExit(f"SSE pre-error text mismatch: {fixture_case['id']}")


def summarize_output(stdout: bytes, stderr: bytes, marker: str) -> dict:
    parsed = None
    parse_error = None
    try:
        parsed = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        parse_error = type(error).__name__
    selected = None
    source = None
    if isinstance(parsed, dict) and isinstance(parsed.get("result"), str):
        selected = parsed["result"]
        source = "stdout-json.result"
    else:
        candidates = strings_in(parsed) if parsed is not None else []
        selected = next((text for text in candidates if marker in text), None)
        if selected is not None:
            source = "stdout-json-string"
        else:
            stderr_text = stderr.decode("utf-8", errors="replace")
            if marker in stderr_text:
                selected = stderr_text
                source = "stderr-utf8-replacement-view"
    units = utf16_units(selected) if selected is not None else None
    result_summary = ({
        key: parsed.get(key)
        for key in ("type", "subtype", "is_error", "terminal_reason", "api_error_status", "num_turns")
        if key in parsed
    } if isinstance(parsed, dict) else None)
    return {
        "stdout_is_json": parsed is not None,
        "stdout_json_parse_error": parse_error,
        "native_result_summary": result_summary,
        "selected_text_source": source,
        "selected_result_text": selected,
        "selected_result_utf16_code_units": units,
        "selected_result_contains_isolated_surrogate": (
            any(0xD800 <= unit <= 0xDFFF for unit in units) if units is not None else None
        ),
        "selected_result_contains_literal_surrogate_escape": (
            "\\ud800" in selected.lower() if selected is not None else None
        ),
        "selected_result_contains_replacement_character": (
            "�" in selected if selected is not None else None
        ),
        "raw_stdout_contains_one_backslash_u_d800": b"\\ud800" in stdout.lower(),
        "raw_stdout_contains_two_backslash_u_d800": b"\\\\ud800" in stdout.lower(),
    }


actual_binary_sha = sha256(BINARY.read_bytes())
if actual_binary_sha != EXPECTED_SHA256:
    raise SystemExit(f"Native SHA mismatch: {actual_binary_sha}")
if OUTPUT.exists():
    shutil.rmtree(OUTPUT)
OUTPUT.mkdir(parents=True)

run_root = OUTPUT / "run"
cwd = run_root / "cwd"
cwd.mkdir(parents=True)
active_case = [None]
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
            self.wfile.flush()

    def do_HEAD(self) -> None:
        accepted = self.client_address[0] == "127.0.0.1" and self.path == "/api/hello"
        request_records.append({
            "case": active_case[0], "method": "HEAD", "path": self.path, "accepted": accepted,
        })
        self.reply(200 if accepted else 404)

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length)
        case_id = active_case[0]
        accepted = (
            case_id is not None
            and self.client_address[0] == "127.0.0.1"
            and self.path == "/v1/messages?beta=true"
            and self.headers.get("x-api-key") == DUMMY_KEY
        )
        try:
            request_json = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            request_json = None
        request_records.append({
            "case": case_id,
            "method": "POST",
            "path": self.path,
            "accepted": accepted,
            "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
            "request_bytes": len(raw),
            "request_sha256": sha256(raw),
            "request_stream": request_json.get("stream") if isinstance(request_json, dict) else None,
            "request_model": request_json.get("model") if isinstance(request_json, dict) else None,
        })
        if not accepted:
            self.reply(404)
            return
        fixture_case = next(item for item in FIXTURE["cases"] if item["id"] == case_id)
        self.reply(
            fixture_case["http_status"],
            bytes.fromhex(fixture_case["response_body_hex"]),
            fixture_case["content_type"],
        )


server = HTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
case_results = []
try:
    for fixture_case in FIXTURE["cases"]:
        case_id = fixture_case["id"]
        active_case[0] = case_id
        case_root = OUTPUT / case_id
        config = case_root / "config"
        temp = case_root / "tmp"
        home = case_root / "home"
        for directory in (case_root, config, temp, home):
            directory.mkdir(parents=True, exist_ok=True)
        argv = [
            str(BINARY),
            "--setting-sources", "project", "--strict-mcp-config",
            "--print", "--output-format", "json", "--no-session-persistence",
            "--tools", "", "--max-budget-usd", "1",
            "--model", FIXTURE["model"], "--session-id", fixture_case["session_id"],
            FIXTURE["prompt"],
        ]
        env = {
            "PATH": "/usr/bin:/bin",
            "HOME": str(home),
            "TMPDIR": str(temp),
            "LANG": "C.UTF-8",
            "TZ": "UTC",
            "TERM": "dumb",
            "CLAUDE_CONFIG_DIR": str(config),
            "ANTHROPIC_API_KEY": DUMMY_KEY,
            "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
            "ANTHROPIC_MAX_RETRIES": str(FIXTURE["max_retries"]),
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_TELEMETRY": "1",
            "DO_NOT_TRACK": "1",
        }
        try:
            completed = subprocess.run(
                argv, cwd=cwd, env=env, capture_output=True, timeout=35, check=False,
            )
            stdout, stderr = completed.stdout, completed.stderr
            exit_code, timed_out = completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout = error.stdout or b""
            stderr = error.stderr or b""
            exit_code, timed_out = None, True

        body = bytes.fromhex(fixture_case["response_body_hex"])
        response_path = case_root / "http-response.sse.raw"
        stdout_path = case_root / "native.stdout.raw"
        stderr_path = case_root / "native.stderr.raw"
        response_path.write_bytes(body)
        stdout_path.write_bytes(stdout)
        stderr_path.write_bytes(stderr)
        stdout_is_json = False
        try:
            json.loads(stdout)
            stdout_is_json = True
        except (UnicodeDecodeError, json.JSONDecodeError):
            pass
        if stdout_is_json:
            # Preserve the exact CLI output bytes, without JSON reserialization.
            (case_root / "native.stdout.json.raw").write_bytes(stdout)
        case_requests = [record for record in request_records if record["case"] == case_id]
        posts = [record for record in case_requests if record["method"] == "POST"]
        interpretation = summarize_output(stdout, stderr, "native ")
        selected_units = interpretation["selected_result_utf16_code_units"]
        if fixture_case.get("pre_error_text") is not None:
            interpretation["selected_result_contains_pre_error_text"] = (
                fixture_case["pre_error_text"]
                in (interpretation["selected_result_text"] or "")
            )
        expected_message_units = [
            int(fixture_case["error_message_utf16_units_hex"][index:index + 4], 16)
            for index in range(0, len(fixture_case["error_message_utf16_units_hex"]), 4)
        ]
        interpretation["selected_result_contains_fixture_message_units"] = (
            selected_units is not None and all(
                selected_units[index:index + len(expected_message_units)] == expected_message_units
                for index in range(max(0, len(selected_units) - len(expected_message_units) + 1))
            )
        )
        case_results.append({
            "id": case_id,
            "session_id": fixture_case["session_id"],
            "native_exit_code": exit_code,
            "timed_out": timed_out,
            "retry_setting": {"ANTHROPIC_MAX_RETRIES": FIXTURE["max_retries"]},
            "request_count": len(posts),
            "requests": case_requests,
            "request_stream_values": [record["request_stream"] for record in posts],
            "first_post_stream": posts[0]["request_stream"] if posts else None,
            "last_post_stream": posts[-1]["request_stream"] if posts else None,
            "http_status": fixture_case["http_status"],
            "event": fixture_case["event_type"],
            "event_sequence": [item["event"] for item in parse_sse_events(body)],
            "data_type": fixture_case["data_type"],
            "nested_error_type": fixture_case["error_type"],
            "error_message_utf16_units_hex": fixture_case["error_message_utf16_units_hex"],
            "pre_error_text": fixture_case.get("pre_error_text"),
            "response_body_bytes": len(body),
            "response_body_sha256": sha256(body),
            "response_file": f"{case_id}/http-response.sse.raw",
            "stdout_bytes": len(stdout),
            "stdout_sha256": sha256(stdout),
            "stdout_file": f"{case_id}/native.stdout.raw",
            "stdout_json_file": f"{case_id}/native.stdout.json.raw" if stdout_is_json else None,
            "stderr_bytes": len(stderr),
            "stderr_sha256": sha256(stderr),
            "stderr_file": f"{case_id}/native.stderr.raw",
            "raw_stdout_contains_one_backslash_u_d800": b"\\ud800" in stdout.lower(),
            "raw_stdout_contains_two_backslash_u_d800": b"\\\\ud800" in stdout.lower(),
            "interpretation": interpretation,
        })
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)

actual_binary_sha = sha256(BINARY.read_bytes())
receipt = {
    "native_version": FIXTURE["native_version"],
    "native_sha256": actual_binary_sha,
    "fixture_runner_sha256": sha256(Path(__file__).read_bytes()),
    "fixture_cases_sha256": sha256((ROOT / "cases.json").read_bytes()),
    "network_boundary": "127.0.0.1-only; dummy key; no upstream relay",
    "retry_setting": {"ANTHROPIC_MAX_RETRIES": FIXTURE["max_retries"]},
    "fixed_working_directory": FIXTURE["cwd_relative"],
    "fixed_model": FIXTURE["model"],
    "budget_cap_usd": 1,
    "cases": case_results,
    "one_post_per_case": all(case["request_count"] == 1 for case in case_results),
    "first_post_is_streaming_for_every_case": all(case["first_post_stream"] is True for case in case_results),
    "nonstream_fallback_cases": [case["id"] for case in case_results
                                 if case["request_stream_values"] == [True, False]],
    "all_request_sequences_bounded": all(
        case["request_stream_values"] in ([True], [True, False]) for case in case_results
    ),
    "all_posts_were_local_dummy_key": all(
        record.get("accepted") and record.get("dummy_key_matched")
        for record in request_records if record["method"] == "POST"
    ),
    "all_processes_completed_without_timeout": all(not case["timed_out"] for case in case_results),
    "raw_sse_stdout_stderr_preserved": True,
    "scope": "Native public CLI statusless Anthropic SSE error result only; SDK intermediate source and integrated Harness/Host delivery remain separate gates",
}
(OUTPUT / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n")
print(json.dumps(receipt, ensure_ascii=True, indent=2))
if not (
    receipt["all_request_sequences_bounded"]
    and receipt["first_post_is_streaming_for_every_case"]
    and receipt["all_posts_were_local_dummy_key"]
    and receipt["all_processes_completed_without_timeout"]
):
    raise SystemExit(1)
