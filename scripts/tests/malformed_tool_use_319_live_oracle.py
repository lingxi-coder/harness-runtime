#!/usr/bin/env python3
"""Exercise malformed-tool retries in an isolated, unmodified native process."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from compact_live_oracle import isolated_env

SHA256 = "1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd"
NUDGE = "The previous response failed to produce a valid tool call. Please retry the tool call now."


def capture(binary, output, recover, refusal=False):
    root = output / ("refusal" if refusal else ("recover-after-two" if recover else "exhausted"))
    root.mkdir()
    for name in ("config", "work", "tmp"):
        (root / name).mkdir()
    (root / "config" / ".claude.json").write_text(
        json.dumps({"hasCompletedOnboarding": True})
    )
    requests = []

    class MockAPI(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_CONNECT(self):
            self.send_error(403, "Only the loopback oracle is available")

        def do_POST(self):
            if self.path.split("?")[0] not in (
                "/v1/messages", "/v1/messages/count_tokens"
            ):
                self.send_error(403, "Unexpected oracle endpoint")
                return
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            body = json.loads(raw)
            if "count_tokens" in self.path:
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.end_headers()
                self.wfile.write(b'{"input_tokens":100}')
                return
            requests.append(body)
            (root / f"request-{len(requests):02d}.json").write_bytes(raw)
            stop = "refusal" if refusal else ("end_turn" if recover and len(requests) >= 3 else "tool_use")
            text = "RECOVERED" if stop == "end_turn" else "I will call a tool."
            message = {
                "id": f"msg_oracle_{len(requests)}", "type": "message",
                "role": "assistant", "model": body["model"], "content": [],
                "stop_reason": None, "stop_sequence": None,
                "usage": {"input_tokens": 100, "output_tokens": 0,
                          "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            }
            events = [
                {"type": "message_start", "message": message},
                {"type": "content_block_start", "index": 0,
                 "content_block": {"type": "text", "text": ""}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "text_delta", "text": text}},
                {"type": "content_block_stop", "index": 0},
                {"type": "message_delta", "delta": {"stop_reason": stop,
                  "stop_sequence": None}, "usage": {"output_tokens": 5}},
                {"type": "message_stop"},
            ]
            if refusal:
                events[-2]["delta"]["stop_details"] = {
                    "type": "refusal", "category": "other", "explanation": "ORACLE_EXPLANATION"
                }
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            if refusal:
                self.send_header("request-id", "req_refusal_oracle")
            self.end_headers()
            for event in events:
                self.wfile.write(
                    f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode()
                )
                self.wfile.flush()

    server = ThreadingHTTPServer(("127.0.0.1", 0), MockAPI)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = isolated_env(root, f"http://127.0.0.1:{server.server_port}")
    env["CLAUDE_CODE_DISABLE_FEATURE_FLAGS"] = "1"
    env["CLAUDE_CODE_DISABLE_AUTO_MEMORY"] = "1"
    command = [str(binary), "--bare", "-p", "MALFORMED_TOOL_LOCAL_ORACLE",
               "--model", "claude-sonnet-4-6", "--output-format", "stream-json",
               "--verbose", "--setting-sources", "", "--strict-mcp-config",
               "--include-partial-messages",
               "--mcp-config", '{"mcpServers":{}}', "--disable-slash-commands",
               "--no-session-persistence", "--tools", ""]
    process = subprocess.Popen(command, cwd=root / "work", env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True)
    try:
        try:
            stdout, stderr = process.communicate(timeout=45)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=5)
            raise RuntimeError("Native malformed-tool oracle timed out")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    (root / "stdout.jsonl").write_text(stdout)
    (root / "stderr.txt").write_text(stderr)
    assert len(requests) == (2 if refusal else 3), (len(requests), stdout[-1000:], stderr[-1000:])
    if not refusal:
        for request in requests[1:]:
            assert NUDGE in json.dumps(request["messages"]), "Retry nudge absent"
    events = [json.loads(line) for line in stdout.splitlines() if line.strip()]
    result = next(event for event in reversed(events) if event.get("type") == "result")
    partials = [event for event in events if event.get("type") == "stream_event"]
    assert partials, "Native partial stream output absent"
    assert all(event.get("api_message_id", "").startswith("msg_oracle_")
               for event in partials), partials
    if refusal:
        assert result["is_error"], result
        assert "ORACLE_EXPLANATION" not in result["result"], result
        assert "Request ID: req_refusal_oracle" in result["result"], result
        assert "Message ID: msg_oracle_2" in result["result"], result
        error_row = next(event for event in reversed(events) if event.get("is_api_error_message"))
        assert error_row["request_id"] == "req_refusal_oracle", error_row
        assert error_row["refused_message_id"] == "msg_oracle_2", error_row
        assert error_row["error"] == "invalid_request" and "api_error" not in error_row, error_row
        assert result["terminal_reason"] == "api_error" and result["stop_reason"] == "refusal", result
    elif recover:
        assert process.returncode == 0 and not result["is_error"], result
        assert result["result"] == "RECOVERED", result
    else:
        assert result["is_error"], result
        assert result["api_error"] == "malformed_tool_use_exhausted", result
        assert result["terminal_reason"] == "malformed_tool_use_exhausted", result
        assert result["result"] == "The model's tool call could not be parsed (retry also failed).", result
    return {"scenario": root.name, "requests": len(requests),
            "partial_events_with_api_message_id": len(partials),
            "exit_code": process.returncode, "result_subtype": result["subtype"],
            "api_error": result.get("api_error"),
            "terminal_reason": result.get("terminal_reason"),
            "is_error": result["is_error"], "passed": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--refusal", action="store_true", help="Capture the terminal refusal envelope instead of malformed retries")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    digest = hashlib.sha256()
    with binary.open("rb") as source:
        for part in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(part)
    assert digest.hexdigest() == SHA256, "Unexpected native artifact"
    args.output.mkdir(parents=True, exist_ok=False)
    cases = [capture(binary, args.output, False, True)] if args.refusal else [
        capture(binary, args.output, recover) for recover in (False, True)
    ]
    report = {"version": "2.1.319", "native_sha256": SHA256, "cases": cases,
              "scope": "Unmodified native process, isolated configuration, fake API key, loopback scripted SSE. No model-quality or real-provider claim."}
    (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(cases, indent=2))


if __name__ == "__main__":
    main()
