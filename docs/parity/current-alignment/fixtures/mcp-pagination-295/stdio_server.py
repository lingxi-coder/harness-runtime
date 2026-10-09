#!/usr/bin/env python3
"""Bounded MCP pagination oracle server for Native Claude Code 2.1.295."""

import json
import os
from pathlib import Path
import sys


TRACE = Path(os.environ["MCP_PAGINATION_TRACE_DIR"])
FIXTURE = json.loads(Path(os.environ["MCP_PAGINATION_CASES_PATH"]).read_text())
DISCOVERY_MODE = os.environ.get("MCP_PAGINATION_DISCOVERY_MODE", "fallback")
TRACE.mkdir(parents=True, exist_ok=True)
IN_PATH = TRACE / "mcp-stdio-in.raw.jsonl"
OUT_PATH = TRACE / "mcp-stdio-out.raw.jsonl"
COUNTS = {"tools/list": 0, "prompts/list": 0, "resources/list": 0}
CURSOR_COUNTS = {method: 0 for method in COUNTS}


def raw_json(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode() + b"\n"


def save(path: Path, data: bytes) -> None:
    with path.open("ab") as f:
        f.write(data)
        f.flush()


def send(value: dict, stem: str, index: int) -> None:
    data = raw_json(value)
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()
    save(OUT_PATH, data)
    (TRACE / f"{stem}-response-{index:02}.raw.jsonrpc").write_bytes(data[:-1])


def item_for(method: str, page: int) -> dict:
    if method == "tools/list":
        return {
            "name": f"{FIXTURE['tool_name_prefix']}{page:02}",
            "description": f"Fixed pagination marker tool from page {page}.",
            "inputSchema": {
                "type": "object",
                "properties": {"page": {"type": "integer"}},
                "required": ["page"],
                "additionalProperties": False,
            },
        }
    if method == "prompts/list":
        return {"name": f"prompt_page_{page:02}", "description": f"Prompt marker from page {page}.", "arguments": []}
    return {
        "uri": f"fixture://pagination/resource/{page:02}",
        "name": f"resource_page_{page:02}",
        "description": f"Resource marker from page {page}.",
        "mimeType": "text/plain",
    }


def result_for(method: str, item: dict, next_cursor: str | None) -> dict:
    key = {"tools/list": "tools", "prompts/list": "prompts", "resources/list": "resources"}[method]
    result = {key: [item]}
    if next_cursor is not None:
        result["nextCursor"] = next_cursor
    if DISCOVERY_MODE == "modern":
        result = {"resultType": "complete", "ttlMs": 0, "cacheScope": "private", **result}
    return result


def handle(request: dict, raw_request: bytes) -> None:
    method = request.get("method")
    request_id = request.get("id")
    params = request.get("params") if isinstance(request.get("params"), dict) else {}
    if request_id is not None and isinstance(method, str):
        index = COUNTS.get(method, 0) + 1
        save(IN_PATH, raw_request)
        stem = method.replace("/", "-")
        (TRACE / f"{stem}-request-{index:02}.raw.jsonrpc").write_bytes(raw_request.rstrip(b"\r\n"))

        if method == "initialize":
            send({"jsonrpc": "2.0", "id": request_id, "result": {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {
                    "tools": {"listChanged": False},
                    "prompts": {"listChanged": False},
                    "resources": {"listChanged": False, "subscribe": False},
                },
                "serverInfo": {"name": "pagination-fixture", "version": "1.0.0"},
            }}, "initialize", index)
            return

        if method in COUNTS:
            COUNTS[method] += 1
            cursor = params.get("cursor")
            if cursor is None:
                page = 0
            elif cursor == FIXTURE["repeated_cursor"]:
                CURSOR_COUNTS[method] += 1
                repeated = CURSOR_COUNTS[method]
                if repeated > FIXTURE["cursor_page_responses"]:
                    error = {"jsonrpc": "2.0", "id": request_id, "error": {
                        "code": -32091,
                        "message": "fixture stopped repeated cursor after bounded page responses",
                    }}
                    send(error, stem, index)
                    return
                page = repeated
            else:
                send({"jsonrpc": "2.0", "id": request_id, "error": {
                    "code": -32092, "message": "fixture received an unexpected cursor",
                }}, stem, index)
                return

            item = item_for(method, page)
            response_result = result_for(method, item, FIXTURE["repeated_cursor"])
            send({"jsonrpc": "2.0", "id": request_id, "result": response_result}, stem, index)
            return

        if method == "tools/call":
            tool = params.get("name", "")
            input_value = params.get("arguments", {})
            response = {"jsonrpc": "2.0", "id": request_id, "result": {
                "content": [{"type": "text", "text": f"CALLED:{tool}:{input_value.get('page')}"}],
                "isError": False,
                "structuredContent": {"tool": tool, "page": input_value.get("page")},
            }}
            if DISCOVERY_MODE == "modern":
                response["result"].update({"resultType": "complete", "ttlMs": 0, "cacheScope": "private"})
            send(response, "tools-call", index)
            return

        if method == "resources/templates/list":
            send({"jsonrpc": "2.0", "id": request_id, "result": {"resourceTemplates": []}}, "resources-templates-list", index)
            return
        if method == "ping" or method == "logging/setLevel":
            send({"jsonrpc": "2.0", "id": request_id, "result": {}}, method.replace("/", "-"), index)
            return
        if method == "server/discover":
            if DISCOVERY_MODE == "modern":
                result = {
                    "supportedVersions": ["2025-11-25", "2026-07-28"],
                    "capabilities": {
                        "tools": {"listChanged": False},
                        "prompts": {"listChanged": False},
                        "resources": {"listChanged": False, "subscribe": False},
                        "extensions": {"vendor/pagination-fixture": {"enabled": True}},
                    },
                    "instructions": "Fixed local pagination acceptance server.",
                    "_meta": {"io.modelcontextprotocol/serverInfo": {
                        "name": "pagination-fixture", "version": "1.0.0",
                    }},
                    "resultType": "complete",
                }
                send({"jsonrpc": "2.0", "id": request_id, "result": result}, "server-discover", index)
            else:
                send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "fixture method not found"}}, "server-discover", index)
            return
        send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "fixture method not found"}}, "unknown-method", index)
        return

    if isinstance(method, str) and method.startswith("notifications/"):
        save(IN_PATH, raw_request)


def main() -> None:
    for line in sys.stdin.buffer:
        raw = line
        try:
            request = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if isinstance(request, dict):
            handle(request, raw)


if __name__ == "__main__":
    main()
