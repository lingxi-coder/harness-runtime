#!/usr/bin/env python3
"""Fixed stdio MCP server used only by the Native 2.1.293 acceptance fixture."""

import json
import os
from pathlib import Path
import sys


TRACE_DIR = Path(os.environ["MCP_FIXTURE_TRACE_DIR"])
TRACE_DIR.mkdir(parents=True, exist_ok=True)
FIXTURE = json.loads(Path(os.environ["MCP_FIXTURE_CASES_PATH"]).read_text(encoding="utf-8"))
INPUT_PATH = TRACE_DIR / "stdio-in.raw.jsonl"
OUTPUT_PATH = TRACE_DIR / "stdio-out.raw.jsonl"
CALL_REQUEST_PATH = TRACE_DIR / "tools-call-request.raw.jsonrpc"
CALL_RESPONSE_PATH = TRACE_DIR / "tools-call-response.raw.jsonrpc"
TOOLS_LIST_RESPONSE_PATH = TRACE_DIR / "tools-list-response.raw.jsonrpc"
TOOL_NAME = FIXTURE["mcp_tool_name"]
PRIMARY_TEXT = FIXTURE["mcp_primary_text_prefix"] + "".join(
    chr(int(unit)) for unit in FIXTURE["mcp_primary_text_units_suffix"]
)
SECONDARY_TEXT = FIXTURE["mcp_secondary_text"]
STRUCTURED_CONTENT = FIXTURE["structured_content"]


def save(path: Path, raw: bytes) -> None:
    with path.open("ab") as handle:
        handle.write(raw)
        handle.flush()


def send(message: dict, response_path: Path | None = None) -> None:
    raw = json.dumps(message, ensure_ascii=True, separators=(",", ":")).encode("utf-8") + b"\n"
    sys.stdout.buffer.write(raw)
    sys.stdout.buffer.flush()
    save(OUTPUT_PATH, raw)
    if response_path is not None:
        response_path.write_bytes(raw)


def response(request_id: object, result: dict, response_path: Path | None = None) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "result": result}, response_path)


def handle(request: dict, raw_request: bytes) -> None:
    method = request.get("method")
    request_id = request.get("id")
    params = request.get("params") if isinstance(request.get("params"), dict) else {}

    if method == "initialize":
        requested = params.get("protocolVersion", "2025-06-18")
        response(request_id, {
            "protocolVersion": requested,
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "current-alignment-fixture", "version": "1.0.0"},
        })
        return
    # Native 2.1.293 probes this optional pre-initialize extension before
    # standard MCP initialize. The fixture intentionally returns JSON-RPC
    # method-not-found so the captured trace records Native's legacy fallback.
    if method == "notifications/initialized" or (isinstance(method, str) and method.startswith("notifications/")):
        return
    if method == "ping":
        response(request_id, {})
        return
    if method == "tools/list":
        response(request_id, {"tools": [{
            "name": TOOL_NAME,
            "description": "Return a fixed multiple-content result and structuredContent with a lone UTF-16 unit.",
            "inputSchema": {
                "type": "object",
                "properties": {"mode": {"type": "string", "enum": ["source-units"]}},
                "required": ["mode"],
                "additionalProperties": False,
            },
        }]}, TOOLS_LIST_RESPONSE_PATH)
        return
    if method == "resources/list":
        response(request_id, {"resources": []})
        return
    if method == "resources/templates/list":
        response(request_id, {"resourceTemplates": []})
        return
    if method == "prompts/list":
        response(request_id, {"prompts": []})
        return
    if method == "logging/setLevel":
        response(request_id, {})
        return
    if method == "tools/call":
        CALL_REQUEST_PATH.write_bytes(raw_request)
        result = {
            "content": [
                {"type": "text", "text": PRIMARY_TEXT},
                {"type": "text", "text": SECONDARY_TEXT},
            ],
            "structuredContent": STRUCTURED_CONTENT,
        }
        response(request_id, result, CALL_RESPONSE_PATH)
        return

    if request_id is None:
        return
    send({
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": -32601, "message": "fixture method not found"},
    })


def main() -> None:
    for raw in sys.stdin.buffer:
        save(INPUT_PATH, raw)
        try:
            request = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if isinstance(request, dict):
            handle(request, raw)


if __name__ == "__main__":
    main()
