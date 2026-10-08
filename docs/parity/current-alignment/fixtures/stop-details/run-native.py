#!/usr/bin/env python3
"""Capture Native 2.1.293 refusal stop_details at the public CLI boundary."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
CASE_ID = os.environ.get("CLAUDE_STOP_DETAILS_CASE", "refusal-cyber")
CASE_SPEC = next((item for item in FIXTURE["cases"] if item["id"] == CASE_ID), None)
if CASE_SPEC is None:
    raise SystemExit(f"Unknown CLAUDE_STOP_DETAILS_CASE: {CASE_ID}")
CASE = {**FIXTURE, **CASE_SPEC}
CASE["stop_details"] = {"type": "refusal"}
if "category" in CASE_SPEC:
    CASE["stop_details"]["category"] = CASE_SPEC["category"]
CASE["stop_details"]["explanation_prefix"] = FIXTURE["explanation_prefix"]
CASE["stop_details"]["explanation_suffix"] = FIXTURE["explanation_suffix"]
OUTPUT = ROOT / "run-output" / CASE_ID
NATIVE_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
EXPECTED_SHA256 = "4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e"
DUMMY_KEY = "native-stop-details-fixture-dummy-key"


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def utf16_units(text: str) -> list[int]:
    raw = text.encode("utf-16-le", errors="surrogatepass")
    return [int.from_bytes(raw[index:index + 2], "little") for index in range(0, len(raw), 2)]


def event(name: str, payload: dict) -> bytes:
    # ensure_ascii retains the lone D800 as a JSON escape in the raw SSE bytes.
    encoded = json.dumps(payload, ensure_ascii=True, separators=(",", ":")).encode("ascii")
    return b"event: " + name.encode("ascii") + b"\ndata: " + encoded + b"\n\n"


EXPLANATION = (
    CASE["explanation_prefix"]
    + "\ud800"
    + CASE["stop_details"]["explanation_suffix"]
)
EXPECTED_EXPLANATION_UNITS = [
    int(CASE["explanation_utf16_units_hex"][index:index + 4], 16)
    for index in range(0, len(CASE["explanation_utf16_units_hex"]), 4)
]
if utf16_units(EXPLANATION) != EXPECTED_EXPLANATION_UNITS:
    raise SystemExit("Fixed explanation UTF-16 units do not match cases.json")

SSE_BODY = b"".join([
    event("message_start", {
        "type": "message_start",
        "message": {
            "id": CASE["response_id"],
            "type": "message",
            "role": "assistant",
            "model": CASE["model"],
            "content": [],
            "stop_reason": None,
            "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 0},
        },
    }),
    event("content_block_start", {
        "type": "content_block_start",
        "index": 0,
        "content_block": {"type": "text", "text": ""},
    }),
    event("content_block_delta", {
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": CASE["visible_text"]},
    }),
    event("content_block_stop", {"type": "content_block_stop", "index": 0}),
    event("message_delta", {
        "type": "message_delta",
        "delta": {
            "stop_reason": CASE["stop_reason"],
            "stop_sequence": None,
            "stop_details": {
                **{key: value for key, value in CASE["stop_details"].items()
                   if key not in ("explanation_prefix", "explanation_suffix")},
                "explanation": EXPLANATION,
            },
        },
        "usage": {"output_tokens": 4},
    }),
    event("message_stop", {"type": "message_stop"}),
])
MESSAGE_BODY = json.dumps({
    "id": CASE["response_id"],
    "type": "message",
    "role": "assistant",
    "model": CASE["model"],
    "content": [{"type": "text", "text": CASE["visible_text"]}],
    "stop_reason": CASE["stop_reason"],
    "stop_details": {
        **{key: value for key, value in CASE["stop_details"].items()
           if key not in ("explanation_prefix", "explanation_suffix")},
        "explanation": EXPLANATION,
    },
    "usage": {"input_tokens": 1, "output_tokens": 4},
}, ensure_ascii=True, separators=(",", ":")).encode("ascii")


def walk(value: object, path: str = "$") -> list[dict]:
    found = []
    if isinstance(value, dict):
        if isinstance(value.get("stop_details"), dict):
            details = value["stop_details"]
            explanation = details.get("explanation")
            found.append({
                "path": path + ".stop_details",
                "category_present": "category" in details,
                "category": details.get("category"),
                "type": details.get("type"),
                "explanation_utf16_units": utf16_units(explanation) if isinstance(explanation, str) else None,
            })
        for key, child in value.items():
            found.extend(walk(child, f"{path}.{key}"))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.extend(walk(child, f"{path}[{index}]"))
    return found


def main() -> None:
    if not NATIVE_VALUE:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
    binary = Path(NATIVE_VALUE).expanduser().resolve()
    binary_hash = sha256(binary.read_bytes())
    if binary_hash != EXPECTED_SHA256:
        raise SystemExit(f"Native SHA mismatch: {binary_hash}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or not version_text.startswith("2.1.293 "):
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    if OUTPUT.exists():
        shutil.rmtree(OUTPUT)
    OUTPUT.mkdir(parents=True)
    run_root = OUTPUT / "run"
    cwd = run_root / "cwd"
    config = run_root / "config"
    home = run_root / "home"
    temp = run_root / "tmp"
    for directory in (cwd, config, home, temp):
        directory.mkdir(parents=True)

    records: list[dict] = []
    response_bodies: list[tuple[str, bytes]] = []

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
            records.append({"method": "HEAD", "path": self.path, "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            request_json = None
            try:
                request_json = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                pass
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            records.append({
                "method": "POST",
                "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
                "stream_requested": request_json.get("stream") if isinstance(request_json, dict) else None,
                "accept_header": self.headers.get("accept"),
            })
            if accepted:
                stream_requested = isinstance(request_json, dict) and request_json.get("stream") is True
                response_body = SSE_BODY if stream_requested else MESSAGE_BODY
                content_type = "text/event-stream" if stream_requested else "application/json"
                response_bodies.append((content_type, response_body))
                records[-1].update({
                    "response_content_type": content_type,
                    "response_bytes": len(response_body),
                    "response_sha256": sha256(response_body),
                })
                self.reply(200, response_body, content_type)
            else:
                self.reply(404, b"{}")

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    argv = [
        str(binary), "--setting-sources", "project", "--strict-mcp-config",
        "--print", "--output-format", "json", "--tools", "", "--max-turns", "1",
        "--max-budget-usd", "1", "--model", CASE["model"], "--session-id", CASE["session_id"],
        CASE["prompt"],
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
        try:
            completed = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=45, check=False)
            stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    case_out = OUTPUT / "native"
    case_out.mkdir()
    (case_out / "native.stdout.raw").write_bytes(stdout)
    (case_out / "native.stderr.raw").write_bytes(stderr)
    response_artifacts: dict[tuple[str, str], str] = {}
    for content_type, raw_response in response_bodies:
        response_key = (content_type, sha256(raw_response))
        if response_key not in response_artifacts:
            suffix = "sse" if content_type == "text/event-stream" else "json"
            filename = f"native-response-{len(response_artifacts) + 1:02d}.{suffix}.raw"
            (case_out / filename).write_bytes(raw_response)
            response_artifacts[response_key] = filename
    for record in records:
        if record.get("method") == "POST" and record.get("accepted"):
            key = (record["response_content_type"], record["response_sha256"])
            record["response_artifact"] = response_artifacts[key]

    stdout_json = None
    stdout_parse_error = None
    try:
        stdout_json = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        stdout_parse_error = type(error).__name__

    sessions = sorted(run_root.rglob(f"{CASE['session_id']}.jsonl"))
    raw_session = sessions[0].read_bytes() if sessions else b""
    (case_out / "session.jsonl.raw").write_bytes(raw_session)
    session_values = []
    session_parse_errors = []
    stop_details_raw_lines = []
    for line_number, line in enumerate(raw_session.splitlines(keepends=True), 1):
        try:
            value = json.loads(line)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            session_parse_errors.append({"line": line_number, "error": type(error).__name__})
            continue
        line_paths = walk(value)
        session_values.append({"line": line_number, "type": value.get("type"), "paths": line_paths})
        if line_paths:
            stop_details_raw_lines.append(line)
    stop_details_raw_rows = b"".join(stop_details_raw_lines)
    (case_out / "session-stop-details-rows.raw.jsonl").write_bytes(stop_details_raw_rows)

    stdout_details = walk(stdout_json) if stdout_json is not None else []
    stdout_result = stdout_json.get("result") if isinstance(stdout_json, dict) else None
    response_summaries = [
        {
            "content_type": content_type,
            "bytes": len(raw_response),
            "sha256": sha256(raw_response),
            "contains_lone_surrogate_escape": b"\\ud800" in raw_response.lower(),
            "artifact": response_artifacts[(content_type, sha256(raw_response))],
        }
        for content_type, raw_response in response_bodies
    ]
    explanation_summary = {
        "fixed_explanation_utf16_units": EXPECTED_EXPLANATION_UNITS,
        "native_cli_result_utf16_units": utf16_units(stdout_result) if isinstance(stdout_result, str) else None,
        "native_cli_result_contains_isolated_surrogate": (
            any(0xD800 <= unit <= 0xDFFF for unit in utf16_units(stdout_result))
            if isinstance(stdout_result, str) else None
        ),
        "native_cli_result_contains_replacement_character": "�" in stdout_result if isinstance(stdout_result, str) else None,
        "native_cli_result_contains_literal_surrogate_escape": "\\ud800" in stdout_result.lower() if isinstance(stdout_result, str) else None,
        "native_cli_result_contains_explanation_prefix": CASE["explanation_prefix"] in stdout_result if isinstance(stdout_result, str) else None,
        "native_stdout_stop_details": stdout_details,
        "native_session_stop_details": [item for item in session_values if item["paths"]],
        "raw_session_stop_details_rows_bytes": len(stop_details_raw_rows),
        "raw_session_stop_details_rows_sha256": sha256(stop_details_raw_rows),
        "stdout_raw_contains_escape": b"\\ud800" in stdout.lower(),
        "stdout_raw_contains_replacement_utf8": b"\xef\xbf\xbd" in stdout,
        "session_raw_contains_escape": b"\\ud800" in raw_session.lower(),
        "session_raw_contains_replacement_utf8": b"\xef\xbf\xbd" in raw_session,
        "session_raw_contains_explanation_prefix": CASE["explanation_prefix"].encode("ascii") in raw_session,
    }
    receipt = {
        "native_version": version_text,
        "native_sha256": binary_hash,
        "public_entrypoint": "pinned Native CLI --print --output-format json; session persistence enabled",
        "case_id": CASE_ID,
        "fixed_inputs": {key: CASE[key] for key in ("model", "prompt", "session_id", "response_id", "visible_text", "stop_reason")},
        "response_fixture": {
            "stop_details": {key: value for key, value in CASE["stop_details"].items()},
            "explanation_utf16_units": EXPECTED_EXPLANATION_UNITS,
            "responses_served_to_native": response_summaries,
        },
        "network_boundary": "127.0.0.1-only HTTP stub; fixed dummy API key; no upstream relay",
        "requests": records,
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "stdout_bytes": len(stdout),
        "stdout_sha256": sha256(stdout),
        "stdout_json_parse_error": stdout_parse_error,
        "stdout_json_summary": ({key: stdout_json.get(key) for key in ("type", "subtype", "is_error", "stop_reason", "terminal_reason", "api_error_status", "session_id", "num_turns") if key in stdout_json} if isinstance(stdout_json, dict) else None),
        "stderr_bytes": len(stderr),
        "stderr_sha256": sha256(stderr),
        "session_file_found": bool(sessions),
        "session_jsonl_bytes": len(raw_session),
        "session_jsonl_sha256": sha256(raw_session) if sessions else None,
        "session_parse_errors": session_parse_errors,
        "parsed_session_lines": session_values,
        "stop_details_observation": explanation_summary,
        "raw_artifact_paths": [
            f"native-baseline/{CASE_ID}/native.stdout.raw",
            f"native-baseline/{CASE_ID}/native.stderr.raw",
            f"native-baseline/{CASE_ID}/native-response-01.sse.raw",
            f"native-baseline/{CASE_ID}/session-stop-details-rows.raw.jsonl",
        ],
        "scope": "Native 2.1.293 public CLI behavior for a refusal stop_details.explanation; does not establish Harness parity or all stop-details values",
    }
    (case_out / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "exit_code": exit_code,
        "requests": records,
        "stdout_bytes": len(stdout),
        "session_file_found": bool(sessions),
        "stop_details_observation": explanation_summary,
        "receipt": str((case_out / "native-receipt.json").relative_to(ROOT)),
    }, ensure_ascii=True))


if __name__ == "__main__":
    main()
