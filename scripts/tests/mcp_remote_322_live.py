#!/usr/bin/env python3
"""Capture 2.1.322 native MCP HTTP/SSE traffic against local scripted peers.

Only the reference CLI's model API is Anthropic-shaped. The MCP peer and its
captured transport contract are independent of any LLM provider. No real API
keys or network services are used.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from compact_live_oracle import isolated_env
from mcp_hooks_322_live import SHA256, blocks, make_handler as make_api_handler


VERSION = "2.1.322"
SCENARIOS = (
    "http-json", "http-sse-events", "http-json-array", "http-invalid-type",
    "http-configured-headers", "http-legacy-fallback", "sse-direct", "sse-invalid-type",
    "http-get-plain-stream", "sse-uppercase-type",
)
SERVER_NAME = "wire remote"
GOOD_TEXT = "WIRE_REMOTE_RESULT_322"
WRONG_TEXT = "WRONG_NAMED_EVENT_MUST_BE_IGNORED"
SESSION_ID = "wire-session-322"


def encoded(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode()


def dump(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n")


def make_mcp_handler(root: Path, scenario: str, traffic: list[dict], stop: threading.Event):
    lock = threading.Lock()
    legacy_replies = queue.Queue()
    legacy = scenario in ("http-legacy-fallback", "sse-direct", "sse-invalid-type", "sse-uppercase-type")

    class Peer(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def record(self, raw: bytes) -> dict | None:
            body = json.loads(raw) if raw else None
            with lock:
                index = len(traffic) + 1
                traffic.append({"index": index, "method": self.command, "path": self.path,
                                "request_line": self.requestline, "headers": list(self.headers.items()),
                                "body": body, "body_sha256": hashlib.sha256(raw).hexdigest()})
                (root / f"mcp-{index:02}-{self.command}.raw").write_bytes(raw)
                dump(root / "mcp-traffic.json", traffic)
            return body

        def respond(self, status: int, raw: bytes = b"", content_type: str | None = None,
                    headers: dict[str, str] | None = None) -> None:
            self.send_response(status)
            if content_type:
                self.send_header("Content-Type", content_type)
            for name, value in (headers or {}).items():
                self.send_header(name, value)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            try:
                self.wfile.write(raw)
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass

        def do_GET(self):
            self.record(b"")
            if not legacy and scenario != "http-get-plain-stream":
                return self.respond(405)
            self.send_response(200)
            content_type = "text/plain" if scenario in ("sse-invalid-type", "http-get-plain-stream") else "text/event-stream"
            if scenario == "sse-uppercase-type":
                content_type = "Text/Event-Stream"
            self.send_header("Content-Type", content_type)
            self.send_header("Cache-Control", "no-cache")
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                if scenario == "http-get-plain-stream":
                    time.sleep(0.05)
                    self.wfile.write(b'data: {"jsonrpc":"2.0","method":"notifications/tools/list_changed"}\r\n\r\n')
                    self.wfile.flush()
                    stop.wait(10)
                    self.close_connection = True
                    return
                self.wfile.write(b"event: endpoint\r\ndata: /messages?session=legacy322\r\n\r\n")
                self.wfile.flush()
                while not stop.is_set():
                    try:
                        frame = legacy_replies.get(timeout=0.2)
                    except queue.Empty:
                        continue
                    if frame.get("result", {}).get("content"):
                        wrong = {**frame, "result": {"content": [{"type": "text", "text": WRONG_TEXT}]}}
                        self.wfile.write(b"event: other\r\ndata: " + encoded(wrong) + b"\r\n\r\n")
                    self.wfile.write(b"event: message\r\ndata: " + encoded(frame) + b"\r\n\r\n")
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            self.close_connection = True

        def do_DELETE(self):
            self.record(b"")
            self.respond(200)

        def do_POST(self):
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            frame = self.record(raw)
            if self.path == "/mcp" and scenario == "http-legacy-fallback":
                return self.respond(405, b"legacy stream endpoint", "text/plain")
            if not isinstance(frame, dict):
                return self.respond(400)
            method, request_id = frame.get("method"), frame.get("id")
            if request_id is None:
                return self.respond(202)
            response = {"jsonrpc": "2.0", "id": request_id}
            if method == "server/discover":
                response["error"] = {"code": -32601, "message": "Method not found"}
            elif method == "initialize":
                response["result"] = {"protocolVersion": frame["params"]["protocolVersion"],
                    "capabilities": {"tools": {"listChanged": scenario == "http-get-plain-stream"}},
                    "serverInfo": {"name": "remote322", "version": "1"}}
            elif method == "tools/list":
                response["result"] = {"tools": [{"name": "echo", "description": "Transport oracle tool",
                    "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}}},
                    "annotations": {"readOnlyHint": True}}]}
            elif method == "tools/call":
                response["result"] = {"content": [{"type": "text", "text": GOOD_TEXT}]}
            elif method == "resources/list":
                response["result"] = {"resources": []}
            elif method == "resources/templates/list":
                response["result"] = {"resourceTemplates": []}
            elif method == "prompts/list":
                response["result"] = {"prompts": []}
            else:
                response["result"] = {}
            if legacy:
                legacy_replies.put(response)
                return self.respond(202)
            headers = {"Mcp-Session-Id": SESSION_ID} if method == "initialize" else {}
            if method == "tools/call" and scenario == "http-invalid-type":
                return self.respond(200, encoded(response), "text/plain")
            if method == "tools/call" and scenario == "http-json-array":
                return self.respond(200, encoded([response]), "application/json")
            if method == "tools/call" and scenario == "http-sse-events":
                wrong = {**response, "result": {"content": [{"type": "text", "text": WRONG_TEXT}]}}
                raw = b"event: other\r\nid: named-event\r\ndata: " + encoded(wrong) + b"\r\n\r\n"
                # Multi-line data and CRLF framing are valid SSE; JSON accepts
                # the inserted newline between the two object members.
                answer = encoded(response).replace(b',"result":', b',\r\ndata: "result":', 1)
                raw += b"event: message\r\ndata: " + answer + b"\r\n\r\n"
                return self.respond(200, raw, "text/event-stream; charset=utf-8")
            self.respond(200, encoded(response), "Application/JSON; charset=utf-8", headers)

    return Peer


def result_texts(result: dict) -> list[str]:
    content = result.get("content", [])
    return [content] if isinstance(content, str) else [block.get("text", "") for block in content]


def assert_header_contract(traffic: list[dict], scenario: str) -> None:
    posts = [row for row in traffic if row["method"] == "POST"]
    for row in posts:
        headers = {key.lower(): value for key, value in row["headers"]}
        assert headers.get("content-type") == "application/json", headers
        expected_accept = "application/json, text/event-stream"
        if scenario == "http-configured-headers":
            expected_accept = "application/custom, " + expected_accept
            assert headers.get("x-dupe") == "one, two", headers
        assert headers.get("accept") == expected_accept, headers
        method = (row["body"] or {}).get("method")
        if method == "initialize":
            assert "mcp-session-id" not in headers, headers
        elif method in ("notifications/initialized", "tools/list", "tools/call"):
            expected_version = "2025-11-25"
            if scenario == "http-configured-headers":
                expected_version += ", configured-version"
            assert headers.get("mcp-protocol-version") == expected_version, headers
    for row in [row for row in traffic if row["method"] == "GET"]:
        headers = {key.lower(): value for key, value in row["headers"]}
        expected_accept = "text/event-stream"
        if scenario == "http-configured-headers":
            expected_accept = "application/custom, text/event-stream"
            assert headers.get("content-type") == "text/custom", headers
        else:
            assert "content-type" not in headers, headers
        assert headers.get("accept") == expected_accept, headers


def run(binary: Path, root: Path, scenario: str, timeout: float) -> dict:
    root.mkdir(parents=True)
    for directory in ("config", "work", "tmp"):
        (root / directory).mkdir()
    (root / "config" / ".claude.json").write_text('{"hasCompletedOnboarding":true}')
    requests, refused, traffic = [], [], []
    stop = threading.Event()
    api_handler = make_api_handler(root, requests, refused)
    if scenario == "http-get-plain-stream":
        class DelayedAPI(api_handler):
            def do_POST(self):
                time.sleep(0.25)
                super().do_POST()
        api_handler = DelayedAPI
    api = ThreadingHTTPServer(("127.0.0.1", 0), api_handler)
    peer = ThreadingHTTPServer(("127.0.0.1", 0), make_mcp_handler(root, scenario, traffic, stop))
    threads = [threading.Thread(target=server.serve_forever, daemon=True) for server in (api, peer)]
    for thread in threads:
        thread.start()
    transport = "sse" if scenario.startswith("sse-") else "http"
    config = {"type": transport, "url": f"http://127.0.0.1:{peer.server_port}/mcp", "alwaysLoad": True}
    if scenario == "http-configured-headers":
        config["headers"] = {"Accept": "application/custom", "Content-Type": "text/custom",
                             "Mcp-Session-Id": "configured-session", "Mcp-Protocol-Version": "configured-version",
                             "X-Wire-Probe": "remote322", "X-Dupe": "one", "x-dupe": "two"}
    dump(root / "mcp-config.json", {"mcpServers": {SERVER_NAME: config}})
    base_url = f"http://127.0.0.1:{api.server_port}"
    env = isolated_env(root, base_url)
    env.update({"CLAUDE_CODE_DISABLE_FEATURE_FLAGS": "1", "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1",
                "ENABLE_TOOL_SEARCH": "false", "MCP_TIMEOUT": "4000", "MCP_TOOL_TIMEOUT": "4000"})
    command = [str(binary), "--bare", "-p", "WIRE_REMOTE_322", "--model", "claude-sonnet-4-6",
               "--system-prompt", "WIRE_SYSTEM", "--output-format", "stream-json", "--verbose",
               "--setting-sources", "", "--strict-mcp-config", "--mcp-config", str(root / "mcp-config.json"),
               "--disable-slash-commands", "--no-session-persistence", "--tools", "",
               "--permission-mode", "bypassPermissions", "--dangerously-skip-permissions",
               "--debug-file", str(root / "debug.log")]
    dump(root / "invocation.json", {"command": command, "native_sha256": SHA256, "fake_credentials": True})
    process = subprocess.Popen(command, cwd=root / "work", env=env, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            stdout, stderr = process.communicate(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate(timeout=3)
        (root / "stdout.jsonl").write_text(stdout)
        (root / "stderr.txt").write_text(stderr)
        raise
    finally:
        stop.set()
        for server in (api, peer):
            server.shutdown()
            server.server_close()
        for thread in threads:
            thread.join(timeout=2)
    (root / "stdout.jsonl").write_text(stdout)
    (root / "stderr.txt").write_text(stderr)
    assert process.returncode == 0, (scenario, process.returncode, stderr[-2000:], stdout[-1500:])
    model_requests = [row for row in requests if "count_tokens" not in row["path"]]
    results = [block for row in model_requests for block in blocks(row["body"])
               if block.get("type") == "tool_result" and block.get("tool_use_id") == "toolu_mcp_hooks_322"]
    if scenario in ("sse-invalid-type", "sse-uppercase-type"):
        assert traffic and not results
        assert not any(row["method"] == "POST" for row in traffic), "invalid SSE MIME must fail before POST"
    else:
        assert results, "native MCP call did not produce a model tool result"
        text = "\n".join(result_texts(results[-1]))
        if scenario == "http-invalid-type":
            assert results[-1].get("is_error") and "Unexpected content type: text/plain" in text, text
        else:
            assert not results[-1].get("is_error") and GOOD_TEXT in text and WRONG_TEXT not in text, text
    if scenario == "http-get-plain-stream":
        assert sum((row["body"] or {}).get("method") == "tools/list" for row in traffic) >= 2, "plain-type GET notification was not processed"
    assert_header_contract(traffic, scenario)
    report = {"scenario": scenario, "passed": True, "exit_code": process.returncode,
              "transport": transport, "model_requests": len(model_requests),
              "tool_result": results[-1] if results else None,
              "mcp_requests": [{"method": row["method"], "path": row["path"],
                  "rpc_method": (row["body"] or {}).get("method"), "headers": row["headers"]} for row in traffic],
              "refused_api_paths": refused}
    dump(root / "report.json", report)
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scenario", choices=("all", *SCENARIOS), default="all")
    parser.add_argument("--timeout", type=float, default=35)
    args = parser.parse_args()
    digest = hashlib.sha256()
    binary = args.binary.resolve(strict=True)
    with binary.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    assert digest.hexdigest() == SHA256, "native artifact changed"
    args.output.mkdir(parents=True, exist_ok=True)
    reports = []
    for scenario in SCENARIOS if args.scenario == "all" else [args.scenario]:
        report = run(binary, args.output / scenario, scenario, args.timeout)
        reports.append(report)
        print(f"PASS {scenario}: {len(report['mcp_requests'])} MCP HTTP requests", flush=True)
    dump(args.output / "report.json", {"version": VERSION, "native_sha256": SHA256,
        "scope": "Actual native process with isolated fake-credential model API and loopback MCP peers. No external auth, production service, cancellation, or reconnect soak acceptance.",
        "scenarios": reports})


if __name__ == "__main__":
    main()
