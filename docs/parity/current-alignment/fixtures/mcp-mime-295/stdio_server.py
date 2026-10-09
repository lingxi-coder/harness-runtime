#!/usr/bin/env python3
"""Bounded stdio MCP server returning multiple MIME-tagged resource blobs."""

import base64
import json
import os
from pathlib import Path
import sys


TRACE = Path(os.environ["MCP_MIME_TRACE_DIR"])
FIXTURE_PATH = Path(os.environ["MCP_MIME_CASES_PATH"])
FIXTURE = json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))
TRACE.mkdir(parents=True, exist_ok=True)
IN_PATH = TRACE / "mcp-stdio-in.raw.jsonl"
OUT_PATH = TRACE / "mcp-stdio-out.raw.jsonl"
COUNTS: dict[str, int] = {}


def raw_json(value: object) -> bytes:
    return json.dumps(value, ensure_ascii=True, separators=(",", ":")).encode() + b"\n"


def save(path: Path, data: bytes) -> None:
    with path.open("ab") as handle:
        handle.write(data)
        handle.flush()


def send(value: dict, method: str) -> None:
    index = COUNTS.get(method, 1)
    data = raw_json(value)
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()
    save(OUT_PATH, data)
    stem = method.replace("/", "-")
    (TRACE / f"{stem}-response-{index:02}.raw.jsonrpc").write_bytes(data[:-1])


def blob_result() -> dict:
    content = []
    for item in FIXTURE["resources"]:
        content.append({
            "type": "resource",
            "resource": {
                "uri": item["uri"],
                "mimeType": item["mime"],
                "blob": base64.b64encode(bytes.fromhex(item["bytes_hex"])).decode("ascii"),
            },
        })
    return {"content": content, "isError": False}


def handle(request: dict, raw_request: bytes) -> None:
    method = request.get("method")
    request_id = request.get("id")
    if not isinstance(method, str):
        return
    if request_id is None:
        if method.startswith("notifications/"):
            save(IN_PATH, raw_request)
        return

    index = COUNTS.get(method, 0) + 1
    COUNTS[method] = index
    save(IN_PATH, raw_request)
    stem = method.replace("/", "-")
    (TRACE / f"{stem}-request-{index:02}.raw.jsonrpc").write_bytes(raw_request.rstrip(b"\r\n"))

    if method == "server/discover":
        response = {"jsonrpc": "2.0", "id": request_id, "error": {
            "code": -32601, "message": "fixture does not implement server/discover",
        }}
        send(response, method)
        return
    if method == "initialize":
        params = request.get("params") if isinstance(request.get("params"), dict) else {}
        response = {"jsonrpc": "2.0", "id": request_id, "result": {
            "protocolVersion": params.get("protocolVersion", "2025-06-18"),
            "capabilities": {
                "tools": {"listChanged": False},
                "prompts": {"listChanged": False},
                "resources": {"listChanged": False, "subscribe": False},
            },
            "serverInfo": {"name": "mcp-mime-fixture", "version": "1.0.0"},
        }}
        send(response, method)
        return
    if method == "tools/list":
        response = {"jsonrpc": "2.0", "id": request_id, "result": {
            "tools": [{
                "name": FIXTURE["tool_name"],
                "description": "Return a fixed batch of MIME-tagged embedded resource blobs.",
                "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
            }],
        }}
        send(response, method)
        return
    if method in ("prompts/list", "resources/list", "resources/templates/list"):
        key = {
            "prompts/list": "prompts",
            "resources/list": "resources",
            "resources/templates/list": "resourceTemplates",
        }[method]
        send({"jsonrpc": "2.0", "id": request_id, "result": {key: []}}, method)
        return
    if method == "tools/call":
        params = request.get("params") if isinstance(request.get("params"), dict) else {}
        if params.get("name") != FIXTURE["tool_name"]:
            send({"jsonrpc": "2.0", "id": request_id, "error": {
                "code": -32602, "message": "unexpected fixture tool name",
            }}, method)
            return
        send({"jsonrpc": "2.0", "id": request_id, "result": blob_result()}, method)
        return
    if method in ("ping", "logging/setLevel"):
        send({"jsonrpc": "2.0", "id": request_id, "result": {}}, method)
        return
    send({"jsonrpc": "2.0", "id": request_id, "error": {
        "code": -32601, "message": "fixture method not found",
    }}, method)


def main() -> None:
    for raw in sys.stdin.buffer:
        try:
            request = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if isinstance(request, dict):
            handle(request, raw)


if __name__ == "__main__":
    main()
