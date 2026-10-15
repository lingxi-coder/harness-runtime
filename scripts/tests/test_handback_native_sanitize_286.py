#!/usr/bin/env python3
"""Verify pinned report-sanitizer bytes through the actual native Agent path.

Usage:
  python3 scripts/tests/test_handback_native_sanitize_286.py /path/to/claude-2.1.286 \
    --output /tmp/handback-native-sanitize-286

Each scenario uses a fresh temporary config/workspace and a fake Anthropic API
key. All HTTP proxies point to the loopback mock, which rejects unexpected paths
and CONNECT. No live model or provider is called. The mock returns one Agent call
and the fixture's child report; native Agent finalization, report sanitization,
task notifications and caller-request serialization execute without replacement.
Native notification JSON preserves unmatched UTF-16 units. The subsequent model
API text projects those units to U+FFFD; both boundaries are checked explicitly.

Bare mode removes Agent from the available tools, so this probe uses an isolated
normal invocation with explicit Agent-only tools and no skills or MCP servers.
Reported token usage and cost are synthetic. Captures stay under --output.
Only the listed scalar report and seven UTF-16 cases are native CLI checks; the
remaining corpus cases execute the extracted native source in pinned Node.
"""

from __future__ import annotations

import argparse
import hashlib
import html
import json
import os
import signal
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from compact_live_oracle import isolated_env

REPO = Path(__file__).resolve().parents[2]
FIXTURE = REPO / "crates/core/tests/fixtures/handback_sanitize_2_1_286.json"
ROOT_PROMPT = "NATIVE_SANITIZER_PROBE_PARENT"
CHILD_PROMPT = "NATIVE_SANITIZER_PROBE_CHILD"
TOOL_ID = "toolu_native_oracle_agent"
UTF16_CASE_NAMES = (
    "unicode17-letter-pair-boundary",
    "unicode17-number-pair-boundary",
    "unicode17-letter-unpaired-high-boundary",
    "unicode17-letter-unpaired-low-boundary",
    "unicode17-letter-split-pair-boundary",
    "unicode17-letter-pair-escaped",
    "unicode17-letter-pair-before-lone-low",
)


def dump(path: Path, value: object) -> None:
    # ASCII JSON keeps unmatched UTF-16 units distinct in captures and SSE.
    path.write_text(json.dumps(value, indent=2, ensure_ascii=True) + "\n")


def utf16_bytes(value: str) -> bytes:
    return value.encode("utf-16-le", errors="surrogatepass")


def from_units(units: list[int]) -> str:
    raw = b"".join(unit.to_bytes(2, "little") for unit in units)
    return raw.decode("utf-16-le", errors="surrogatepass")


def model_api_text(value: str) -> str:
    # The actual native caller request replaces unmatched units, independently
    # of mR and notification JSON which retain their original UTF-16 identity.
    return utf16_bytes(value).decode("utf-16-le", errors="replace")


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for block in iter(lambda: file.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def last_user_blocks(body: dict) -> list[dict]:
    last = next((message for message in reversed(body.get("messages", []))
                 if message.get("role") == "user"), {})
    content = last.get("content", [])
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    return [block for block in content if isinstance(block, dict)] if isinstance(content, list) else []


def make_handler(root: Path, report: str, requests: list[dict], refused: list[str]):
    lock = threading.Lock()

    class MockAPI(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reject(self):
            with lock:
                refused.append(self.path)
            self.send_error(403, "Only the loopback fake Anthropic API is available")

        def do_CONNECT(self):
            self.reject()

        def do_GET(self):
            self.reject()

        def do_POST(self):
            if self.path.split("?")[0] not in ("/v1/messages", "/v1/messages/count_tokens"):
                self.reject()
                return
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            body = json.loads(raw)
            with lock:
                requests.append({"path": self.path, "body": body})
                sequence = len(requests)
                dump(root / "requests.json", requests)
                (root / f"request-{sequence:02d}.json").write_bytes(raw)
            if "count_tokens" in self.path:
                self.respond_json(200, {"input_tokens": 100})
                return
            blocks = last_user_blocks(body)
            text = "\n".join(block.get("text", "") for block in blocks
                             if block.get("type") == "text")
            has_result = any(block.get("type") == "tool_result"
                             and block.get("tool_use_id") == TOOL_ID for block in blocks)
            if has_result:
                response = [{"type": "text", "text": "ORACLE_DONE"}]
                reason = "end_turn"
            elif CHILD_PROMPT in text:
                response = [{"type": "text", "text": report}]
                reason = "end_turn"
            elif ROOT_PROMPT in text and any(tool.get("name") == "Agent" for tool in body.get("tools", [])):
                response = [{"type": "tool_use", "id": TOOL_ID, "name": "Agent", "input": {
                    "description": "Native sanitizer engine probe", "prompt": CHILD_PROMPT,
                    "subagent_type": "native-probe", "run_in_background": True,
                }}]
                reason = "tool_use"
            else:
                response = [{"type": "text", "text": "MOCK_AUXILIARY_DONE"}]
                reason = "end_turn"
            message = {
                "id": f"msg_native_mock_{sequence}", "type": "message", "role": "assistant",
                "model": body["model"], "content": [], "stop_reason": None,
                "stop_sequence": None, "usage": {"input_tokens": 100, "output_tokens": 0,
                "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            }
            if not body.get("stream"):
                message.update(content=response, stop_reason=reason)
                message["usage"]["output_tokens"] = 20
                self.respond_json(200, message)
                return
            events = [{"type": "message_start", "message": message}]
            for index, block in enumerate(response):
                if block["type"] == "tool_use":
                    start = {**block, "input": {}}
                    delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])}
                else:
                    start = {"type": "text", "text": ""}
                    delta = {"type": "text_delta", "text": block["text"]}
                events.extend([
                    {"type": "content_block_start", "index": index, "content_block": start},
                    {"type": "content_block_delta", "index": index, "delta": delta},
                    {"type": "content_block_stop", "index": index},
                ])
            events.extend([
                {"type": "message_delta", "delta": {"stop_reason": reason, "stop_sequence": None},
                 "usage": {"output_tokens": 20}}, {"type": "message_stop"},
            ])
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            for event in events:
                self.wfile.write(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode())
                self.wfile.flush()

        def respond_json(self, status: int, body: dict):
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(body).encode())

    return MockAPI


def run_scenario(binary: Path, root: Path, name: str, report: str, expected: str, timeout: float) -> dict:
    root.mkdir(parents=True, exist_ok=False)
    for directory in ("config", "work", "tmp"):
        (root / directory).mkdir()
    requests: list[dict] = []
    refused: list[str] = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(root, report, requests, refused))
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    command = [
        str(binary), "-p", ROOT_PROMPT, "--output-format", "stream-json", "--verbose",
        "--model", "claude-sonnet-4-6", "--setting-sources", "", "--strict-mcp-config",
        "--mcp-config", '{"mcpServers":{}}', "--system-prompt", "Local mock API sanitizer probe.",
        "--permission-mode", "bypassPermissions", "--dangerously-skip-permissions",
        "--allowedTools", "Agent", "--tools", "Agent", "--disable-slash-commands", "--agents",
        json.dumps({"native-probe": {"description": "Local mock API sanitizer probe",
                   "prompt": CHILD_PROMPT, "tools": []}}), "--debug-file", str(root / "debug.log"),
    ]
    dump(root / "metadata.json", {"scenario": name, "command": command,
         "synthetic_model_usage_and_cost": True, "actual_binary_agent_sanitizer": True})
    dump(root / "expected.json", {"input": report, "expected_summary": expected})
    env = isolated_env(root, f"http://127.0.0.1:{server.server_port}")
    process = subprocess.Popen(command, cwd=root / "work", env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate(timeout=10)
        (root / "stdout.jsonl").write_text(stdout)
        (root / "stderr.txt").write_text(stderr)
        raise AssertionError(f"Native probe timed out: {root}") from None
    finally:
        server.shutdown()
        server.server_close()
    (root / "stdout.jsonl").write_text(stdout)
    (root / "stderr.txt").write_text(stderr)
    assert process.returncode == 0, f"Native exit {process.returncode}: {root}/stderr.txt"
    assert not refused, f"Native attempted an unexpected endpoint: {refused}"
    events = [json.loads(line) for line in stdout.splitlines() if line.strip()]
    notifications = [event for event in events if event.get("type") == "system"
                     and event.get("subtype") == "task_notification"
                     and event.get("tool_use_id") == TOOL_ID]
    assert len(notifications) == 1, f"Expected exactly one native Agent completion: {root}"
    notification = notifications[0]
    assert notification["status"] == "completed", notification["status"]
    actual = notification["summary"]
    assert utf16_bytes(actual) == utf16_bytes(expected), f"Native report bytes differ: {root}"
    caller_results = []
    for request in requests:
        for message in request["body"].get("messages", []):
            content = message.get("content", [])
            if not isinstance(content, list):
                continue
            for block in content:
                if block.get("type") != "text":
                    continue
                text = block.get("text", "")
                if f"<tool-use-id>{TOOL_ID}</tool-use-id>" not in text or "<result>" not in text:
                    continue
                caller_results.append(text.split("<result>", 1)[1].split("</result>", 1)[0])
    assert caller_results, f"Sanitized Agent report never reached the caller API request: {root}"
    expected_xml = model_api_text(html.escape(expected, quote=False))
    for result in caller_results:
        assert utf16_bytes(result) == utf16_bytes(expected_xml), f"Caller result bytes differ: {root}"
    result = {"scenario": name, "passed": True, "native_cli_scenarios": 1,
              "request_count": len(requests), "summary_utf16_sha256": hashlib.sha256(utf16_bytes(actual)).hexdigest(),
              "summary_utf16_units": len(utf16_bytes(actual)) // 2,
              "caller_api_result_verified": True,
              "caller_result_utf16_sha256": hashlib.sha256(utf16_bytes(expected_xml)).hexdigest(),
              "native_summary_unmatched_surrogates_preserved":
              any(0xD800 <= ord(character) <= 0xDFFF for character in report)}
    dump(root / "result.json", result)
    return result


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=40)
    args = parser.parse_args()
    fixture = json.loads(FIXTURE.read_text())
    binary = args.binary.resolve(strict=True)
    assert file_sha256(binary) == fixture["binary_sha256"], "Unpinned native binary"
    args.output.mkdir(parents=True, exist_ok=False)
    scalar = fixture["native_cli_probe"]
    scenarios = [("unicode17-scalar-report", scalar["input"], scalar["expected_summary"])]
    for name in UTF16_CASE_NAMES:
        case = next(case for case in fixture["utf16_cases"] if case["name"] == name
                    and case["prepend_marker"] and case["provenance_enabled"])
        scenarios.append((name, from_units(case["input_units"]), from_units(case["expected_units"])))
    results = []
    for name, report, expected in scenarios:
        result = run_scenario(binary, args.output / name, name, report, expected, args.timeout)
        results.append(result)
        print(json.dumps(result), flush=True)
    summary = {"version": fixture["version"], "binary_sha256": fixture["binary_sha256"],
               "fixture_sha256": file_sha256(FIXTURE), "actual_native_cli_scenarios": len(results),
               "all_passed": True, "results": results,
               "limitations": "Fixed fake model replies; no live classifier, provider, or account validation. Other corpus modes are extracted-source Node checks."}
    dump(args.output / "validation.json", summary)
    print(f"Verified {len(results)} actual native CLI report scenarios: {args.output}/validation.json")


if __name__ == "__main__":
    main()
