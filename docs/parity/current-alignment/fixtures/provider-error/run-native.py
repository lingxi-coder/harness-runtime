#!/usr/bin/env python3
"""Capture Native 2.1.293 public HTTP 400 error-text behavior."""
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
DUMMY_KEY = "native-provider-error-fixture-dummy-key"
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
STATUS_FIXTURE = json.loads((ROOT / "status-controls.json").read_text(encoding="utf-8"))


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def utf16_units(text: str) -> list[int]:
    raw = text.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[i:i + 2], "little") for i in range(0, len(raw), 2)]


def strings_in(value: object) -> list[str]:
    if isinstance(value, str):
        return [value]
    if isinstance(value, list):
        return [item for child in value for item in strings_in(child)]
    if isinstance(value, dict):
        return [item for child in value.values() for item in strings_in(child)]
    return []


for fixture_case in FIXTURE["cases"]:
    body = bytes.fromhex(fixture_case["response_body_hex"])
    expected_units = fixture_case["nested_error_message_utf16_units_hex"]
    if expected_units:
        message = json.loads(body)["error"]["message"]
        actual_units_hex = "".join(f"{unit:04x}" for unit in utf16_units(message))
        if actual_units_hex != expected_units:
            raise SystemExit(f"Fixture error.message UTF-16 units mismatch: {fixture_case['id']}")

response_case = next(
    (case for case in FIXTURE["cases"] if case["id"] == STATUS_FIXTURE["response_body_case"]),
    None,
)
if response_case is None:
    raise SystemExit("Status-control response_body_case must name an existing fixed response body")
if STATUS_FIXTURE["max_retries"] != 0:
    raise SystemExit("Status-control fixtures must set max_retries to zero")
CASE_LOOKUP = {case["id"]: case for case in FIXTURE["cases"]}
STATUS_CASES = [
    {
        **case,
        "content_type": response_case["content_type"],
        "response_body_hex": response_case["response_body_hex"],
    }
    for case in STATUS_FIXTURE["cases"]
]
CASE_LOOKUP.update({case["id"]: case for case in STATUS_CASES})


def observable_error_text(stdout: bytes, stderr: bytes, case_id: str, session_id: str) -> dict:
    json_value = None
    json_parse_error = None
    try:
        json_value = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        json_parse_error = type(error).__name__
    candidates = strings_in(json_value) if json_value is not None else []
    selected = None
    selected_source = None
    if isinstance(json_value, dict) and isinstance(json_value.get("result"), str):
        selected = json_value["result"]
        selected_source = "stdout-json.result"
    else:
        marker = (
            "fixture provider error"
            if case_id == "nested-error-message-lone-surrogate"
            else "status code (no body)"
        )
        selected = next((text for text in candidates if marker in text), None)
        if selected is not None:
            selected_source = "stdout-json-string"
        if selected is None:
            stderr_text = stderr.decode("utf-8", errors="replace")
            if marker in stderr_text:
                selected = stderr_text
                selected_source = "stderr-utf8-replacement-view"
    return {
        "stdout_is_json": json_value is not None,
        "stdout_json_parse_error": json_parse_error,
        "native_json_summary": ({
            key: json_value.get(key)
            for key in ("type", "subtype", "is_error", "terminal_reason", "api_error_status", "num_turns")
            if key in json_value
        } if isinstance(json_value, dict) else None),
        "native_output_uuid_format_valid": (
            bool(re.fullmatch(
                r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
                json_value.get("uuid", ""),
            )) if isinstance(json_value, dict) else None
        ),
        "native_output_uuid_matches_fixed_session_id": (
            json_value.get("uuid") == session_id if isinstance(json_value, dict) else None
        ),
        "selected_text_source": selected_source,
        "selected_error_text": selected,
        "selected_error_text_utf16_code_units": utf16_units(selected) if selected is not None else None,
        "selected_text_contains_replacement_character": "�" in selected if selected is not None else None,
        "selected_text_contains_isolated_surrogate": (
            any(0xD800 <= unit <= 0xDFFF for unit in utf16_units(selected))
            if selected is not None else None
        ),
        "selected_text_contains_literal_surrogate_escape": "\\ud800" in selected.lower() if selected is not None else None,
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
active_phase = [None]
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
        request_records.append({
            "case": active_case[0], "phase": active_phase[0],
            "method": "HEAD", "path": self.path, "accepted": accepted,
        })
        self.reply(200 if accepted else 404)

    def do_POST(self) -> None:
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length)
        case = active_case[0]
        accepted = (
            case is not None
            and self.client_address[0] == "127.0.0.1"
            and self.path == "/v1/messages?beta=true"
            and self.headers.get("x-api-key") == DUMMY_KEY
        )
        request_records.append({
            "case": case,
            "phase": active_phase[0],
            "method": "POST",
            "path": self.path,
            "accepted": accepted,
            "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
        })
        if not accepted:
            self.reply(404)
            return
        fixture_case = CASE_LOOKUP[case]
        response_body = bytes.fromhex(fixture_case["response_body_hex"])
        self.reply(
            fixture_case["http_status"],
            response_body,
            fixture_case["content_type"],
        )


server = HTTPServer(("127.0.0.1", 0), Handler)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
case_results = []
persisted_case_results = []
status_control_results = []
try:
    for case in FIXTURE["cases"]:
        active_case[0] = case["id"]
        active_phase[0] = "no-session-persistence"
        case_root = OUTPUT / case["id"]
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
            "--model", FIXTURE["model"], "--session-id", case["session_id"],
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
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_TELEMETRY": "1",
            "DO_NOT_TRACK": "1",
        }
        try:
            completed = subprocess.run(
                argv, cwd=cwd, env=env, capture_output=True, timeout=45, check=False,
            )
            stdout, stderr = completed.stdout, completed.stderr
            exit_code, timed_out = completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout = error.stdout or b""
            stderr = error.stderr or b""
            exit_code, timed_out = None, True
        (case_root / "native.stdout.raw").write_bytes(stdout)
        (case_root / "native.stderr.raw").write_bytes(stderr)
        stdout_is_json = False
        try:
            json.loads(stdout)
            stdout_is_json = True
        except (UnicodeDecodeError, json.JSONDecodeError):
            pass
        if stdout_is_json:
            # Preserve exact stdout bytes. This is not JSON reserialization.
            (case_root / "native.stdout.json.raw").write_bytes(stdout)
        requests = [
            record for record in request_records
            if record["case"] == case["id"] and record["phase"] == active_phase[0]
        ]
        post_requests = [record for record in requests if record["method"] == "POST"]
        response_body = bytes.fromhex(case["response_body_hex"])
        (case_root / "http-response.body.raw").write_bytes(response_body)
        interpretation = observable_error_text(stdout, stderr, case["id"], case["session_id"])
        expected_units_hex = case["expected_result_utf16_units_hex"]
        expected_units = [
            int(expected_units_hex[index:index + 4], 16)
            for index in range(0, len(expected_units_hex), 4)
        ]
        expected_json_summary = {
            "type": "result",
            "subtype": "success",
            "is_error": True,
            "terminal_reason": "api_error",
            "api_error_status": 400,
            "num_turns": 1,
        }
        interpretation["result_utf16_units_match_fixture"] = (
            interpretation["selected_error_text_utf16_code_units"] == expected_units
        )
        interpretation["native_error_json_shape_matches_fixture"] = (
            interpretation["native_json_summary"] == expected_json_summary
        )
        case_results.append({
            "id": case["id"],
            "phase": active_phase[0],
            "session_id": case["session_id"],
            "native_exit_code": exit_code,
            "timed_out": timed_out,
            "request_count": len(post_requests),
            "requests": requests,
            "http_status": case["http_status"],
            "response_body_bytes": len(response_body),
            "response_body_sha256": sha256(response_body),
            "stdout_bytes": len(stdout),
            "stdout_sha256": sha256(stdout),
            "stderr_bytes": len(stderr),
            "stderr_sha256": sha256(stderr),
            "stdout_is_json": stdout_is_json,
            "stdout_json_file": "native.stdout.json.raw" if stdout_is_json else None,
            "stdout_raw_contains_surrogate_escape": b"\\ud800" in stdout.lower(),
            "stdout_raw_contains_utf8_replacement": bytes.fromhex("efbfbd") in stdout,
            "native_cli_error_exit": exit_code == 1,
            "interpretation": interpretation,
        })

    # Repeat the same public CLI request with the same fixed input and loopback
    # response, changing only Native's session-persistence option. Keep the raw
    # assistant JSONL line for transcript inspection; the complete session stays
    # under ignored run-output/ because it can contain generated metadata.
    for case in FIXTURE["cases"]:
        active_case[0] = case["id"]
        active_phase[0] = "session-persistence-enabled"
        case_root = OUTPUT / "persisted" / case["id"]
        config = case_root / "config"
        temp = case_root / "tmp"
        home = case_root / "home"
        for directory in (case_root, config, temp, home):
            directory.mkdir(parents=True, exist_ok=True)
        argv = [
            str(BINARY),
            "--setting-sources", "project", "--strict-mcp-config",
            "--print", "--output-format", "json",
            "--tools", "", "--max-budget-usd", "1",
            "--model", FIXTURE["model"], "--session-id", case["session_id"],
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
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_TELEMETRY": "1",
            "DO_NOT_TRACK": "1",
        }
        try:
            completed = subprocess.run(
                argv, cwd=cwd, env=env, capture_output=True, timeout=45, check=False,
            )
            stdout, stderr = completed.stdout, completed.stderr
            exit_code, timed_out = completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout, stderr = error.stdout or b"", error.stderr or b""
            exit_code, timed_out = None, True

        stdout_path = case_root / "native.stdout.raw"
        stderr_path = case_root / "native.stderr.raw"
        stdout_path.write_bytes(stdout)
        stderr_path.write_bytes(stderr)
        sessions = sorted(config.rglob(f"{case['session_id']}.jsonl"))
        raw_session = sessions[0].read_bytes() if sessions else b""
        assistant_rows = []
        row_parse_errors = []
        for line_number, line in enumerate(raw_session.splitlines(keepends=True), start=1):
            raw_line = line.rstrip(b"\r\n")
            try:
                row = json.loads(raw_line)
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                row_parse_errors.append({"line": line_number, "error": type(error).__name__})
                continue
            message = row.get("message") if isinstance(row, dict) else None
            is_assistant = (
                isinstance(row, dict) and row.get("type") == "assistant"
            ) or (
                isinstance(message, dict) and message.get("role") == "assistant"
            )
            if not is_assistant:
                continue
            content = message.get("content") if isinstance(message, dict) else None
            texts = [
                item.get("text") for item in content
                if isinstance(item, dict) and item.get("type") == "text" and isinstance(item.get("text"), str)
            ] if isinstance(content, list) else []
            assistant_rows.append({
                "line_number": line_number,
                "raw_line": raw_line,
                "text_values": texts,
            })

        case_requests = [
            record for record in request_records
            if record["case"] == case["id"] and record["phase"] == active_phase[0]
        ]
        posts = [record for record in case_requests if record["method"] == "POST"]
        expected_units_hex = case["expected_result_utf16_units_hex"]
        expected_units = [
            int(expected_units_hex[index:index + 4], 16)
            for index in range(0, len(expected_units_hex), 4)
        ]
        matched_rows = [
            row for row in assistant_rows
            if any(utf16_units(text) == expected_units for text in row["text_values"])
        ]
        interpretation = observable_error_text(stdout, stderr, case["id"], case["session_id"])
        interpretation["result_utf16_units_match_fixture"] = (
            interpretation["selected_error_text_utf16_code_units"] == expected_units
        )
        interpretation["native_error_json_shape_matches_fixture"] = (
            interpretation["native_json_summary"] == {
                "type": "result", "subtype": "success", "is_error": True,
                "terminal_reason": "api_error", "api_error_status": 400, "num_turns": 1,
            }
        )
        raw_row_paths = []
        for index, row in enumerate(matched_rows):
            target = ROOT / "native-baseline" / "persisted-assistant-rows" / f"{case['id']}-{index + 1}.jsonl.raw"
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(row["raw_line"] + b"\n")
            raw_row_paths.append({
                "path": str(target.relative_to(ROOT)),
                "line_number": row["line_number"],
                "bytes": len(row["raw_line"] + b"\n"),
                "sha256": sha256(row["raw_line"] + b"\n"),
                "assistant_text_utf16_units": [utf16_units(text) for text in row["text_values"]],
                "contains_api_error_prefix": any(text.startswith("API Error: ") for text in row["text_values"]),
            })
        session_path = sessions[0] if sessions else None
        persisted_case_results.append({
            "id": case["id"],
            "phase": active_phase[0],
            "session_id_fixed_input": case["session_id"],
            "native_exit_code": exit_code,
            "timed_out": timed_out,
            "request_count": len(posts),
            "requests": case_requests,
            "session_file_found": session_path is not None,
            "session_file_bytes": len(raw_session),
            "session_file_sha256": sha256(raw_session) if session_path else None,
            "assistant_row_parse_errors": row_parse_errors,
            "assistant_row_count": len(assistant_rows),
            "rows_matching_exact_cli_result_units": len(matched_rows),
            "matching_raw_assistant_rows": raw_row_paths,
            "all_matching_rows_have_api_error_prefix": all(
                item["contains_api_error_prefix"] for item in raw_row_paths
            ) if raw_row_paths else None,
            "stdout_bytes": len(stdout),
            "stdout_sha256": sha256(stdout),
            "stdout_result_utf16_units_match_fixture": interpretation["result_utf16_units_match_fixture"],
            "stdout_json_shape_matches_fixture": interpretation["native_error_json_shape_matches_fixture"],
            "stderr_bytes": len(stderr),
            "stderr_sha256": sha256(stderr),
            "session_file_path_relative_to_config": (
                str(session_path.relative_to(config)) if session_path else None
            ),
        })

    # Small status-only controls reuse one already captured body and the same
    # loopback stub. Retries are explicitly disabled; only public CLI
    # presentation is measured here, not provider-specific request headers.
    for case in STATUS_CASES:
        active_case[0] = case["id"]
        active_phase[0] = "http-status-control"
        case_root = OUTPUT / "status-controls" / case["id"]
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
            "--model", STATUS_FIXTURE["model"], "--session-id", case["session_id"],
            STATUS_FIXTURE["prompt"],
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
            "ANTHROPIC_MAX_RETRIES": str(STATUS_FIXTURE["max_retries"]),
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_TELEMETRY": "1",
            "DO_NOT_TRACK": "1",
        }
        try:
            completed = subprocess.run(
                argv, cwd=cwd, env=env, capture_output=True, timeout=45, check=False,
            )
            stdout, stderr = completed.stdout, completed.stderr
            exit_code, timed_out = completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout, stderr = error.stdout or b"", error.stderr or b""
            exit_code, timed_out = None, True
        (case_root / "native.stdout.raw").write_bytes(stdout)
        (case_root / "native.stderr.raw").write_bytes(stderr)
        interpretation = observable_error_text(stdout, stderr, case["id"], case["session_id"])
        requests = [
            record for record in request_records
            if record["case"] == case["id"] and record["phase"] == active_phase[0]
        ]
        posts = [record for record in requests if record["method"] == "POST"]
        baseline_dir = ROOT / "native-baseline" / "status-controls"
        baseline_dir.mkdir(parents=True, exist_ok=True)
        native_stdout = baseline_dir / f"{case['id']}.stdout.raw"
        native_stderr = baseline_dir / f"{case['id']}.stderr.raw"
        native_stdout.write_bytes(stdout)
        native_stderr.write_bytes(stderr)
        status_control_results.append({
            "id": case["id"],
            "phase": active_phase[0],
            "http_status": case["http_status"],
            "expected_native_posts": case["expected_native_posts"],
            "body_case": STATUS_FIXTURE["response_body_case"],
            "response_body_bytes": len(bytes.fromhex(case["response_body_hex"])),
            "response_body_sha256": sha256(bytes.fromhex(case["response_body_hex"])),
            "max_retries": STATUS_FIXTURE["max_retries"],
            "request_count": len(posts),
            "requests": requests,
            "native_exit_code": exit_code,
            "timed_out": timed_out,
            "native_json_summary": interpretation["native_json_summary"],
            "result_utf16_units": interpretation["selected_error_text_utf16_code_units"],
            "result_has_api_error_prefix": (
                interpretation["selected_error_text"].startswith("API Error: ")
                if interpretation["selected_error_text"] is not None else None
            ),
            "result_contains_replacement_character": interpretation["selected_text_contains_replacement_character"],
            "stdout_path": str(native_stdout.relative_to(ROOT)),
            "stdout_bytes": len(stdout),
            "stdout_sha256": sha256(stdout),
            "stderr_path": str(native_stderr.relative_to(ROOT)),
            "stderr_bytes": len(stderr),
            "stderr_sha256": sha256(stderr),
        })
finally:
    server.shutdown()
    server.server_close()
    thread.join(timeout=2)

receipt = {
    "native_version": FIXTURE["native_version"],
    "native_sha256": actual_binary_sha,
    "network_boundary": "127.0.0.1-only; dummy key; no upstream relay",
    "fixed_working_directory": FIXTURE["cwd_relative"],
    "fixed_model": FIXTURE["model"],
    "budget_cap_usd": 1,
    "cases": case_results,
    "persisted_session_cases": persisted_case_results,
    "status_controls": status_control_results,
    "one_post_per_case": all(case["request_count"] == 1 for case in case_results),
    "one_post_per_persisted_case": all(case["request_count"] == 1 for case in persisted_case_results),
    "all_persisted_cases_have_session_file": all(case["session_file_found"] for case in persisted_case_results),
    "all_persisted_cases_have_exact_assistant_row": all(
        case["rows_matching_exact_cli_result_units"] == 1 for case in persisted_case_results
    ),
    "all_persisted_stdout_and_assistant_rows_match_fixture": all(
        case["stdout_result_utf16_units_match_fixture"]
        and case["stdout_json_shape_matches_fixture"]
        and case["rows_matching_exact_cli_result_units"] == 1
        and case["all_matching_rows_have_api_error_prefix"] is True
        for case in persisted_case_results
    ),
    "all_post_requests_were_local_dummy_key": all(
        record.get("accepted") and record.get("dummy_key_matched")
        for record in request_records if record["method"] == "POST"
    ),
    "no_retries_observed": all(case["request_count"] == 1 for case in case_results),
    "all_error_exits_are_nonzero": all(case["native_cli_error_exit"] for case in case_results),
    "all_result_utf16_units_match_fixture": all(
        case["interpretation"]["result_utf16_units_match_fixture"] for case in case_results
    ),
    "all_native_error_json_shapes_match_fixture": all(
        case["interpretation"]["native_error_json_shape_matches_fixture"] for case in case_results
    ),
    "status_controls_each_one_post_no_retry": all(
        case["request_count"] == 1 and case["max_retries"] == 0 for case in status_control_results
    ),
    "status_controls_finite_capture": (
        len(status_control_results) == len(STATUS_CASES)
        and all(case["request_count"] >= 1 and (case["timed_out"] or case["native_exit_code"] is not None)
                for case in status_control_results)
    ),
    "status_controls_known_counts_match": all(
        case["request_count"] == case["expected_native_posts"]
        and case["native_exit_code"] is not None
        and case["stdout_bytes"] > 0
        for case in status_control_results if case["expected_native_posts"] is not None
    ),
    "raw_stdout_stderr_preserved": True,
    "raw_json_output_is_unmodified_stdout_copy": all(
        not case["stdout_is_json"] or case["stdout_json_file"] == "native.stdout.json.raw"
        for case in case_results
    ),
    "scope": "Native public CLI HTTP 400 text selection only; Harness/ProviderErrorText delivery remains a separate gate",
    "persisted_assistant_rows_scope": "Same fixed prompt/model/session IDs and loopback response bodies with persistence enabled; only exact assistant JSONL rows matching the CLI result UTF-16 units are checked in. Full isolated session files remain under ignored run-output/.",
}
(OUTPUT / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n")
persisted_receipt = {
    "native_version": FIXTURE["native_version"],
    "native_sha256": actual_binary_sha,
    "public_cli_mode": "--print --output-format json; session persistence enabled",
    "fixed_inputs_reference": "../cases.json (prompt, model, session IDs, status and exact response body)",
    "network_boundary": "127.0.0.1-only; fixed dummy key; no upstream relay",
    "comparison_boundary": "Each checked-in original JSONL assistant row matched the fixed CLI result UTF-16 units. The row itself is retained byte-for-byte, but Native-generated UUIDs/timestamps and the fixed per-run cwd/session path are separately constrained and are not treated as cross-run byte-equal fields.",
    "cases": [
        {
            "case": case["id"],
            "request_count": case["request_count"],
            "native_exit_code": case["native_exit_code"],
            "session_file_found": case["session_file_found"],
            "exact_result_row_count": case["rows_matching_exact_cli_result_units"],
            "raw_assistant_rows": case["matching_raw_assistant_rows"],
            "assistant_text_has_api_error_prefix": case["all_matching_rows_have_api_error_prefix"],
            "stdout_result_units_match_fixed_fixture": case["stdout_result_utf16_units_match_fixture"],
        }
        for case in persisted_case_results
    ],
    "all_cases_pass": receipt["all_persisted_stdout_and_assistant_rows_match_fixture"],
}
baseline_receipt = ROOT / "native-baseline" / "persisted-assistant-rows" / "receipt.json"
baseline_receipt.parent.mkdir(parents=True, exist_ok=True)
baseline_receipt.write_text(json.dumps(persisted_receipt, ensure_ascii=True, indent=2) + "\n")
status_receipt = {
    "native_version": FIXTURE["native_version"],
    "native_sha256": actual_binary_sha,
    "public_cli_mode": "--print --output-format json --no-session-persistence",
    "prompt_and_model_fixed": True,
    "response_body_case": STATUS_FIXTURE["response_body_case"],
    "max_retries": STATUS_FIXTURE["max_retries"],
    "network_boundary": "127.0.0.1-only; fixed dummy key; no upstream relay",
    "observed_results": status_control_results,
    "all_cases_one_post": receipt["status_controls_each_one_post_no_retry"],
    "bounded_controls_completed": receipt["status_controls_finite_capture"],
    "known_request_counts_match_fixture": receipt["status_controls_known_counts_match"],
    "comparison_boundary": "These are Native presentation controls only. Status-specific result text is recorded as observed UTF-16 units and is not used to claim a Harness pass.",
}
status_receipt_path = ROOT / "native-baseline" / "status-controls" / "receipt.json"
status_receipt_path.parent.mkdir(parents=True, exist_ok=True)
status_receipt_path.write_text(json.dumps(status_receipt, ensure_ascii=True, indent=2) + "\n")
print(json.dumps(receipt, ensure_ascii=True, indent=2))
if not (
    receipt["one_post_per_case"]
    and receipt["all_post_requests_were_local_dummy_key"]
    and receipt["all_error_exits_are_nonzero"]
    and receipt["all_result_utf16_units_match_fixture"]
    and receipt["all_native_error_json_shapes_match_fixture"]
    and receipt["one_post_per_persisted_case"]
    and receipt["all_persisted_cases_have_session_file"]
    and receipt["all_persisted_stdout_and_assistant_rows_match_fixture"]
    and receipt["status_controls_finite_capture"]
    and receipt["status_controls_known_counts_match"]
):
    raise SystemExit(1)
