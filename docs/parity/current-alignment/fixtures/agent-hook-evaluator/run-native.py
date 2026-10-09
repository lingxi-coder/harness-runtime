#!/usr/bin/env python3
"""Capture one Native 2.1.294 public type:agent Stop hook evaluator request."""

import hashlib
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import threading


ROOT = Path(__file__).resolve().parent
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
BINARY_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
DUMMY_KEY = "native-agent-hook-evaluator-294-dummy-key"
MARKER = b"FIXED_AGENT_HOOK_EVALUATOR_INSTRUCTION"


def sha256(raw: bytes) -> str:
    return hashlib.sha256(raw).hexdigest()


def skip_ws(raw: bytes, cursor: int) -> int:
    while cursor < len(raw) and raw[cursor] in b" \t\r\n":
        cursor += 1
    return cursor


def string_end(raw: bytes, start: int) -> int:
    if raw[start:start + 1] != b'"':
        raise ValueError("expected JSON string")
    cursor = start + 1
    while cursor < len(raw):
        if raw[cursor] == 0x5C:
            cursor += 2
        elif raw[cursor] == 0x22:
            return cursor + 1
        else:
            cursor += 1
    raise ValueError("unterminated JSON string")


def value_end(raw: bytes, start: int) -> int:
    first = raw[start:start + 1]
    if first == b'"':
        return string_end(raw, start)
    if first not in (b"{", b"["):
        cursor = start
        while cursor < len(raw) and raw[cursor] not in b",}] \t\r\n":
            cursor += 1
        return cursor
    stack = [0x7D if first == b"{" else 0x5D]
    cursor = start + 1
    in_string = False
    while cursor < len(raw):
        byte = raw[cursor]
        if in_string:
            if byte == 0x5C:
                cursor += 2
                continue
            if byte == 0x22:
                in_string = False
        elif byte == 0x22:
            in_string = True
        elif byte == 0x7B:
            stack.append(0x7D)
        elif byte == 0x5B:
            stack.append(0x5D)
        elif byte in (0x7D, 0x5D):
            if not stack or stack.pop() != byte:
                raise ValueError("mismatched JSON delimiters")
            if not stack:
                return cursor + 1
        cursor += 1
    raise ValueError("unterminated JSON compound value")


def raw_top_level_property(raw: bytes, wanted: str) -> bytes | None:
    cursor = skip_ws(raw, 0)
    if raw[cursor:cursor + 1] != b"{":
        return None
    cursor += 1
    while True:
        cursor = skip_ws(raw, cursor)
        if raw[cursor:cursor + 1] == b"}":
            return None
        key_start = cursor
        key_end = string_end(raw, key_start)
        key = json.loads(raw[key_start:key_end])
        cursor = skip_ws(raw, key_end)
        if raw[cursor:cursor + 1] != b":":
            raise ValueError("expected JSON object colon")
        value_start = skip_ws(raw, cursor + 1)
        end = value_end(raw, value_start)
        if key == wanted:
            return raw[value_start:end]
        cursor = skip_ws(raw, end)
        if raw[cursor:cursor + 1] == b",":
            cursor += 1
        elif raw[cursor:cursor + 1] == b"}":
            return None
        else:
            raise ValueError("expected JSON object comma or close")


def response_events(response_id: str, model: str, text: str) -> bytes:
    events = [
        ("message_start", {"type": "message_start", "message": {
            "id": response_id, "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 8, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                 "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                 "delta": {"type": "text_delta", "text": text}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                           "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                           "usage": {"output_tokens": 4}}),
        ("message_stop", {"type": "message_stop"}),
    ]
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("ascii")


def response_json(response_id: str, model: str, text: str) -> bytes:
    return json.dumps({
        "id": response_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "stop_sequence": None,
        "usage": {"input_tokens": 8, "output_tokens": 4},
    }, ensure_ascii=True, separators=(",", ":")).encode("ascii")


def main() -> int:
    if not BINARY_VALUE:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the exact 2.1.294 executable")
    binary = Path(BINARY_VALUE).expanduser().resolve()
    if not binary.is_file():
        raise SystemExit(f"Native executable does not exist: {binary}")
    version_result = subprocess.run(
        [str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version_result.stdout + version_result.stderr).decode(
        "utf-8", errors="backslashreplace").strip()
    binary_hash = sha256(binary.read_bytes())
    if version_result.returncode != 0 or FIXTURE["native_version"] not in version_text:
        raise SystemExit(f"Expected Native 2.1.294, got {version_text!r}")
    if binary_hash != FIXTURE["native_sha256"]:
        raise SystemExit(f"Native 2.1.294 SHA mismatch: {binary_hash}")

    output = ROOT / "run-output" / "2.1.294"
    if output.exists():
        shutil.rmtree(output)
    output.mkdir(parents=True)
    dirs = {name: output / name for name in ("cwd", "config", "home", "tmp")}
    for path in dirs.values():
        path.mkdir(parents=True)
    settings_path = ROOT / "settings.json"
    request_records = []
    response_records = []
    main_response_id = "msg_fixed_agent_hook_main_294"
    hook_response_id = "msg_fixed_agent_hook_eval_294"

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
            self.wfile.flush()

        def do_POST(self) -> None:
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            try:
                request = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                request = None
            marker_in_request = MARKER in raw
            hook_event_present = b"hook_event_name" in raw and b"Stop" in raw
            is_evaluator = marker_in_request or hook_event_present
            sequence = len(request_records) + 1
            role = "agent_hook_evaluator" if is_evaluator else (
                "main_turn" if sequence == 1 else "unclassified_followup")
            request_path = f"request-{sequence:02d}.raw.json"
            raw_request_path = output / request_path
            raw_request_path.write_bytes(raw)
            record = {
                "method": "POST",
                "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "sequence": sequence,
                "classification": role,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
                "model": request.get("model") if isinstance(request, dict) else None,
                "stream": request.get("stream") if isinstance(request, dict) else None,
                "agent_instruction_marker_in_raw_request": marker_in_request,
                "hook_event_marker_in_raw_request": hook_event_present,
                "raw_request": request_path,
            }
            request_records.append(record)
            for field in ("system", "messages"):
                value = raw_top_level_property(raw, field)
                if value is None:
                    record[f"{field}_raw"] = None
                    continue
                field_path = f"request-{sequence:02d}.{field}.raw.json"
                (output / field_path).write_bytes(value)
                record[f"{field}_raw"] = {
                    "path": field_path,
                    "bytes": len(value),
                    "sha256": sha256(value),
                }
            if not accepted:
                body = b'{"error":{"type":"invalid_request_error","message":"Unexpected request in local agent-hook fixture"}}'
                self.reply(404, body, "application/json")
                return

            if role == "main_turn":
                response_id = main_response_id
                response_text = FIXTURE["main_response_text"]
            else:
                response_id = hook_response_id
                response_text = FIXTURE["hook_response_text"]
            streaming = isinstance(request, dict) and request.get("stream") is True
            if streaming:
                body = response_events(response_id, FIXTURE["model"], response_text)
                content_type = "text/event-stream"
                extension = "sse.raw"
            else:
                body = response_json(response_id, FIXTURE["model"], response_text)
                content_type = "application/json"
                extension = "json.raw"
            response_path = f"response-{sequence:02d}.{extension}"
            (output / response_path).write_bytes(body)
            response_records.append({
                "sequence": sequence,
                "role": role,
                "path": response_path,
                "content_type": content_type,
                "bytes": len(body),
                "sha256": sha256(body),
                "response_id": response_id,
            })
            record["response_path"] = response_path
            record["response_sha256"] = sha256(body)
            self.reply(200, body, content_type)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    argv = [
        str(binary),
        "--setting-sources", "project",
        "--settings", str(settings_path),
        "--strict-mcp-config",
        "--print", "--output-format", "json", "--no-session-persistence",
        "--tools", "", "--max-turns", "1", "--max-budget-usd", "1",
        "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"],
        "--debug", "hooks",
        FIXTURE["main_prompt"],
    ]
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(dirs["home"]),
        "TMPDIR": str(dirs["tmp"]),
        "LANG": "C.UTF-8",
        "TZ": "UTC",
        "TERM": "dumb",
        "CLAUDE_CONFIG_DIR": str(dirs["config"]),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1",
        "DO_NOT_TRACK": "1",
    }
    try:
        try:
            completed = subprocess.run(
                argv, cwd=dirs["cwd"], env=env, capture_output=True,
                timeout=45, check=False)
            stdout, stderr = completed.stdout, completed.stderr
            exit_code, timed_out = completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout, stderr = error.stdout or b"", error.stderr or b""
            exit_code, timed_out = None, True
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    (output / "native.stdout.raw").write_bytes(stdout)
    (output / "native.stderr.raw").write_bytes(stderr)
    hook_records = [record for record in request_records
                    if record["classification"] == "agent_hook_evaluator"]
    receipt = {
        "native_version": version_text,
        "native_sha256": binary_hash,
        "binary_path": str(binary),
        "public_entrypoint": "Native CLI --print --output-format json with isolated --settings Stop type:agent hook",
        "fixed_inputs": FIXTURE,
        "settings": {
            "path": "settings.json",
            "sha256": sha256(settings_path.read_bytes()),
            "hook_event": "Stop",
            "hook_type": "agent",
            "prompt_marker": MARKER.decode("ascii"),
        },
        "argv": ["$CLAUDE_NATIVE_BINARY", "--setting-sources", "project", "--settings",
                 "settings.json", "--strict-mcp-config", "--print", "--output-format", "json",
                 "--no-session-persistence", "--tools", "", "--max-turns", "1",
                 "--max-budget-usd", "1", "--model", FIXTURE["model"], "--session-id",
                 FIXTURE["session_id"], "--debug", "hooks", FIXTURE["main_prompt"]],
        "network_boundary": "127.0.0.1-only Messages stub; fixed dummy key; retries disabled; no upstream relay",
        "timeout_seconds": 45,
        "requests": request_records,
        "responses": response_records,
        "agent_hook_request_count": len(hook_records),
        "agent_instruction_marker_observed_in_original_request_bytes": any(
            item["agent_instruction_marker_in_raw_request"] for item in hook_records),
        "hook_event_observed_in_original_request_bytes": any(
            item["hook_event_marker_in_raw_request"] for item in hook_records),
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "stdout": {"path": "native.stdout.raw", "bytes": len(stdout), "sha256": sha256(stdout)},
        "stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha256(stderr)},
        "scope": "Native 2.1.294 public type:agent Stop hook evaluator request only; fixed local responses do not test agent judgment quality",
    }
    (output / "native-receipt.json").write_text(
        json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "native_version": version_text,
        "native_sha256": binary_hash,
        "exit_code": exit_code,
        "timed_out": timed_out,
        "requests": request_records,
        "agent_hook_requests": len(hook_records),
        "receipt": str((output / "native-receipt.json").relative_to(ROOT)),
    }, ensure_ascii=True, indent=2))
    if timed_out or exit_code != 0 or not hook_records:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
