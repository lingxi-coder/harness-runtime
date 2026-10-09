#!/usr/bin/env python3
"""MCP stdio server that hangs only on the model-triggered resources/list."""

import json
import os
from pathlib import Path
import sys
import threading
import time


TRACE = Path(os.environ["MCP_RESOURCES_INTERRUPT_TRACE_DIR"])
FIXTURE = json.loads(Path(os.environ["MCP_RESOURCES_INTERRUPT_CASES_PATH"]).read_text(encoding="utf-8"))
TRACE.mkdir(parents=True, exist_ok=True)
IN_PATH = TRACE / "mcp-stdio-in.raw.jsonl"
OUT_PATH = TRACE / "mcp-stdio-out.raw.jsonl"
RESOURCE_LIST_COUNT = 0
MODEL_TOOL_USE_MARKER = TRACE / "model-tool-use-sent.signal"
FOLLOWUP_REQUEST_MARKER = TRACE / "model-followup-request.signal"
NOTIFICATION_SENT = TRACE / "resources-list-changed-notification-sent.signal"
DISCOVERY_MODE = os.environ.get("MCP_RESOURCES_INTERRUPT_DISCOVERY_MODE", "modern")
OUT_LOCK = threading.Lock()


def append(path: Path, raw: bytes) -> None:
    with path.open("ab") as handle:
        handle.write(raw)
        handle.flush()


def emit(message: dict, stem: str) -> None:
    raw = json.dumps(message, ensure_ascii=True, separators=(",", ":")).encode("utf-8") + b"\n"
    with OUT_LOCK:
        sys.stdout.buffer.write(raw)
        sys.stdout.buffer.flush()
        append(OUT_PATH, raw)
        (TRACE / f"{stem}.raw.jsonrpc").write_bytes(raw[:-1])


def notify_resources_changed_after_followup() -> None:
    while not FOLLOWUP_REQUEST_MARKER.exists():
        time.sleep(0.01)
    notification = {
        "jsonrpc": "2.0",
        "method": "notifications/resources/list_changed",
        "params": {},
    }
    raw = json.dumps(notification, ensure_ascii=True, separators=(",", ":")).encode("utf-8") + b"\n"
    with OUT_LOCK:
        sys.stdout.buffer.write(raw)
        sys.stdout.buffer.flush()
        append(OUT_PATH, raw)
        (TRACE / "resources-list-changed-notification.raw.jsonrpc").write_bytes(raw[:-1])
    NOTIFICATION_SENT.write_text("sent\n", encoding="ascii")


def handle(request: dict, raw_request: bytes) -> None:
    global RESOURCE_LIST_COUNT
    append(IN_PATH, raw_request)
    method = request.get("method")
    request_id = request.get("id")
    params = request.get("params") if isinstance(request.get("params"), dict) else {}
    stem = str(method).replace("/", "-") if isinstance(method, str) else "unknown"

    if method == "server/discover":
        if DISCOVERY_MODE == "legacy-notification-refresh":
            emit({"jsonrpc": "2.0", "id": request_id, "error": {
                "code": -32601, "message": "fixture uses the standard notifications fallback",
            }}, f"{stem}-error")
            return
        emit({"jsonrpc": "2.0", "id": request_id, "result": {
            "supportedVersions": ["2026-07-28"],
            "capabilities": {
                "tools": {"listChanged": False},
                "prompts": {"listChanged": False},
                "resources": {"listChanged": False, "subscribe": False},
            },
            "instructions": "Fixed resources-list interrupt fixture.",
            "_meta": {"io.modelcontextprotocol/serverInfo": {
                "name": "resources-interrupt-fixture", "version": "1.0.0",
            }},
            "resultType": "complete",
        }}, f"{stem}-response")
        return

    if method == "initialize":
        resource_capability = {"listChanged": DISCOVERY_MODE == "legacy-notification-refresh", "subscribe": False}
        emit({"jsonrpc": "2.0", "id": request_id, "result": {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {
                "tools": {"listChanged": False},
                "prompts": {"listChanged": False},
                "resources": resource_capability,
            },
            "serverInfo": {"name": "resources-interrupt-fixture", "version": "1.0.0"},
        }}, f"{stem}-response")
        return

    if method == "resources/list":
        RESOURCE_LIST_COUNT += 1
        (TRACE / f"resources-list-request-{RESOURCE_LIST_COUNT:02}.raw.jsonrpc").write_bytes(raw_request.rstrip(b"\r\n"))
        if RESOURCE_LIST_COUNT == 1:
            emit({"jsonrpc": "2.0", "id": request_id, "result": {
                "resultType": "complete", "ttlMs": 0, "cacheScope": "private",
                "resources": [{
                    "uri": FIXTURE["resource_uri"],
                    "name": "Initial fixture resource",
                    "description": "Makes the Native ListMcpResourcesTool path model-visible.",
                    "mimeType": "text/plain",
                }],
            }}, f"resources-list-response-{RESOURCE_LIST_COUNT:02}")
            (TRACE / "initial-resources-list-complete.signal").write_text("complete\n", encoding="ascii")
            return

        if DISCOVERY_MODE == "legacy-notification-refresh" and NOTIFICATION_SENT.exists():
            pending = {
                "request_id": request_id,
                "method": method,
                "params": params,
                "pending_epoch_ns": time.time_ns(),
                "pending_monotonic_ns": time.monotonic_ns(),
                "raw_request_sha256": __import__("hashlib").sha256(raw_request.rstrip(b"\r\n")).hexdigest(),
                "trigger": "notifications/resources/list_changed after ListMcpResourcesTool result",
            }
            (TRACE / "resources-list-pending.json").write_text(json.dumps(pending, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
            while True:
                line = sys.stdin.buffer.readline()
                if not line:
                    (TRACE / "mcp-stdio-eof-after-pending.signal").write_text("eof\n", encoding="ascii")
                    return
                append(IN_PATH, line)
                append(TRACE / "post-pending-incoming.raw.jsonl", line)
                try:
                    message = json.loads(line)
                except (UnicodeDecodeError, json.JSONDecodeError):
                    continue
                if isinstance(message, dict):
                    name = message.get("method")
                    if isinstance(name, str) and ("cancel" in name.lower() or "interrupt" in name.lower()):
                        append(TRACE / "cancellation-notifications.raw.jsonl", line)
                    if name == "exit":
                        return

        if not MODEL_TOOL_USE_MARKER.exists():
            emit({"jsonrpc": "2.0", "id": request_id, "result": {
                "resultType": "complete", "ttlMs": 0, "cacheScope": "private", "resources": [],
            }}, f"resources-list-response-{RESOURCE_LIST_COUNT:02}")
            (TRACE / f"resources-list-pre-tool-response-{RESOURCE_LIST_COUNT:02}.signal").write_text(
                "completed before model tool-use\n", encoding="ascii",
            )
            return

        pending = {
            "request_id": request_id,
            "method": method,
            "params": params,
            "pending_epoch_ns": time.time_ns(),
            "pending_monotonic_ns": time.monotonic_ns(),
            "raw_request_sha256": __import__("hashlib").sha256(raw_request.rstrip(b"\r\n")).hexdigest(),
        }
        (TRACE / "resources-list-pending.json").write_text(json.dumps(pending, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        # Deliberately do not answer this RPC. Keep reading the same stdio stream
        # so an actual Native cancellation notification (if any) is captured.
        while True:
            line = sys.stdin.buffer.readline()
            if not line:
                (TRACE / "mcp-stdio-eof-after-pending.signal").write_text("eof\n", encoding="ascii")
                return
            append(IN_PATH, line)
            append(TRACE / "post-pending-incoming.raw.jsonl", line)
            try:
                message = json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if isinstance(message, dict):
                name = message.get("method")
                if isinstance(name, str) and ("cancel" in name.lower() or "interrupt" in name.lower()):
                    append(TRACE / "cancellation-notifications.raw.jsonl", line)
                if name == "exit":
                    return

    if method == "tools/list":
        emit({"jsonrpc": "2.0", "id": request_id, "result": {
            "resultType": "complete", "ttlMs": 0, "cacheScope": "private", "tools": [],
        }}, f"{stem}-response")
        return

    if method == "prompts/list":
        emit({"jsonrpc": "2.0", "id": request_id, "result": {
            "resultType": "complete", "ttlMs": 0, "cacheScope": "private", "prompts": [],
        }}, f"{stem}-response")
        return

    if method == "resources/templates/list":
        emit({"jsonrpc": "2.0", "id": request_id, "result": {
            "resultType": "complete", "ttlMs": 0, "cacheScope": "private", "resourceTemplates": [],
        }}, f"{stem}-response")
        return

    if method in ("ping", "logging/setLevel") and request_id is not None:
        emit({"jsonrpc": "2.0", "id": request_id, "result": {}}, f"{stem}-response")
        return

    if isinstance(method, str) and method.startswith("notifications/"):
        return

    if request_id is not None:
        emit({"jsonrpc": "2.0", "id": request_id, "error": {
            "code": -32601, "message": "fixture method not found",
        }}, f"{stem}-error")


if DISCOVERY_MODE == "legacy-notification-refresh":
    threading.Thread(target=notify_resources_changed_after_followup, daemon=True).start()

for incoming_line in sys.stdin.buffer:
    try:
        incoming_message = json.loads(incoming_line)
    except (UnicodeDecodeError, json.JSONDecodeError):
        append(IN_PATH, incoming_line)
        continue
    if isinstance(incoming_message, dict):
        handle(incoming_message, incoming_line)
