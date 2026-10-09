#!/usr/bin/env python3
"""Capture public Native SubagentStop prompt-hook requests for 2.1.293/294."""

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
FIXTURE = json.loads((ROOT / "cases.json").read_text(encoding="utf-8"))
NATIVE_VALUE = os.environ.get("CLAUDE_NATIVE_BINARY")
if not NATIVE_VALUE:
    raise SystemExit("Set CLAUDE_NATIVE_BINARY to the Native Claude executable")
BINARY = Path(NATIVE_VALUE).expanduser().resolve()
DUMMY_KEY = "native-subagent-stop-fixture-dummy-key"


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
    """Return the original top-level JSON value token without reserialization."""
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


def text_paths(value: object, path: str = "$") -> list[tuple[str, str]]:
    found: list[tuple[str, str]] = []
    if isinstance(value, str):
        found.append((path, value))
    elif isinstance(value, dict):
        for key, child in value.items():
            found.extend(text_paths(child, f"{path}.{key}"))
    elif isinstance(value, list):
        for index, child in enumerate(value):
            found.extend(text_paths(child, f"{path}[{index}]"))
    return found


def parent_received_child_result(request: object) -> bool:
    if not isinstance(request, dict):
        return False
    messages = request.get("messages")
    if not isinstance(messages, list):
        return False
    agent_call_ids = set()
    for message in messages:
        if not isinstance(message, dict) or message.get("role") != "assistant":
            continue
        for block in message.get("content", []):
            if isinstance(block, dict) and block.get("type") == "tool_use" \
                    and block.get("name") == FIXTURE["agent_tool_name"]:
                agent_call_ids.add(block.get("id"))
    if not agent_call_ids:
        return False
    for message in messages:
        if not isinstance(message, dict) or message.get("role") != "user":
            continue
        for block in message.get("content", []):
            if not isinstance(block, dict) or block.get("type") != "tool_result":
                continue
            if block.get("tool_use_id") not in agent_call_ids:
                continue
            if FIXTURE["child_response_text"] in "\n".join(value for _, value in text_paths(block)):
                return True
    return False


def classify_request(request: object) -> tuple[str, dict]:
    paths = text_paths(request)
    joined = "\n".join(value for _, value in paths)
    tool_names = {
        tool.get("name")
        for tool in request.get("tools", [])
        if isinstance(request, dict) and isinstance(tool, dict)
    } if isinstance(request, dict) else set()

    if FIXTURE["hook_instruction"] in joined:
        event_match = re.search(r'"hook_event_name"\s*:\s*"([^"]+)"', joined)
        event = event_match.group(1) if event_match else None
        if event == "SubagentStop":
            return "subagent_stop_hook", {"hook_event_name": event}
        return "hook_prompt_unconfirmed", {"hook_event_name": event}
    if parent_received_child_result(request):
        return "parent_continuation", {
            "agent_tool_result_matched": True,
            "child_result_marker_observed": True,
        }
    if FIXTURE["child_task_marker"] in joined:
        return "child_request", {"child_task_marker_observed": True}
    if FIXTURE["main_prompt"].split(":", 1)[0] in joined and FIXTURE["agent_tool_name"] in tool_names:
        return "main_request", {"agent_tool_advertised": True}
    return "unclassified", {
        "agent_tool_advertised": FIXTURE["agent_tool_name"] in tool_names,
        "known_marker_paths": [path for path, text in paths if "FIXED_SUBAGENT_" in text],
    }


def sse(events: list[tuple[str, dict]]) -> bytes:
    return "".join(
        f"event: {name}\ndata: {json.dumps(value, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, value in events
    ).encode("ascii")


def make_text_response(response_id: str, text: str) -> bytes:
    model = FIXTURE["model"]
    return sse([
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
    ])


def make_agent_response() -> bytes:
    response_id = "msg_fixed_subagent_agent_tool_293_294"
    model = FIXTURE["model"]
    tool_input = json.dumps(FIXTURE["agent_tool_input"], ensure_ascii=True, separators=(",", ":"))
    return sse([
        ("message_start", {"type": "message_start", "message": {
            "id": response_id, "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 8, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                 "content_block": {"type": "tool_use",
                                                   "id": FIXTURE["agent_tool_use_id"],
                                                   "name": FIXTURE["agent_tool_name"], "input": {}}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                 "delta": {"type": "input_json_delta", "partial_json": tool_input}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                           "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                           "usage": {"output_tokens": 4}}),
        ("message_stop", {"type": "message_stop"}),
    ])


def response_for(classification: str) -> bytes:
    if classification == "main_request":
        return make_agent_response()
    if classification == "child_request":
        return make_text_response("msg_fixed_subagent_child_293_294", FIXTURE["child_response_text"])
    if classification in ("subagent_stop_hook", "hook_prompt_unconfirmed"):
        return make_text_response("msg_fixed_subagent_hook_293_294", FIXTURE["hook_response_text"])
    if classification == "parent_continuation":
        return make_text_response("msg_fixed_subagent_parent_293_294", FIXTURE["main_response_text"])
    return b'{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected request in fixed SubagentStop fixture"}}'


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
    cwd, config, home, temp = [shared_root / name for name in ("cwd", "config", "home", "tmp")]
    for directory in (cwd, config, home, temp):
        directory.mkdir(parents=True, exist_ok=True)

    requests: list[dict] = []
    responses: list[tuple[int, bytes, str]] = []

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
            try:
                request = json.loads(raw)
            except (UnicodeDecodeError, json.JSONDecodeError):
                request = None
            classification, evidence = classify_request(request) if isinstance(request, dict) else (
                "unclassified", {"json_parse_failed": True}
            )
            seq = sum(item.get("method") == "POST" for item in requests) + 1
            local_request_artifact = f"request-{seq:02d}.raw.json"
            record = {
                "method": "POST", "sequence": seq, "path": self.path,
                "accepted": accepted, "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "classification": classification, "classification_evidence": evidence,
                "request_bytes": len(raw), "request_sha256": sha256(raw),
                "model": request.get("model") if isinstance(request, dict) else None,
                "stream": request.get("stream") if isinstance(request, dict) else None,
                "request_artifact": f"../../run-output/{version}/requests/{local_request_artifact}",
            }
            requests.append(record)
            request_dir = output / "requests"
            request_dir.mkdir(parents=True, exist_ok=True)
            (request_dir / local_request_artifact).write_bytes(raw)
            for field in ("system", "messages"):
                try:
                    raw_field = raw_top_level_property(raw, field)
                except (ValueError, json.JSONDecodeError):
                    raw_field = None
                artifact = f"request-{seq:02d}.{field}.raw.json" if raw_field is not None else None
                if artifact is not None:
                    (request_dir / artifact).write_bytes(raw_field)
                    record[f"{field}_artifact"] = artifact
                    record[f"{field}_baseline_artifact"] = f"requests/{artifact}"
                    record[f"{field}_bytes"] = len(raw_field)
                    record[f"{field}_sha256"] = sha256(raw_field)
                else:
                    record[f"{field}_artifact"] = None
                    record[f"{field}_baseline_artifact"] = None

            if not accepted:
                self.reply(404, b"{}")
                return
            stream_requested = isinstance(request, dict) and request.get("stream") is True
            body = response_for(classification)
            status = 200 if classification != "unclassified" else 400
            content_type = "text/event-stream" if stream_requested and status == 200 else "application/json"
            if stream_requested and status == 200:
                response_artifact = f"response-{seq:02}.sse.raw"
            else:
                response_artifact = f"response-{seq:02}.json.raw"
            record.update({
                "status": status, "response_content_type": content_type,
                "response_bytes": len(body), "response_sha256": sha256(body),
                "response_artifact": response_artifact,
            })
            responses.append((status, body, response_artifact))
            self.reply(status, body, content_type)

    server = HTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    argv = [
        str(BINARY), "--setting-sources", "project", "--settings", str(ROOT / "settings.json"),
        "--strict-mcp-config", "--restricted", "--permission-mode", "acceptEdits",
        "--permission-prompts", "none", "--print", "--output-format", "json",
        "--no-session-persistence", "--tools", "Agent", "--max-turns", "3",
        "--max-budget-usd", "1", "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"],
        "--debug", "hooks", FIXTURE["main_prompt"],
    ]
    env = {
        "PATH": "/usr/bin:/bin", "HOME": str(home), "TMPDIR": str(temp), "LANG": "C.UTF-8",
        "TZ": "UTC", "TERM": "dumb", "CLAUDE_CONFIG_DIR": str(config),
        "ANTHROPIC_API_KEY": DUMMY_KEY,
        "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1",
    }
    try:
        try:
            completed = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=90, check=False)
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
    for status, body, artifact in responses:
        (output / artifact).write_bytes(body)

    posts = [request for request in requests if request.get("method") == "POST"]
    counts = {kind: sum(item.get("classification") == kind for item in posts) for kind in (
        "main_request", "child_request", "subagent_stop_hook", "hook_prompt_unconfirmed",
        "parent_continuation", "unclassified",
    )}
    result_text = parsed_stdout.get("result") if isinstance(parsed_stdout, dict) else None
    parent_completed = result_text == FIXTURE["main_response_text"]
    actual_hook = counts["subagent_stop_hook"] > 0
    checked_in_artifacts = ["native.stdout.raw", "native.stderr.raw"]
    for record in posts:
        if record.get("classification") in ("child_request", "subagent_stop_hook"):
            checked_in_artifacts.extend(
                artifact for artifact in (
                    record.get("system_baseline_artifact"),
                    record.get("messages_baseline_artifact"),
                ) if artifact
            )
        if record.get("response_artifact"):
            checked_in_artifacts.append(record["response_artifact"])
    receipt = {
        "native_version": version_text, "native_sha256": binary_hash,
        "public_entrypoint": "Native CLI --print --output-format json with isolated SubagentStop prompt hook and built-in Agent tool",
        "fixed_inputs": FIXTURE,
        "settings_file_sha256": sha256((ROOT / "settings.json").read_bytes()),
        "network_boundary": "127.0.0.1-only Messages stub; fixed dummy API key; no upstream relay",
        "classification_rule": "Request role is derived from fixed prompt/result/hook markers and advertised Agent tool; request sequence is recorded only for chronology, never used to infer role.",
        "checked_in_baseline_artifacts": checked_in_artifacts,
        "full_request_bodies": "Retained only under the ignored run-output/<version>/requests/ path because they include injected project/system context; the selected original system/messages value slices are checked in.",
        "requests": requests, "classification_counts": counts,
        "actual_subagent_stop_event_observed": actual_hook,
        "child_request_observed": counts["child_request"] > 0,
        "parent_completed_with_fixed_response": parent_completed,
        "native_exit_code": exit_code, "timed_out": timed_out,
        "stdout_bytes": len(stdout), "stdout_sha256": sha256(stdout),
        "stdout_json_parse_error": stdout_parse_error,
        "stdout_summary": ({key: parsed_stdout.get(key) for key in ("type", "subtype", "is_error", "session_id", "num_turns") if key in parsed_stdout} if isinstance(parsed_stdout, dict) else None),
        "stdout_result": result_text,
        "scope": "Only a confirmed request containing the configured hook marker and hook_event_name=SubagentStop proves Native SubagentStop execution; fixed child/hook/parent model responses do not test agent judgment quality.",
    }
    (output / "native-receipt.json").write_text(json.dumps(receipt, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({
        "native_version": version_text, "native_exit_code": exit_code,
        "classification_counts": counts, "actual_subagent_stop_event_observed": actual_hook,
        "child_request_observed": receipt["child_request_observed"],
        "parent_completed_with_fixed_response": parent_completed,
        "stdout_result": result_text, "receipt": str((output / "native-receipt.json").relative_to(ROOT)),
    }, ensure_ascii=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
