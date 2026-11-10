#!/usr/bin/env python3
"""Capture SHA-pinned Claude Code MCP/hook behavior using only loopback peers.

The native process receives an isolated config, fake API key, scripted API, and
stdio MCP server. Raw requests and hook stdin are retained in --output. This is
a controlled native execution receipt, not a live-provider acceptance test.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from compact_live_oracle import isolated_env


VERSION = "2.1.322"
SHA256 = "a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450"
SERVER_NAME = "wire audit"
DESCRIPTION = "MCP_DESCRIPTION_322:" + "x" * 20_000
SCENARIOS = ("success", "logical-error", "rpc-error", "connection-close", "permission-request", "deferred-description")
EXTRA_SCENARIOS = ("permission-denied-auto",)
EVENTS = ("PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest", "PermissionDenied")
SELF = Path(__file__).resolve()


def dump(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, ensure_ascii=True, indent=2) + "\n")


def append(path: Path, raw: bytes) -> None:
    with path.open("ab") as output:
        output.write(raw)


def capture_hook(output: Path) -> None:
    raw = sys.stdin.buffer.read()
    payload = json.loads(raw)
    event = payload["hook_event_name"]
    (output / f"{event}.{hashlib.sha256(raw).hexdigest()[:12]}.raw.json").write_bytes(raw)
    if event == "PermissionRequest":
        print('{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}')


def stdio_server(output: Path, scenario: str, tool_name: str = "echo", expected_wire_tool_name: str | None = None) -> None:
    expected_wire_tool_name = tool_name if expected_wire_tool_name is None else expected_wire_tool_name
    def send(value: dict) -> None:
        raw = json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode() + b"\n"
        append(output / "mcp-out.raw.jsonl", raw)
        sys.stdout.buffer.write(raw)
        sys.stdout.buffer.flush()

    for raw in sys.stdin.buffer:
        append(output / "mcp-in.raw.jsonl", raw)
        request = json.loads(raw)
        method, request_id = request.get("method"), request.get("id")
        if request_id is None:
            continue
        if method == "initialize":
            result = {
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": "mcp-hooks-322-oracle", "version": "1"},
            }
        elif method == "tools/list":
            result = {"tools": [{
                "name": tool_name,
                "description": DESCRIPTION,
                "inputSchema": {"type": "object", "properties": {"value": {"type": "string"}}},
                "annotations": {"readOnlyHint": scenario not in ("permission-request", "permission-denied-auto")},
            }]}
        elif method == "tools/call":
            assert request["params"]["name"] == expected_wire_tool_name, "upstream tool name differs from the captured native catalog sanitizer"
            if scenario == "connection-close":
                return
            if scenario == "rpc-error":
                # A typed JSON-RPC error is a thrown call failure, distinct
                # from MCP's successful response envelope with isError=true.
                send({"jsonrpc": "2.0", "id": request_id, "error": {
                    "code": -32001, "message": "WIRE_RPC_FAILURE_322",
                }})
                continue
            result = {"content": [{"type": "text", "text": "WIRE_MCP_RESULT_322"}]}
            if scenario == "logical-error":
                result["isError"] = True
        elif method in ("ping", "logging/setLevel"):
            result = {}
        elif method == "resources/list":
            result = {"resources": []}
        elif method == "resources/templates/list":
            result = {"resourceTemplates": []}
        elif method == "prompts/list":
            result = {"prompts": []}
        else:
            send({"jsonrpc": "2.0", "id": request_id, "error": {
                "code": -32601, "message": "oracle method not found",
            }})
            continue
        send({"jsonrpc": "2.0", "id": request_id, "result": result})


def blocks(body: dict) -> list[dict]:
    return [block for message in body.get("messages", [])
            for block in message.get("content", []) if isinstance(block, dict)]


def normalized_server_name(name: str) -> str:
    raw = name.encode("utf-16-le", errors="surrogatepass")
    units = [int.from_bytes(raw[index:index + 2], "little") for index in range(0, len(raw), 2)]
    normalized = "".join(chr(unit) if unit < 128 and (chr(unit).isalnum() or chr(unit) in "_-") else "_" for unit in units)
    if name.startswith("claude.ai "):
        import re
        normalized = re.sub("_+", "_", normalized).strip("_")
    return normalized


def make_handler(root: Path, requests: list[dict], refused: list[str], server_name: str = SERVER_NAME, scenario: str = "success", tool_name: str = "echo"):
    lock = threading.Lock()

    class Mock(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reject(self):
            refused.append(self.path)
            self.send_error(403, "Only the loopback oracle is available")

        do_GET = reject
        do_CONNECT = reject

        def do_POST(self):
            if self.path.split("?")[0] not in ("/v1/messages", "/v1/messages/count_tokens"):
                return self.reject()
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            body = json.loads(raw)
            with lock:
                index = len(requests) + 1
                requests.append({"path": self.path, "body": body, "sha256": hashlib.sha256(raw).hexdigest()})
                (root / f"request-{index:02}.raw.json").write_bytes(raw)
                dump(root / "requests.json", requests)
            if "count_tokens" in self.path:
                return self.respond_json({"input_tokens": 100})
            tool_result = any(b.get("type") == "tool_result" and b.get("tool_use_id") == "toolu_mcp_hooks_322"
                              for b in blocks(body))
            tools = [t for t in body.get("tools", []) if t.get("name", "").startswith("mcp__")]
            classifier = scenario == "permission-denied-auto" and not tools and "<block>" in json.dumps(body)
            if classifier:
                content = [{"type": "text", "text": "<block>yes</block><category>Data Exfiltration</category><reason>[Data Exfiltration] WIRE_CLASSIFIER_DENIAL_322</reason>"}]
                reason = "end_turn"
            elif not tool_result and tools:
                content = [{"type": "tool_use", "id": "toolu_mcp_hooks_322", "name": tools[0]["name"],
                            "input": {"value": "WIRE_TOOL_INPUT_322"}}]
                reason = "tool_use"
            elif not tool_result and any(t.get("name") == "ToolSearch" for t in body.get("tools", [])):
                content = [{"type": "tool_use", "id": "toolu_search_322", "name": "ToolSearch",
                            "input": {"query": "select:mcp__" + normalized_server_name(server_name) + "__" + normalized_server_name(tool_name)}}]
                reason = "tool_use"
            else:
                content = [{"type": "text", "text": "WIRE_DONE_322"}]
                reason = "end_turn"
            message = {
                "id": f"msg_mcp_hooks_322_{index}", "type": "message", "role": "assistant",
                "model": body["model"], "content": [], "stop_reason": None,
                "stop_sequence": None, "usage": {"input_tokens": 100, "output_tokens": 0,
                "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0},
            }
            if not body.get("stream"):
                message.update(content=content, stop_reason=reason)
                message["usage"]["output_tokens"] = 10
                return self.respond_json(message)
            events = [{"type": "message_start", "message": message}]
            for i, block in enumerate(content):
                start = {**block, "input": {}} if block["type"] == "tool_use" else {"type": "text", "text": ""}
                delta = {"type": "input_json_delta", "partial_json": json.dumps(block["input"])} if block["type"] == "tool_use" else {"type": "text_delta", "text": block["text"]}
                events.extend([{"type": "content_block_start", "index": i, "content_block": start},
                               {"type": "content_block_delta", "index": i, "delta": delta},
                               {"type": "content_block_stop", "index": i}])
            events.extend([{"type": "message_delta", "delta": {"stop_reason": reason, "stop_sequence": None},
                            "usage": {"output_tokens": 10}}, {"type": "message_stop"}])
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
            try:
                for event in events:
                    self.wfile.write(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode())
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass

        def respond_json(self, body):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(json.dumps(body).encode())

    return Mock


def run(binary: Path, root: Path, scenario: str, timeout: float, server_name: str = SERVER_NAME, tool_name: str = "echo", expected_wire_tool_name: str | None = None) -> dict:
    root.mkdir(parents=True)
    for directory in ("config", "work", "tmp", "hooks"):
        (root / directory).mkdir()
    (root / "config" / ".claude.json").write_text('{"hasCompletedOnboarding":true}')
    hook_command = shlex.join([sys.executable, str(SELF), "--capture-hook", str(root / "hooks")])
    settings = {"hooks": {event: [{"hooks": [{"type": "command", "command": hook_command,
                                              "timeout": 5}]}] for event in EVENTS}}
    dump(root / "settings.json", settings)
    mcp_config = {"mcpServers": {server_name: {"command": sys.executable,
        "args": [str(SELF), "--stdio-server", str(root), scenario, json.dumps(tool_name), json.dumps(expected_wire_tool_name)],
        "alwaysLoad": scenario != "deferred-description"}}}
    dump(root / "mcp-config.json", mcp_config)
    requests, refused = [], []
    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(root, requests, refused, server_name, scenario, tool_name))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    base_url = f"http://127.0.0.1:{server.server_port}"
    env = isolated_env(root, base_url)
    env.update({"CLAUDE_CODE_DISABLE_FEATURE_FLAGS": "1", "CLAUDE_CODE_DISABLE_AUTO_MEMORY": "1",
                "ENABLE_TOOL_SEARCH": "true" if scenario == "deferred-description" else "false"})
    command = [str(binary), "-p", "WIRE_MCP_HOOKS_322", "--model", "claude-sonnet-4-6",
               "--system-prompt", "WIRE_SYSTEM", "--output-format", "stream-json", "--verbose",
               "--setting-sources", "", "--settings", str(root / "settings.json"),
               "--strict-mcp-config", "--mcp-config", str(root / "mcp-config.json"),
               "--disable-slash-commands", "--no-session-persistence", "--tools",
               "ToolSearch" if scenario == "deferred-description" else "",
               "--debug-file", str(root / "debug.log")]
    if scenario == "permission-denied-auto":
        # The native env switch selects its client-side classifier, whose
        # actual request and verdict parser run against the loopback peer.
        env["CLAUDE_CODE_AUTO_MODE_SERVER"] = "0"
        command += ["--permission-mode", "auto"]
    elif scenario != "permission-request":
        command += ["--permission-mode", "bypassPermissions", "--dangerously-skip-permissions"]
    dump(root / "invocation.json", {"command": command, "native_sha256": SHA256,
                                    "fake_credentials": True, "loopback_only": True})
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
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    (root / "stdout.jsonl").write_text(stdout)
    (root / "stderr.txt").write_text(stderr)
    assert process.returncode == 0, (process.returncode, stderr[-1500:], stdout[-1500:])
    messages = [request for request in requests if "count_tokens" not in request["path"]]
    assert len(messages) >= 2, "MCP tool was not called"
    hook_rows = []
    for path in sorted((root / "hooks").glob("*.raw.json")):
        raw = path.read_bytes()
        payload = json.loads(raw)
        if payload.get("tool_name", "").startswith("mcp__"):
            assert payload.get("mcp_server") == {"name": server_name, "source": "dynamic"}, payload
            assert list(payload)[-1] == "mcp_server", payload
        else:
            assert "mcp_server" not in payload, payload
        hook_rows.append({"event": payload["hook_event_name"], "keys": list(payload),
                          "tool_name": payload["tool_name"], "mcp_server": payload.get("mcp_server"),
                          "raw_sha256": hashlib.sha256(raw).hexdigest()})
    events = {row["event"] for row in hook_rows}
    assert "PreToolUse" in events, "native hook was not invoked"
    if scenario == "permission-denied-auto":
        assert "PermissionDenied" in events and "PostToolUse" not in events, events
        assert "PermissionRequest" not in events, "denial must come from the classifier"
        assert any("WIRE_CLASSIFIER_DENIAL_322" in path.read_text() for path in (root / "hooks").glob("PermissionDenied.*.raw.json"))
        wire_requests = [json.loads(line) for line in (root / "mcp-in.raw.jsonl").read_text().splitlines()]
        assert not any(request.get("method") == "tools/call" for request in wire_requests)
        assert any(not request["body"].get("tools") and "<block>" in json.dumps(request["body"]) for request in messages), "native classifier was not called"
    elif scenario in ("success", "permission-request", "deferred-description"):
        assert "PostToolUse" in events and "PostToolUseFailure" not in events
    else:
        assert "PostToolUseFailure" in events and "PostToolUse" not in events
    if scenario == "permission-request":
        assert "PermissionRequest" in events
    results = [block for request in messages for block in blocks(request["body"])
               if block.get("type") == "tool_result" and block.get("tool_use_id") == "toolu_mcp_hooks_322"]
    assert results, "MCP tool result missing from follow-up model request"
    if scenario in ("logical-error", "rpc-error", "connection-close", "permission-denied-auto"):
        assert results[-1].get("is_error") is True, results[-1]
    mcp_schemas = [t for request in messages for t in request["body"].get("tools", [])
                   if t.get("name", "").startswith("mcp__")]
    assert mcp_schemas, "MCP schema was not exposed"
    limit = 16384 if scenario == "deferred-description" else 4096
    expected_description = DESCRIPTION[:limit] + "\u2026 [truncated]"
    assert all(t["description"] == expected_description for t in mcp_schemas)
    if scenario == "deferred-description":
        assert len(messages) >= 3
        assert not any(t.get("name", "").startswith("mcp__") for t in messages[0]["body"].get("tools", []))
        assert all(t.get("defer_loading") is True for t in mcp_schemas)
    report = {"scenario": scenario, "server_name": server_name, "normalized_server_name": normalized_server_name(server_name), "tool_name": tool_name, "expected_wire_tool_name": expected_wire_tool_name if expected_wire_tool_name is not None else tool_name, "passed": True, "exit_code": process.returncode,
              "api_requests": len(messages), "hooks": hook_rows, "model_tool_result": results[-1],
              "description_utf16_units": sorted({len(t["description"].encode("utf-16-le", errors="surrogatepass")) // 2 for t in mcp_schemas}),
              "refused_paths": refused}
    dump(root / "report.json", report)
    return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scenario", choices=("all", *SCENARIOS, *EXTRA_SCENARIOS), default="all")
    parser.add_argument("--timeout", type=float, default=40)
    parser.add_argument("--server-name-json", default=json.dumps(SERVER_NAME), help="JSON string containing the authored server name, including escaped lone UTF16")
    parser.add_argument("--tool-name-json", default='"echo"', help="JSON string containing the original upstream tool name")
    parser.add_argument("--expected-wire-tool-name-json", help="Expected native sanitized tools/call name, when different from the advertised name")
    args = parser.parse_args()
    server_name = json.loads(args.server_name_json)
    assert isinstance(server_name, str), "--server-name-json must decode to one string"
    tool_name = json.loads(args.tool_name_json)
    assert isinstance(tool_name, str), "--tool-name-json must decode to one string"
    expected_wire_tool_name = json.loads(args.expected_wire_tool_name_json) if args.expected_wire_tool_name_json is not None else None
    assert expected_wire_tool_name is None or isinstance(expected_wire_tool_name, str)
    binary = args.binary.resolve(strict=True)
    digest = hashlib.sha256()
    with binary.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    assert digest.hexdigest() == SHA256, "native artifact changed"
    args.output.mkdir(parents=True, exist_ok=True)
    reports = []
    for scenario in SCENARIOS if args.scenario == "all" else [args.scenario]:
        report = run(binary, args.output / scenario, scenario, args.timeout, server_name, tool_name, expected_wire_tool_name)
        reports.append(report)
        print(f"PASS {scenario}: {report['api_requests']} model requests; hooks {[row['event'] for row in report['hooks']]}", flush=True)
    denial_scope = " Native client-classifier PermissionDenied is exercised." if any(report["scenario"] == "permission-denied-auto" for report in reports) else " PermissionDenied requires its separate optional scenario."
    dump(args.output / "report.json", {"version": VERSION, "native_sha256": SHA256,
        "scope": "Unmodified native CLI, isolated settings, fake credentials, loopback scripted API and stdio MCP server." + denial_scope + " Other transports require separate acceptance.",
        "scenarios": reports})


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--stdio-server":
        stdio_server(Path(sys.argv[2]), sys.argv[3], json.loads(sys.argv[4]) if len(sys.argv) > 4 else "echo", json.loads(sys.argv[5]) if len(sys.argv) > 5 else None)
    elif len(sys.argv) > 1 and sys.argv[1] == "--capture-hook":
        capture_hook(Path(sys.argv[2]))
    else:
        main()
