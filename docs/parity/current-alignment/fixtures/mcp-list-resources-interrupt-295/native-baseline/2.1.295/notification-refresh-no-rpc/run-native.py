#!/usr/bin/env python3
"""Bounded Native 2.1.295 ListMcpResourcesTool SIGINT probe."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parent
FIXTURE_PATH = ROOT / "cases.json"
FIXTURE = json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))
SERVER_PATH = ROOT / "stdio_server.py"
OUTPUT = ROOT / "run-output"
DUMMY_KEY = "native-mcp-resources-interrupt-295-dummy-key"
EXPECTED_SHA = FIXTURE["native_sha256"]
TOOL_USE_ID = FIXTURE["tool_use_id"]


def sha(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sse(events: list[tuple[str, dict]]) -> bytes:
    return "".join(
        f"event: {name}\ndata: {json.dumps(data, ensure_ascii=True, separators=(',', ':'))}\n\n"
        for name, data in events
    ).encode("utf-8")


def sse_text(text: str, message_id: str) -> bytes:
    return sse([
        ("message_start", {"type": "message_start", "message": {
            "id": message_id, "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "text", "text": ""}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "text_delta", "text": text}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                            "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ])


def sse_tool(tool_name: str) -> bytes:
    args = {"server_name": FIXTURE["server_name"]}
    return sse([
        ("message_start", {"type": "message_start", "message": {
            "id": "msg_native_mcp_resources_interrupt_tool", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-5-20250929", "content": [],
            "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": 10, "output_tokens": 0},
        }}),
        ("content_block_start", {"type": "content_block_start", "index": 0,
                                  "content_block": {"type": "tool_use", "id": TOOL_USE_ID,
                                                    "name": tool_name, "input": {}}}),
        ("content_block_delta", {"type": "content_block_delta", "index": 0,
                                  "delta": {"type": "input_json_delta",
                                            "partial_json": json.dumps(args, separators=(",", ":"))}}),
        ("content_block_stop", {"type": "content_block_stop", "index": 0}),
        ("message_delta", {"type": "message_delta",
                            "delta": {"stop_reason": "tool_use", "stop_sequence": None},
                            "usage": {"output_tokens": 5}}),
        ("message_stop", {"type": "message_stop"}),
    ])


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--notification-refresh", action="store_true",
                        help="trigger a standard resources/list_changed after the ListMcpResourcesTool result")
    options = parser.parse_args()
    discovery_mode = "legacy-notification-refresh" if options.notification_refresh else "modern"

    binary_value = os.environ.get("CLAUDE_NATIVE_BINARY")
    if not binary_value:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the pinned Native 2.1.295 executable")
    binary = Path(binary_value).expanduser().resolve()
    binary_bytes = binary.read_bytes()
    if sha(binary_bytes) != EXPECTED_SHA:
        raise SystemExit(f"Native SHA mismatch: {sha(binary_bytes)}")
    version = subprocess.run([str(binary), "--version"], capture_output=True, timeout=15, check=False)
    version_text = (version.stdout + version.stderr).decode("utf-8", errors="backslashreplace").strip()
    if version.returncode != 0 or version_text != "2.1.295 (Claude Code)":
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    OUTPUT.mkdir(parents=True, exist_ok=True)
    run_root = Path(tempfile.mkdtemp(prefix="native-mcp-list-resources-interrupt-295-", dir=OUTPUT))
    for name in ("home", "config", "tmp", "mcp-trace"):
        (run_root / name).mkdir()
    external_cwd = Path(tempfile.mkdtemp(prefix="native-mcp-list-resources-interrupt-295-cwd-", dir="/private/tmp"))
    trace = run_root / "mcp-trace"
    config_path = run_root / "mcp-config.json"
    config_path.write_text(json.dumps({"mcpServers": {FIXTURE["server_name"]: {
        "command": sys.executable,
        "args": [str(SERVER_PATH)],
        "env": {
            "MCP_RESOURCES_INTERRUPT_TRACE_DIR": str(trace),
            "MCP_RESOURCES_INTERRUPT_CASES_PATH": str(FIXTURE_PATH),
            "MCP_RESOURCES_INTERRUPT_DISCOVERY_MODE": discovery_mode,
        },
    }}}, ensure_ascii=True, indent=2) + "\n", encoding="utf-8")

    request_rows: list[dict] = []
    response_rows: list[dict] = []
    tool_use_sent = trace / "model-tool-use-sent.signal"
    initial_resources_done = trace / "initial-resources-list-complete.signal"
    pending_resources_list = trace / "resources-list-pending.json"
    followup_request_marker = trace / "model-followup-request.signal"
    process_done = threading.Event()

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

        def do_POST(self) -> None:
            body = self.rfile.read(int(self.headers.get("content-length", "0")))
            accepted = (
                self.client_address[0] == "127.0.0.1"
                and self.path == "/v1/messages?beta=true"
                and self.headers.get("x-api-key") == DUMMY_KEY
            )
            row = {
                "method": "POST", "path": self.path, "accepted": accepted,
                "dummy_key_matched": self.headers.get("x-api-key") == DUMMY_KEY,
                "request_bytes": len(body), "request_sha256": sha(body),
            }
            request_rows.append(row)
            index = len(request_rows)
            (run_root / f"model-request-{index:02}.raw.json").write_bytes(body)
            if not accepted:
                self.reply(404 if self.path != "/v1/messages?beta=true" else 401, b"{}", "application/json")
                return
            try:
                request = json.loads(body)
            except (UnicodeDecodeError, json.JSONDecodeError):
                self.reply(400, b"{}", "application/json")
                return
            row["model"] = request.get("model")
            row["stream"] = request.get("stream")
            tools = request.get("tools", [])
            tool_names = [tool.get("name") for tool in tools if isinstance(tool, dict)]
            target = next((name for name in tool_names if name in ("ListMcpResourcesTool", "ListMcpResources")), None)
            row["resource_list_tools"] = [name for name in tool_names if isinstance(name, str) and "McpResources" in name]

            if index == 1:
                row["initial_resources_list_completed"] = initial_resources_done.exists()
                if target is None:
                    out = sse_text("LIST_MCP_RESOURCES_TOOL_NOT_VISIBLE", "msg_native_resources_tool_missing")
                    self.reply(200, out, "text/event-stream")
                    (run_root / "model-response-01.raw.sse").write_bytes(out)
                    response_rows.append({"index": 1, "kind": "tool_not_visible", "bytes": len(out), "sha256": sha(out)})
                    return
                tool_use_sent.write_text("sent\n", encoding="ascii")
                out = sse_tool(target)
                self.reply(200, out, "text/event-stream")
                (run_root / "model-response-01.raw.sse").write_bytes(out)
                response_rows.append({"index": 1, "kind": "list_resources_tool_use", "name": target,
                                      "bytes": len(out), "sha256": sha(out)})
                return

            row["fixture_tool_result_present"] = any(
                isinstance(message.get("content"), list)
                and any(isinstance(block, dict) and block.get("type") == "tool_result"
                        and block.get("tool_use_id") == TOOL_USE_ID for block in message["content"])
                for message in request.get("messages", [])
            )
            if options.notification_refresh:
                followup_request_marker.write_text("received\n", encoding="ascii")
                deadline = time.monotonic() + FIXTURE["pending_timeout_seconds"]
                while time.monotonic() < deadline and not pending_resources_list.exists():
                    time.sleep(0.02)
                row["notification_refresh_pending_before_followup_response"] = pending_resources_list.exists()
                if pending_resources_list.exists():
                    process_done.wait(timeout=FIXTURE["interrupt_settle_seconds"])
                    response_rows.append({"index": index, "kind": "held_for_sigint", "bytes": 0})
                    return
            out = sse_text(FIXTURE["final_text"], "msg_native_resources_interrupt_final")
            self.reply(200, out, "text/event-stream")
            (run_root / f"model-response-{index:02}.raw.sse").write_bytes(out)
            response_rows.append({"index": index, "kind": "final", "bytes": len(out), "sha256": sha(out)})

    server = HTTPServer(("127.0.0.1", 0), Handler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    env = {
        "PATH": "/usr/bin:/bin", "TMPDIR": str(run_root / "tmp"), "LANG": "C.UTF-8", "TZ": "UTC",
        "TERM": "dumb", "HOME": str(run_root / "home"), "CLAUDE_CONFIG_DIR": str(run_root / "config"),
        "ANTHROPIC_API_KEY": DUMMY_KEY, "ANTHROPIC_BASE_URL": f"http://127.0.0.1:{server.server_port}",
        "ANTHROPIC_MAX_RETRIES": "0", "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_TELEMETRY": "1", "DO_NOT_TRACK": "1",
    }
    args = [
        "--setting-sources", "project", "--strict-mcp-config", "--mcp-config", str(config_path),
        "--print", "--output-format", "json", "--tools", "default", "--max-turns", "2",
        "--model", FIXTURE["model"], "--session-id", FIXTURE["session_id"], "--debug", "mcp",
        "--dangerously-skip-permissions", "--allow-dangerously-skip-permissions", FIXTURE["prompt"],
    ]
    started_ns = time.monotonic_ns()
    started_wall_ns = time.time_ns()
    process = subprocess.Popen([str(binary), *args], cwd=external_cwd, env=env,
                               stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    interrupt_trace: dict = {"signal": "SIGINT", "requested": False, "discovery_mode": discovery_mode}

    def interrupt_after_pending_call() -> None:
        deadline = time.monotonic() + FIXTURE["pending_timeout_seconds"]
        pending_path = trace / "resources-list-pending.json"
        while time.monotonic() < deadline and process.poll() is None:
            if pending_path.exists():
                pending = json.loads(pending_path.read_text(encoding="utf-8"))
                interrupt_trace.update({
                    "requested": True,
                    "pending": pending,
                    "delay_from_pending_ms": round((time.monotonic_ns() - pending["pending_monotonic_ns"]) / 1_000_000, 3),
                })
                time.sleep(0.25)
                try:
                    process.send_signal(signal.SIGINT)
                    interrupt_trace["sent"] = True
                except ProcessLookupError:
                    interrupt_trace["sent"] = False
                    interrupt_trace["native_exited_before_signal"] = True
                interrupt_trace["signal_sent_monotonic_ns"] = time.monotonic_ns()
                interrupt_trace["signal_sent_epoch_ns"] = time.time_ns()
                return
            time.sleep(0.02)
        interrupt_trace["pending_observed"] = False

    interrupt_thread = threading.Thread(target=interrupt_after_pending_call, daemon=True)
    interrupt_thread.start()
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=FIXTURE["timeout_seconds"])
        exit_code = process.returncode
    except subprocess.TimeoutExpired:
        timed_out = True
        process.kill()
        stdout, stderr = process.communicate()
        exit_code = process.returncode
    interrupt_thread.join(timeout=1)
    process_done.set()
    interrupt_trace["native_exit_code"] = exit_code
    interrupt_trace["elapsed_since_launch_ms"] = round((time.monotonic_ns() - started_ns) / 1_000_000, 3)
    interrupt_trace["elapsed_since_launch_epoch_ms"] = round((time.time_ns() - started_wall_ns) / 1_000_000, 3)
    interrupt_trace["stdout_bytes"] = len(stdout)
    interrupt_trace["stderr_bytes"] = len(stderr)

    server.shutdown()
    server.server_close()
    server_thread.join(timeout=2)
    (run_root / "native.stdout.raw.json").write_bytes(stdout)
    (run_root / "native.stderr.raw").write_bytes(stderr)
    (run_root / "interrupt-trace.json").write_text(json.dumps(interrupt_trace, indent=2) + "\n", encoding="utf-8")
    try:
        parsed_stdout = json.loads(stdout)
    except (UnicodeDecodeError, json.JSONDecodeError):
        parsed_stdout = None

    session_paths = sorted((run_root / "config" / "projects").rglob(f"{FIXTURE['session_id']}.jsonl"))
    session_jsonl = b""
    if len(session_paths) == 1:
        session_jsonl = session_paths[0].read_bytes()
        (run_root / "native-session.raw.jsonl").write_bytes(session_jsonl)
    trace_in = trace / "mcp-stdio-in.raw.jsonl"
    trace_out = trace / "mcp-stdio-out.raw.jsonl"
    incoming_raw = trace_in.read_bytes() if trace_in.exists() else b""
    outgoing_raw = trace_out.read_bytes() if trace_out.exists() else b""
    debug_files = list((run_root / "home").rglob("mcp-logs-*/*.jsonl"))
    debug_raw = debug_files[0].read_bytes() if len(debug_files) == 1 else b""
    if debug_raw:
        (run_root / "native-mcp-debug.raw.jsonl").write_bytes(debug_raw)
    incoming = []
    for line in incoming_raw.splitlines():
        try:
            incoming.append(json.loads(line))
        except json.JSONDecodeError:
            pass
    methods = [message.get("method") for message in incoming]
    cancellation_messages = [message for message in incoming
                             if isinstance(message.get("method"), str)
                             and ("cancel" in message["method"].lower() or "interrupt" in message["method"].lower())]
    result_summary = ({key: parsed_stdout.get(key) for key in (
        "type", "subtype", "is_error", "result", "num_turns", "stop_reason", "session_id",
    )} if isinstance(parsed_stdout, dict) else None)
    receipt = {
        "native_version": version_text,
        "native_sha256": sha(binary_bytes),
        "runner_sha256": sha(Path(__file__).read_bytes()),
        "stdio_server_sha256": sha(SERVER_PATH.read_bytes()),
        "fixture_sha256": sha(FIXTURE_PATH.read_bytes()),
        "native_path": str(binary),
        "public_entrypoint": "Native 2.1.295 --print --output-format json, isolated strict --mcp-config, local stdio MCP, loopback Anthropic Messages stub",
        "discovery_mode": discovery_mode,
        "fixed_inputs": FIXTURE,
        "network_boundary": {"model_endpoint": "127.0.0.1 loopback only", "mcp_transport": "local stdio subprocess only",
                             "dummy_api_key_only": True, "isolated_home_config": True, "no_upstream_relay": True},
        "timeout_seconds": FIXTURE["timeout_seconds"],
        "timed_out": timed_out,
        "native_exit_code": exit_code,
        "native_result": result_summary,
        "interrupt": interrupt_trace,
        "mcp": {
            "incoming": {"path": "mcp-trace/mcp-stdio-in.raw.jsonl", "bytes": len(incoming_raw), "sha256": sha(incoming_raw)},
            "outgoing": {"path": "mcp-trace/mcp-stdio-out.raw.jsonl", "bytes": len(outgoing_raw), "sha256": sha(outgoing_raw)},
            "methods": methods,
            "resource_list_calls": sum(method == "resources/list" for method in methods),
            "cancellation_notifications": cancellation_messages,
        },
        "anthropic_messages_requests": request_rows,
        "anthropic_messages_responses": response_rows,
        "session_jsonl": {"path": "native-session.raw.jsonl", "bytes": len(session_jsonl), "sha256": sha(session_jsonl)} if session_jsonl else None,
        "native_mcp_debug": {"path": "native-mcp-debug.raw.jsonl", "bytes": len(debug_raw), "sha256": sha(debug_raw)} if debug_raw else None,
        "native_stdout": {"path": "native.stdout.raw.json", "bytes": len(stdout), "sha256": sha(stdout)},
        "native_stderr": {"path": "native.stderr.raw", "bytes": len(stderr), "sha256": sha(stderr)},
        "raw_outputs_relative_path": str(run_root.relative_to(ROOT)),
    }
    (run_root / "receipt.json").write_text(json.dumps(receipt, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    try:
        shutil.rmtree(external_cwd)
    except OSError:
        pass
    print(json.dumps({"native_version": version_text, "native_exit_code": exit_code, "timed_out": timed_out,
                      "native_result": result_summary, "interrupt": interrupt_trace, "mcp": receipt["mcp"],
                      "anthropic_messages_requests": request_rows, "receipt": str((run_root / "receipt.json").relative_to(ROOT))},
                     ensure_ascii=False, indent=2))
    return 1 if timed_out else 0


if __name__ == "__main__":
    raise SystemExit(main())
