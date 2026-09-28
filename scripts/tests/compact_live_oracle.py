#!/usr/bin/env python3
"""Capture compact requests/events from a supplied Claude Code binary, offline.

Usage:
  python3 scripts/tests/compact_live_oracle.py /path/to/claude --output /tmp/compact-oracle

Nothing is downloaded. Each scenario uses a fresh config and working directory,
--bare (no keychain/OAuth reads), a fake API key, and a loopback mock API. The
child inherits no account/provider/proxy credentials or user settings. Proxy
variables also point to the mock, which refuses CONNECT and unexpected paths.
Raw request bodies and stdout are saved under --output, never as repo fixtures.
The reported token usage and costs are synthetic and incur no model charges.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import re
import signal
import subprocess
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SUMMARY_PREFIX = "CRITICAL: Respond with TEXT ONLY."
FOCUS = "Preserve PRIORITY_MARKER_42 and the protocol event fields."
EMPTY_ERROR = "Error during compaction: summarization produced empty response"
SCENARIOS = ("success", "empty", "too-few", "no-messages", "whitespace", "prompt-too-long")


def dump(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")


def isolated_env(root: Path, base_url: str) -> dict[str, str]:
    return {
        "PATH": os.defpath,
        "TMPDIR": str(root / "tmp"),
        "TERM": "dumb",
        "CLAUDE_CONFIG_DIR": str(root / "config"),
        "ANTHROPIC_API_KEY": "sk-ant-api03-fake-local-oracle-only",
        "ANTHROPIC_BASE_URL": base_url,
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_AUTOUPDATER": "1",
        "DISABLE_TELEMETRY": "1",
        "DISABLE_ERROR_REPORTING": "1",
        "HTTP_PROXY": base_url,
        "HTTPS_PROXY": base_url,
        "ALL_PROXY": base_url,
        "NO_PROXY": "127.0.0.1,localhost",
    }


def is_summary(body: dict) -> bool:
    return any(
        block.get("text", "").startswith(SUMMARY_PREFIX)
        for message in body.get("messages", [])
        for block in message.get("content", [])
        if isinstance(block, dict)
    )


def make_handler(root: Path, scenario: str, requests: list[dict]):
    class MockAPI(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_CONNECT(self):
            self.send_error(403, "External connections are disabled")

        def do_POST(self):
            if self.path.split("?")[0] not in ("/v1/messages", "/v1/messages/count_tokens"):
                self.send_error(403, "Only the loopback Anthropic mock is available")
                return
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            body = json.loads(raw)
            requests.append({"path": self.path, "body": body})
            dump(root / "requests.json", requests)
            (root / f"request-{len(requests):02d}.json").write_bytes(raw)
            if "count_tokens" in self.path:
                self.respond_json(200, {"input_tokens": 2500})
                return
            summary = is_summary(body)
            attempt = sum(is_summary(request["body"]) for request in requests)
            if summary and scenario == "prompt-too-long" and attempt == 1:
                self.respond_json(400, {
                    "type": "error", "error": {"type": "invalid_request_error",
                    "message": "prompt is too long: 210000 tokens > 200000 maximum"},
                })
                return
            text = (
                "<analysis>Mock analysis.</analysis>\n\n"
                "<summary>MOCK_SUMMARY_PAYLOAD: preserve PRIORITY_MARKER_42.</summary>"
                if summary else f"MOCK_ASSISTANT_{len(requests)}: PRIORITY_MARKER_42."
            )
            if summary and scenario in ("empty", "whitespace"):
                text = "" if scenario == "empty" else "\ufeff \n\t"
            message = {
                "id": f"msg_mock_{len(requests)}", "type": "message", "role": "assistant",
                "model": body["model"], "content": [], "stop_reason": None,
                "stop_sequence": None, "usage": {
                    "input_tokens": 2500, "output_tokens": 0,
                    "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                },
            }
            if not body.get("stream"):
                message.update(content=[{"type": "text", "text": text}], stop_reason="end_turn")
                message["usage"]["output_tokens"] = 35
                self.respond_json(200, message)
                return
            events = [
                {"type": "message_start", "message": message},
                {"type": "content_block_start", "index": 0,
                 "content_block": {"type": "text", "text": ""}},
                {"type": "content_block_delta", "index": 0,
                 "delta": {"type": "text_delta", "text": text}},
                {"type": "content_block_stop", "index": 0},
                {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                 "usage": {"output_tokens": 35}},
                {"type": "message_stop"},
            ]
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


def check_capture(scenario: str, events: list[dict], requests: list[dict]) -> dict:
    summaries = [request["body"] for request in requests if is_summary(request["body"])]
    statuses = [event for event in events if event.get("subtype") == "status"]
    boundaries = [event for event in events if event.get("subtype") == "compact_boundary"]
    errors = [event["compact_error"] for event in statuses if "compact_error" in event]
    if scenario in ("success", "prompt-too-long"):
        assert len(boundaries) == 1, "Expected one successful compact boundary"
        assert any(event.get("compact_result") == "success" for event in statuses)
        summary_index = next(i for i, request in enumerate(requests) if is_summary(request["body"]))
        parent = requests[summary_index - 1]["body"]
        for key in ("tools", "thinking", "max_tokens", "context_management", "output_config"):
            assert summaries[0].get(key) == parent.get(key), f"Parent {key} changed during compact"
        assert "PRIORITY_MARKER_42" in json.dumps(requests[-1]["body"]["messages"])
        if scenario == "prompt-too-long":
            assert len(summaries) == 2, "Expected one prompt-too-long retry"
            assert summaries[1]["messages"][0] == summaries[0]["messages"][0]
            assert len(summaries[1]["messages"]) < len(summaries[0]["messages"])
    elif scenario in ("empty", "whitespace"):
        assert len(summaries) == 1 and not boundaries
        assert errors == [EMPTY_ERROR], errors
    elif scenario == "too-few":
        assert not summaries and not boundaries
        assert errors == ["Not enough messages to compact."], errors
    else:
        assert not requests and not boundaries and not statuses
        assert "Error: No messages to compact" in json.dumps(events)
    return {
        "scenario": scenario, "passed": True, "requests": len(requests),
        "summary_requests": len(summaries), "compact_errors": errors,
        "compact_metadata": [event["compact_metadata"] for event in boundaries],
        "summary_request_options": [
            {key: body.get(key) for key in ("model", "max_tokens", "thinking", "tool_choice")}
            | {"tools": [tool["name"] for tool in body.get("tools", [])],
               "message_roles": [message["role"] for message in body["messages"]]}
            for body in summaries
        ],
    }


def run_scenario(binary: Path, root: Path, scenario: str, timeout: float) -> dict:
    root.mkdir(parents=True, exist_ok=False)
    for name in ("config", "work", "tmp"):
        (root / name).mkdir()
    requests: list[dict] = []
    dump(root / "requests.json", requests)
    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(root, scenario, requests))
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    base_url = f"http://127.0.0.1:{server.server_port}"
    command = [
        str(binary), "--bare", "-p", "--input-format", "stream-json", "--output-format",
        "stream-json", "--verbose", "--replay-user-messages", "--include-partial-messages",
        "--model", "claude-sonnet-4-6", "--setting-sources", "", "--strict-mcp-config",
        "--mcp-config", '{"mcpServers":{}}', "--system-prompt",
        "You are testing compact protocol against a local mock server. Keep responses short.",
        "--debug-file", str(root / "debug.log"),
    ]
    dump(root / "metadata.json", {"command": command, "base_url": base_url})
    process = None
    readers = []
    events: list[dict] = []
    output: queue.Queue = queue.Queue()

    def drain(pipe, filename):
        with (root / filename).open("w") as file:
            for line in pipe:
                file.write(line)
                file.flush()
                if filename == "stdout.jsonl":
                    output.put(line)

    deadline = time.monotonic() + timeout
    try:
        process = subprocess.Popen(
            command, cwd=root / "work", env=isolated_env(root, base_url),
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True, bufsize=1, start_new_session=True,
        )
        for pipe, filename in ((process.stdout, "stdout.jsonl"), (process.stderr, "stderr.log")):
            reader = threading.Thread(target=drain, args=(pipe, filename), daemon=True)
            reader.start()
            readers.append(reader)
        turns = 0 if scenario == "no-messages" else 1 if scenario == "too-few" else 2
        if scenario == "prompt-too-long":
            turns = 4
        prompts = [f"User turn {i + 1}: preserve PRIORITY_MARKER_42 and protocol detail {i + 1}."
                   for i in range(turns)] + [f"/compact {FOCUS}"]
        if turns > 1:
            prompts.append("What do you remember?")
        for prompt in prompts:
            payload = {"type": "user", "message": {"role": "user", "content": prompt},
                       "uuid": str(uuid.uuid4())}
            process.stdin.write(json.dumps(payload) + "\n")
            process.stdin.flush()
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"{scenario}: no result within {timeout}s; see {root}")
                try:
                    event = json.loads(output.get(timeout=min(remaining, 0.5)))
                except queue.Empty:
                    if process.poll() is not None:
                        raise RuntimeError(f"Claude exited {process.returncode}; see {root / 'stderr.log'}")
                    continue
                events.append(event)
                if event.get("type") == "result":
                    break
        process.stdin.close()
        process.wait(timeout=max(0.1, deadline - time.monotonic()))
        assert process.returncode == 0, f"Claude exited {process.returncode}"
    finally:
        if process is not None and process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=3)
        for reader in readers:
            reader.join(timeout=2)
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=2)
    # Read again after EOF so the final completed lifecycle event is included.
    events = [json.loads(line) for line in (root / "stdout.jsonl").read_text().splitlines()]
    report = check_capture(scenario, events, requests)
    dump(root / "report.json", report)
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", required=True, type=Path, help="New output directory (usually under /tmp)")
    parser.add_argument("--expected-version", default="2.1.261")
    parser.add_argument("--scenario", choices=("all",) + SCENARIOS, default="all")
    parser.add_argument("--timeout", type=float, default=45, help="Seconds per scenario")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    binary = args.binary.resolve(strict=True)
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    for name in ("config", "tmp"):
        (root / name).mkdir()
    version = subprocess.run(
        [str(binary), "--bare", "--version"], cwd=root, env=isolated_env(root, "http://127.0.0.1:1"),
        capture_output=True, text=True, check=True, timeout=min(args.timeout, 10),
    ).stdout.strip()
    if not re.match(rf"^{re.escape(args.expected_version)}(?:\s|$)", version):
        raise RuntimeError(f"Expected Claude Code {args.expected_version}; got {version!r}")
    scenarios = SCENARIOS[:4] if args.scenario == "all" else (args.scenario,)
    reports = []
    for scenario in scenarios:
        report = run_scenario(binary, root / scenario, scenario, args.timeout)
        reports.append(report)
        print(f"PASS {scenario}: {report['requests']} loopback requests", flush=True)
    dump(root / "report.json", {"version": version, "scenarios": reports})
    print(root / "report.json")


if __name__ == "__main__":
    main()
