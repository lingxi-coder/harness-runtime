#!/usr/bin/env python3
"""Capture public Native Stop prompt-hook requests for 2.1.293 or 2.1.294."""

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
NATIVE_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
if not NATIVE_VALUE:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
BINARY = Path(NATIVE_VALUE).expanduser().resolve()
DUMMY_KEY = "native-stop-prompt-hook-fixture-dummy-key"
MARKER = b"FIXED_STOP_PROMPT_HOOK_INSTRUCTION"


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
            continue
        if raw[cursor] == 0x22:
            return cursor + 1
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
    """Return one original top-level JSON value token without reserialization."""
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
            continue
        if raw[cursor:cursor + 1] == b"}":
            return None
        raise ValueError("expected JSON object comma or close")


def make_sse(response_id: str, model: str, text: str) -> bytes:
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


def make_json(response_id: str, model: str, text: str) -> bytes:
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


def string_paths(value: object, path: str = "$") -> list[dict]:
    found = []
    if isinstance(value, str):
        if "FIXED_STOP_PROMPT_HOOK_INSTRUCTION" in value or "hook_event_name" in value:
            raw = value.encode("utf-16-le", errors="surrogatepass")
            found.append({
                "path": path,
                "utf16_units_count": len(raw) // 2,
                "utf16_sha256": sha256(raw),
                "instruction_marker_offset_utf16": (
                    value.find("FIXED_STOP_PROMPT_HOOK_INSTRUCTION")
                    if "FIXED_STOP_PROMPT_HOOK_INSTRUCTION" in value else None
                ),
                "contains_instruction_marker": "FIXED_STOP_PROMPT_HOOK_INSTRUCTION" in value,
                "contains_stop_event_name": "Stop" in value,
            })
    elif isinstance(value, dict):
        for key, child in value.items():
            found.extend(string_paths(child, f"{path}.{key}"))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.extend(string_paths(child, f"{path}[{index}]"))
    return found


def main() -> int:
    version_result = subprocess.run([str(BINARY), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version_result.stdout + version_result.stderr).decode("utf-8", errors="backslashreplace").strip()
    version = version_text.split(" ", 1)[0]
    if version_result.returncode != 0 or version not in ("2.1.293", "2.1.294", "2.1.295"):
        raise SystemExit(f"Expected Native 2.1.293, 2.1.294, or 2.1.295, got {version_text!r}")

    binary_hash = sha256(BINARY.read_bytes())
    expected_hash = FIXTURE["native_binary_hashes"][version]
    if binary_hash != expected_hash:
        raise SystemExit(f"Native {version} SHA mismatch: {binary_hash}")

    output = ROOT / "run-output" / version
    shared_root = ROOT / "run-output" / "shared"
    if output.exists():
        shutil.rmtree(output)
    if shared_root.exists():
        shutil.rmtree(shared_root)
    output.mkdir(parents=True)
    cwd = shared_root / "cwd"
    config = shared_root / "config"
    home = shared_root / "home"
    temp = shared_root / "tmp"
    for directory in (cwd, config, home, temp):
        directory.mkdir(parents=True, exist_ok=True)

    settings_path = ROOT / "settings.json"

    main_response_id = "msg_fixed_stop_hook_main_293_294"
    hook_response_id = "msg_fixed_stop_hook_eval_293_294"
    main_sse = make_sse(main_response_id, FIXTURE["model"], FIXTURE["main_response_text"])
    main_json = make_json(main_response_id, FIXTURE["model"], FIXTURE["main_response_text"])
    hook_sse = make_sse(hook_response_id, FIXTURE["model"], FIXTURE["hook_response_text"])
    hook_json = make_json(hook_response_id, FIXTURE["model"], FIXTURE["hook_response_text"])
    requests: list[dict] = []
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
            requests.append({"method": "HEAD", "path": self.path, "accepted": accepted})
            self.reply(200 if accepted else 404)

        def do_POST(self) -> None:
            raw = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            request = None
            try:
                request = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                pass
            seq = sum(1 for item in requests if item.get("method") == "POST") + 1
            is_marker_request = MARKER in raw
            is_stop_event_request = b"hook_event_name" in raw and b"Stop" in raw
            is_hook_candidate = is_marker_request or is_stop_event_request or seq > 1
            classification = "prompt_hook" if (is_marker_request or is_stop_event_request) else ("secondary" if seq > 1 else "main")
            record = {
                "method": "POST",
                "path": self.path,
                "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "sequence": seq,
                "classification": classification,
                "request_bytes": len(raw),
                "request_sha256": sha256(raw),
                "model": request.get("model") if isinstance(request, dict) else None,
                "stream": request.get("stream") if isinstance(request, dict) else None,
                "marker_in_original_request_bytes": is_marker_request,
                "stop_event_key_in_original_request_bytes": is_stop_event_request,
                "request_artifact": f"request-{seq:02d}.raw.json",
            }
            requests.append(record)
            request_dir = output / "requests"
            request_dir.mkdir(parents=True, exist_ok=True)
            (request_dir / record["request_artifact"]).write_bytes(raw)

            for field in ("system", "messages"):
                try:
                    raw_field = raw_top_level_property(raw, field)
                except (ValueError, json.JSONDecodeError):
                    raw_field = None
                if raw_field is not None:
                    (request_dir / f"request-{seq:02d}.{field}.raw.json").write_bytes(raw_field)
                    record[f"{field}_source_bytes"] = len(raw_field)
                    record[f"{field}_source_sha256"] = sha256(raw_field)
                    record[f"{field}_artifact"] = f"request-{seq:02d}.{field}.raw.json"
                else:
                    record[f"{field}_source_bytes"] = None
                    record[f"{field}_source_sha256"] = None
                    record[f"{field}_artifact"] = None
            if isinstance(request, dict):
                record["marker_strings"] = string_paths(request)

            if not accepted:
                self.reply(404, b"{}")
                return

            hook_request = is_hook_candidate
            stream_requested = isinstance(request, dict) and request.get("stream") is True
            if hook_request:
                body = hook_sse if stream_requested else hook_json
                content_type = "text/event-stream" if stream_requested else "application/json"
            else:
                body = main_sse if stream_requested else main_json
                content_type = "text/event-stream" if stream_requested else "application/json"
            response_bodies.append((content_type, body))
            record.update({
                "response_content_type": content_type,
                "response_bytes": len(body),
                "response_sha256": sha256(body),
            })
            self.reply(200, body, content_type)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    argv = [
        str(BINARY),
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
            completed = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=60, check=False)
            stdout, stderr, exit_code, timed_out = completed.stdout, completed.stderr, completed.returncode, False
        except subprocess.TimeoutExpired as error:
            stdout, stderr, exit_code, timed_out = error.stdout or b"", error.stderr or b"", None, True
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)

    (output / "native.stdout.raw").write_bytes(stdout)
    (output / "native.stderr.raw").write_bytes(stderr)
    parsed_stdout = None
    stdout_parse_error = None
    try:
        parsed_stdout = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        stdout_parse_error = type(error).__name__

    for record, (content_type, response_body) in zip(
        (item for item in requests if item.get("method") == "POST" and item.get("accepted")),
        response_bodies,
    ):
        record["response_artifact"] = f"response-{record['sequence']:02d}.{'sse' if content_type == 'text/event-stream' else 'json'}.raw"
        (output / record["response_artifact"]).write_bytes(response_body)

    hook_requests = [item for item in requests if item.get("classification") == "prompt_hook"]
    receipt = {
        "native_version": version_text,
        "native_sha256": binary_hash,
        "public_entrypoint": "Native CLI --print --output-format json with isolated --settings Stop prompt hook",
        "fixed_inputs": FIXTURE,
        "hook_settings": {
            "scope": "explicit --settings file plus project-only setting sources",
            "file_sha256": sha256(settings_path.read_bytes()),
            "file_artifact": "settings.json",
            "prompt_marker": "FIXED_STOP_PROMPT_HOOK_INSTRUCTION",
        },
        "network_boundary": "127.0.0.1-only Messages stub; fixed dummy API key; no upstream relay",
        "requests": requests,
        "hook_request_count": len(hook_requests),
        "hook_marker_observed_in_original_request_bytes": any(item["marker_in_original_request_bytes"] for item in hook_requests),
        "native_exit_code": exit_code,
        "timed_out": timed_out,
        "stdout_bytes": len(stdout),
        "stdout_sha256": sha256(stdout),
        "stdout_json_parse_error": stdout_parse_error,
        "stdout_json_summary": ({key: parsed_stdout.get(key) for key in ("type", "subtype", "is_error", "session_id", "num_turns") if key in parsed_stdout} if isinstance(parsed_stdout, dict) else None),
        "stdout_result": parsed_stdout.get("result") if isinstance(parsed_stdout, dict) else None,
        "stderr_bytes": len(stderr),
        "stderr_sha256": sha256(stderr),
        "artifacts": [
            "settings.json",
            "native.stdout.raw",
            "native.stderr.raw",
            *[
                artifact
                for item in requests
                if item.get("method") == "POST" and item.get("accepted")
                for artifact in (
                    item.get("request_artifact"),
                    item.get("system_artifact"),
                    item.get("messages_artifact"),
                )
                if artifact is not None
            ],
            *[item["response_artifact"] for item in requests if item.get("response_artifact")],
        ],
        "scope": "Native public Stop prompt-hook request/evaluation only; fixed model responses do not test LLM judgment quality; SubagentStop remains separate",
    }
    (output / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "native_version": version_text,
        "native_exit_code": exit_code,
        "requests": requests,
        "hook_request_count": len(hook_requests),
        "hook_marker_observed": receipt["hook_marker_observed_in_original_request_bytes"],
        "stdout_result": receipt["stdout_result"],
        "receipt": str((output / "native-receipt.json").relative_to(ROOT)),
    }, ensure_ascii=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
