#!/usr/bin/env python3
"""Return one controlled Native MCP error result with mixed text/image content."""

import json
import os
from pathlib import Path
import sys


TRACE = Path(os.environ["MCP_RESULT_TRACE_DIR"])
FIXTURE = json.loads(Path(os.environ["MCP_RESULT_CASES_PATH"]).read_text(encoding="utf-8"))
TRACE.mkdir(parents=True, exist_ok=True)
IN_PATH = TRACE / "mcp-stdio-in.raw.jsonl"
OUT_PATH = TRACE / "mcp-stdio-out.raw.jsonl"
CASES = {item["name"]: item for item in FIXTURE["tool_cases"]}
CALL_COUNT = 0


def append(path: Path, raw: bytes) -> None:
    with path.open("ab") as handle:
        handle.write(raw)
        handle.flush()


def send(value: dict, stem: str) -> None:
    raw = json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode("utf-8") + b"\n"
    sys.stdout.buffer.write(raw)
    sys.stdout.buffer.flush()
    append(OUT_PATH, raw)
    (TRACE / f"{stem}.raw.jsonrpc").write_bytes(raw[:-1])


def handle(request: dict, raw_request: bytes) -> None:
    global CALL_COUNT
    append(IN_PATH, raw_request)
    method = request.get("method")
    request_id = request.get("id")
    params = request.get("params") if isinstance(request.get("params"), dict) else {}
    stem = str(method).replace("/", "-") if isinstance(method, str) else "unknown"

    if method == "server/discover":
        send({"jsonrpc": "2.0", "id": request_id, "result": {
            "supportedVersions": ["2026-07-28"],
            "capabilities": {
                "tools": {"listChanged": False},
                "prompts": {"listChanged": False},
                "resources": {"listChanged": False, "subscribe": False},
            },
            "instructions": "Fixed MCP tool result presence fixture.",
            "_meta": {"io.modelcontextprotocol/serverInfo": {
                "name": "tool-result-presence-fixture", "version": "1.0.0",
            }},
            "resultType": "complete",
        }}, f"{stem}-response")
        return

    if method == "initialize":
        send({"jsonrpc": "2.0", "id": request_id, "result": {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {"tools": {"listChanged": False}, "prompts": {"listChanged": False},
                             "resources": {"listChanged": False, "subscribe": False}},
            "serverInfo": {"name": "tool-result-presence-fixture", "version": "1.0.0"},
        }}, f"{stem}-response")
        return

    if method == "tools/list":
        tools = [{
            "name": item["name"],
            "description": f"Return the fixed `{item['is_error']}` isError control result.",
            "inputSchema": {"type": "object", "properties": {"case": {"type": "string"}},
                            "required": ["case"], "additionalProperties": False},
        } for item in FIXTURE["tool_cases"]]
        send({"jsonrpc": "2.0", "id": request_id,
              "result": {"resultType": "complete", "ttlMs": 0, "cacheScope": "private", "tools": tools}},
             f"{stem}-response")
        return

    if method in ("prompts/list", "resources/list", "resources/templates/list"):
        key = {"prompts/list": "prompts", "resources/list": "resources",
               "resources/templates/list": "resourceTemplates"}[method]
        send({"jsonrpc": "2.0", "id": request_id,
              "result": {"resultType": "complete", "ttlMs": 0, "cacheScope": "private", key: []}},
             f"{stem}-response")
        return

    if method == "tools/call":
        name = params.get("name")
        case = CASES.get(name)
        CALL_COUNT += 1
        if case is None:
            send({"jsonrpc": "2.0", "id": request_id,
                  "error": {"code": -32602, "message": "unexpected tools/call name"}},
                 f"tools-call-{CALL_COUNT:02}-unexpected")
            return
        response_path = TRACE / f"tools-call-{CALL_COUNT:02}-{case['name']}-request.raw.jsonrpc"
        response_path.write_bytes(raw_request.rstrip(b"\r\n"))
        result = {
            "resultType": "complete",
            "ttlMs": 0,
            "cacheScope": "private",
            "content": case.get("content", [{"type": "text", "text": case["marker"]}]),
        }
        if case["is_error"] != "omitted":
            result["isError"] = case["is_error"]
        raw = json.dumps({"jsonrpc": "2.0", "id": request_id, "result": result},
                         ensure_ascii=True, separators=(",", ":")).encode("utf-8")
        response_path = TRACE / f"tools-call-{CALL_COUNT:02}-{case['name']}-response.raw.jsonrpc"
        response_path.write_bytes(raw)
        line = raw + b"\n"
        sys.stdout.buffer.write(line)
        sys.stdout.buffer.flush()
        append(OUT_PATH, line)
        return

    if method in ("ping", "logging/setLevel") and request_id is not None:
        send({"jsonrpc": "2.0", "id": request_id, "result": {}}, f"{stem}-response")
        return

    if isinstance(method, str) and method.startswith("notifications/"):
        return

    if request_id is not None:
        send({"jsonrpc": "2.0", "id": request_id,
              "error": {"code": -32601, "message": "fixture method not found"}}, f"{stem}-error")


for line in sys.stdin.buffer:
    try:
        message = json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError):
        append(IN_PATH, line)
        continue
    if isinstance(message, dict):
        handle(message, line)
