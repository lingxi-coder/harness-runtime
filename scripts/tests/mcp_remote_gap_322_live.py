#!/usr/bin/env python3
"""Isolated native HTTP/SSE gap probes, retaining complete raw HTTP requests."""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import queue
import select
import signal
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from compact_live_oracle import isolated_env
from mcp_hooks_322_live import SHA256, blocks, make_handler as make_api_handler

SCENARIOS = (
    "raw-defaults", "invalid-json", "invalid-schema", "invalid-array-atomic",
    "sse-invalid-then-valid", "post-resume", "get-reconnect", "get-retry-exhausted",
    "redirect307", "redirect302", "redirect-loop", "redirect-cross-origin",
    "interrupt-headers", "interrupt-body", "legacy-reconnect", "reconnect-soak",
    "sse-schema-then-valid", "legacy-configured", "post-empty-stream", "long-idle",
    "http-error-text", "post-resume-exhausted",
    "concurrent-syntax",
    "json-overflow", "sse-event-overflow",
)
GOOD = "REMOTE_GAP_RESULT_322"


def encoded(value):
    return json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode()


def dump(path, value):
    path.write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n")


class HeaderCapture:
    def __init__(self, stream):
        self.stream, self.lines = stream, []

    def readline(self, *args):
        line = self.stream.readline(*args)
        self.lines.append(line)
        return line

    def __getattr__(self, name):
        return getattr(self.stream, name)


def run(binary, root, scenario, timeout):
    root.mkdir(parents=True, exist_ok=False)
    for name in ("config", "work", "tmp"):
        (root / name).mkdir()
    (root / "config/.claude.json").write_text('{"hasCompletedOnboarding":true}')
    requests, refused, traffic, events = [], [], [], []
    lock, stop, called = threading.Lock(), threading.Event(), threading.Event()
    state = {"gets": 0, "resume_id": None, "call_id": None, "legacy_queue": queue.Queue()}

    class Peer(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_args):
            pass

        def parse_request(self):
            original = self.rfile
            capture = HeaderCapture(original)
            self.rfile = capture
            try:
                valid = super().parse_request()
                self.header_bytes = b"".join(capture.lines)
                return valid
            finally:
                self.rfile = original

        def record(self, raw=b""):
            with lock:
                index = len(traffic) + 1
                data = self.raw_requestline + self.header_bytes + raw
                (root / f"http-{index:03}.raw").write_bytes(data)
                body = json.loads(raw) if raw else None
                traffic.append({"index": index, "method": self.command, "path": self.path,
                    "headers": list(self.headers.items()), "body": body,
                    "raw_sha256": hashlib.sha256(data).hexdigest(), "time": time.monotonic()})
                dump(root / "traffic.json", traffic)
            return body

        def respond(self, status=200, raw=b"", content_type="application/json", headers=None):
            self.send_response(status)
            if content_type:
                self.send_header("Content-Type", content_type)
            for key, value in (headers or {}).items():
                self.send_header(key, value)
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            try:
                self.wfile.write(raw)
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass

        def stream(self, raw):
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(raw)
            self.wfile.flush()

        def wait_closed(self):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline and not stop.is_set():
                readable, _, _ = select.select([self.connection], [], [], 0.05)
                if readable:
                    try:
                        if not self.connection.recv(1, __import__("socket").MSG_PEEK):
                            events.append({"event": "request-socket-closed", "time": time.monotonic()})
                            return
                    except (ConnectionResetError, OSError):
                        events.append({"event": "request-socket-reset", "time": time.monotonic()})
                        return

        def do_DELETE(self):
            self.record()
            self.respond(204)

        def do_GET(self):
            self.record()
            state["gets"] += 1
            number = state["gets"]
            if self.path == "/landing":
                return self.respond(200, b"redirected GET", "text/plain")
            cursor = self.headers.get("Last-Event-ID")
            if scenario == "post-resume-exhausted" and cursor == "post-cursor":
                return self.respond(503, b"replay unavailable", "text/plain")
            if scenario == "post-resume" and cursor == "post-cursor":
                response = {"jsonrpc": "2.0", "id": state["resume_id"], "result": {"content": [{"type": "text", "text": GOOD}]}}
                return self.respond(200, b"id: resumed\ndata: " + encoded(response) + b"\n\n", "text/event-stream")
            if scenario in ("get-reconnect", "reconnect-soak", "get-retry-exhausted"):
                if scenario == "get-retry-exhausted" and number > 1:
                    return self.respond(503, b"retry exhausted", "text/plain")
                raw = f"retry: 10\nid: get-{number}\ndata: ".encode() + encoded({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": "unmatched", "progress": number}}) + b"\n\n"
                return self.respond(200, raw, "text/event-stream")
            if scenario == "long-idle":
                self.stream(b": attached\n\n")
                stop.wait(110)
                self.close_connection = True
                return
            if scenario.startswith("legacy-"):
                self.stream(f"retry: 10\nevent: endpoint\ndata: /messages?stream={number}\n\n".encode())
                try:
                    while not stop.is_set():
                        try:
                            response = state["legacy_queue"].get(timeout=0.1)
                        except queue.Empty:
                            continue
                        self.wfile.write(f"id: legacy-{number}\ndata: ".encode() + encoded(response) + b"\n\n")
                        self.wfile.flush()
                        if number == 1 and "tools" in response.get("result", {}):
                            break
                except (BrokenPipeError, ConnectionResetError):
                    pass
                self.close_connection = True
                return
            self.respond(405)

        def do_POST(self):
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            frame = self.record(raw)
            method, rid = frame.get("method"), frame.get("id")
            if scenario.startswith("redirect") and self.path == "/mcp":
                status = 302 if scenario == "redirect302" else 307
                destination = "/mcp" if scenario == "redirect-loop" else "/landing"
                if scenario == "redirect-cross-origin":
                    destination = f"http://127.0.0.1:{cross.server_port}/landing"
                return self.respond(status, b"", None, {"Location": destination})
            if rid is None:
                return self.respond(202)
            result = {}
            if method == "initialize":
                result = {"protocolVersion": frame["params"]["protocolVersion"], "capabilities": {"tools": {"listChanged": False}}, "serverInfo": {"name": "gap", "version": "1"}}
            elif method == "tools/list":
                result = {"tools": [{"name": "echo", "description": "Loopback gap probe", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}}]}
            elif method == "tools/call":
                state["call_id"] = rid
                called.set()
                result = {"content": [{"type": "text", "text": GOOD}]}
            elif method == "resources/list":
                result = {"resources": []}
            elif method == "resources/templates/list":
                result = {"resourceTemplates": []}
            elif method == "prompts/list":
                result = {"prompts": []}
            response = {"jsonrpc": "2.0", "id": rid, "result": result}
            if method == "server/discover":
                response = {"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "Method not found"}}
            if scenario.startswith("legacy-"):
                if method == "tools/call":
                    deadline = time.monotonic() + 5
                    while state["gets"] < 2 and time.monotonic() < deadline:
                        time.sleep(0.01)
                state["legacy_queue"].put(response)
                return self.respond(202)
            if method == "tools/call":
                if scenario in ("json-overflow", "sse-event-overflow") and frame.get("params", {}).get("arguments", {}).get("value") != "AFTER":
                    response["result"]["content"][0]["text"] = "x" * (17 * 1024 * 1024)
                    raw = encoded(response)
                    if scenario == "sse-event-overflow":
                        return self.respond(200, b"data: " + raw + b"\n\n", "text/event-stream")
                    return self.respond(200, raw)
                if scenario.startswith("interrupt"):
                    if scenario == "interrupt-body":
                        self.stream(b'id: interrupt-body\ndata: {"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"unmatched","progress":1}}\n\n')
                    self.wait_closed()
                    self.close_connection = True
                    return
                if scenario == "concurrent-syntax":
                    if frame.get("params", {}).get("arguments", {}).get("value") == "HOLD":
                        self.send_response(200)
                        self.send_header("Content-Type", "application/json")
                        self.send_header("Connection", "close")
                        self.end_headers()
                        self.wait_closed()
                        self.close_connection = True
                        return
                    return self.respond(200, b"{")
                if scenario == "invalid-json":
                    return self.respond(200, b"{")
                if scenario == "http-error-text":
                    return self.respond(500, "REMOTE_ERROR_Ω".encode(), "text/plain; charset=iso-8859-1")
                if scenario == "invalid-schema":
                    return self.respond(200, encoded({"jsonrpc": "2.0", "id": rid, "result": []}))
                if scenario == "invalid-array-atomic":
                    return self.respond(200, encoded([response, {"invalid": "frame"}]))
                if scenario == "sse-invalid-then-valid":
                    return self.respond(200, b'data: {\n\ndata: {"jsonrpc":"2.0","result":[]}\n\ndata: ' + encoded(response) + b"\n\n", "text/event-stream")
                if scenario == "sse-schema-then-valid":
                    return self.respond(200, b'data: {"jsonrpc":"2.0","result":[]}\n\ndata: ' + encoded(response) + b"\n\n", "text/event-stream")
                if scenario == "post-empty-stream":
                    return self.respond(200, b"", "text/event-stream")
                if scenario in ("post-resume", "post-resume-exhausted"):
                    state["resume_id"] = rid
                    return self.respond(200, b'retry: 5\n\nid: post-cursor\ndata: {"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"unmatched","progress":1}}\n\n', "text/event-stream")
                if scenario in ("get-reconnect", "reconnect-soak", "get-retry-exhausted"):
                    count = 30 if scenario == "reconnect-soak" else 5 if scenario == "get-reconnect" else 3
                    deadline = time.monotonic() + 5
                    while state["gets"] < count and time.monotonic() < deadline:
                        time.sleep(0.01)
                if scenario == "long-idle":
                    time.sleep(92)
            headers = {"Mcp-Session-Id": "gap-session"} if method == "initialize" else None
            self.respond(200, encoded(response), headers=headers)

    api_handler = make_api_handler(root, requests, refused)
    if scenario == "json-overflow":
        class AfterOutput:
            def __init__(self, output, name):
                self.output, self.name = output, name

            def __getattr__(self, name):
                return getattr(self.output, name)

            def write(self, raw):
                if raw.startswith(b"event:"):
                    event = json.loads(raw.split(b"data: ", 1)[1])
                    if event["type"] == "content_block_start":
                        event["content_block"] = {"type": "tool_use", "id": "toolu_after_322", "name": self.name, "input": {}}
                    elif event["type"] == "content_block_delta":
                        event["delta"] = {"type": "input_json_delta", "partial_json": '{"value":"AFTER"}'}
                    elif event["type"] == "message_delta":
                        event["delta"]["stop_reason"] = "tool_use"
                    raw = b"event: " + event["type"].encode() + b"\ndata: " + encoded(event) + b"\n\n"
                return self.output.write(raw)

        class AfterAPI(api_handler):
            def do_POST(self):
                raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                body = json.loads(raw)
                results = [value for value in blocks(body) if value.get("type") == "tool_result"]
                output, source = self.wfile, self.rfile
                self.rfile = io.BytesIO(raw)
                if results and not any(value.get("tool_use_id") == "toolu_after_322" for value in results):
                    name = next(value["name"] for value in body["tools"] if value["name"].startswith("mcp__"))
                    self.wfile = AfterOutput(output, name)
                try:
                    super().do_POST()
                finally:
                    self.wfile, self.rfile = output, source
        api_handler = AfterAPI
    if scenario == "concurrent-syntax":
        class DoubleOutput:
            def __init__(self, output):
                self.output, self.tool_name = output, None

            def __getattr__(self, name):
                return getattr(self.output, name)

            def write(self, raw):
                extra = b""
                if raw.startswith(b"event:"):
                    event = json.loads(raw.split(b"data: ", 1)[1])
                    if event.get("content_block", {}).get("type") == "tool_use":
                        self.tool_name = event["content_block"]["name"]
                    if self.tool_name and event["type"] == "content_block_delta":
                        event["delta"]["partial_json"] = '{"value":"HOLD"}'
                        raw = b"event: " + event["type"].encode() + b"\ndata: " + encoded(event) + b"\n\n"
                    if self.tool_name and event["type"] == "content_block_stop":
                        second = [
                            {"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_broken_322", "name": self.tool_name, "input": {}}},
                            {"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": '{"value":"BROKEN"}'}},
                            {"type": "content_block_stop", "index": 1},
                        ]
                        extra = b"".join(b"event: " + value["type"].encode() + b"\ndata: " + encoded(value) + b"\n\n" for value in second)
                with (root / "model-response.raw").open("ab") as output:
                    output.write(raw + extra)
                return self.output.write(raw + extra)

        class DoubleAPI(api_handler):
            def do_POST(self):
                output = self.wfile
                self.wfile = DoubleOutput(output)
                try:
                    super().do_POST()
                finally:
                    self.wfile = output
        api_handler = DoubleAPI
    api = ThreadingHTTPServer(("127.0.0.1", 0), api_handler)
    peer = ThreadingHTTPServer(("127.0.0.1", 0), Peer)
    cross = ThreadingHTTPServer(("127.0.0.1", 0), Peer)
    for server in (api, peer, cross):
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
    config = {"type": "sse" if scenario.startswith("legacy-") else "http", "url": f"http://127.0.0.1:{peer.server_port}/mcp", "alwaysLoad": True}
    if scenario == "redirect-cross-origin":
        config["headers"] = {"Authorization": "Bearer fake-loopback-only", "X-Wire-Identity": "gap322"}
    if scenario == "legacy-configured":
        config["headers"] = {"Accept": "application/custom", "Content-Type": "text/custom"}
    dump(root / "mcp.json", {"mcpServers": {"gap": config}})
    env = isolated_env(root, f"http://127.0.0.1:{api.server_port}")
    env.update({"ENABLE_TOOL_SEARCH": "false", "MCP_TIMEOUT": "3000", "MCP_TOOL_TIMEOUT": "3000"})
    if scenario == "long-idle":
        env.update({"MCP_TIMEOUT": "120000", "MCP_TOOL_TIMEOUT": "120000"})
    args = [str(binary), "--bare", "-p", "--model", "claude-sonnet-4-6", "--system-prompt", "GAP_SYSTEM", "--output-format", "stream-json", "--verbose", "--setting-sources", "", "--strict-mcp-config", "--mcp-config", str(root / "mcp.json"), "--no-session-persistence", "--tools", "", "--permission-mode", "bypassPermissions", "--dangerously-skip-permissions", "--debug-file", str(root / "debug.log")]
    interactive = scenario.startswith("interrupt")
    args += ["--input-format", "stream-json"] if interactive else ["GAP_REMOTE_322"]
    dump(root / "invocation.json", {"argv": args, "native_sha256": SHA256, "fake_credentials": True, "loopback_only": True})
    process = subprocess.Popen(args, cwd=root / "work", env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    try:
        if interactive:
            def send(value):
                data = encoded(value) + b"\n"
                with (root / "stdin.jsonl").open("ab") as output:
                    output.write(data)
                process.stdin.write(data)
                process.stdin.flush()
            send({"type": "control_request", "request_id": "init", "request": {"subtype": "initialize"}})
            send({"type": "user", "message": {"role": "user", "content": "Call the gap echo tool."}})
            assert called.wait(15), "native tool call did not start"
            send({"type": "control_request", "request_id": "stop", "request": {"subtype": "interrupt"}})
            time.sleep(0.5)
            process.stdin.close()
            process.stdin = None
        stdout, stderr = process.communicate(timeout=timeout)
    except BaseException:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate()
        (root / "stdout.jsonl").write_bytes(stdout)
        (root / "stderr.txt").write_bytes(stderr)
        raise
    finally:
        stop.set()
        for server in (api, peer, cross):
            server.shutdown()
            server.server_close()
    (root / "stdout.jsonl").write_bytes(stdout)
    (root / "stderr.txt").write_bytes(stderr)
    results = [b for row in requests for b in blocks(row["body"]) if b.get("type") == "tool_result"]
    report = {"scenario": scenario, "native_sha256": SHA256, "exit_code": process.returncode,
        "tool_results": results, "gets": state["gets"], "events": events,
        "delete_count": sum(row["method"] == "DELETE" for row in traffic),
        "cancelled_notifications": [row["body"] for row in traffic if (row["body"] or {}).get("method") == "notifications/cancelled"],
        "traffic": traffic, "refused_api_paths": refused}
    dump(root / "report.json", report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scenario", choices=("all", *SCENARIOS), default="all")
    parser.add_argument("--timeout", type=float, default=25)
    args = parser.parse_args()
    assert hashlib.sha256(args.binary.read_bytes()).hexdigest() == SHA256
    reports = []
    for scenario in SCENARIOS if args.scenario == "all" else [args.scenario]:
        report = run(args.binary, args.output / scenario, scenario, args.timeout)
        reports.append({"scenario": scenario, "exit_code": report["exit_code"], "tool_results": report["tool_results"], "gets": report["gets"], "events": report["events"], "delete_count": report["delete_count"], "cancelled_notifications": report["cancelled_notifications"]})
        print(json.dumps({key: value for key, value in reports[-1].items() if key != "tool_results"}, ensure_ascii=True), flush=True)
    dump(args.output / "report.json", {"native_version": "2.1.322", "native_sha256": SHA256, "cases": reports})


if __name__ == "__main__":
    main()
