#!/usr/bin/env python3
"""Modern MCP stdio server for the Native 2.1.295 subscription-listener probe."""
import json
import os
from pathlib import Path
import sys

TRACE=Path(os.environ["MCP_LISTENER_TRACE_DIR"])
FIXTURE=json.loads(Path(os.environ["MCP_LISTENER_CASES_PATH"]).read_text(encoding="utf-8"))
TRACE.mkdir(parents=True,exist_ok=True)
IN=TRACE/"mcp-stdio-in.raw.jsonl"
OUT=TRACE/"mcp-stdio-out.raw.jsonl"


def save(path:Path,data:bytes)->None:
    with path.open("ab") as f:
        f.write(data); f.flush()


def emit(value:dict,stem:str)->None:
    raw=json.dumps(value,ensure_ascii=True,separators=(",",":")).encode()+b"\n"
    sys.stdout.buffer.write(raw); sys.stdout.buffer.flush()
    save(OUT,raw)
    (TRACE/f"{stem}.raw.jsonrpc").write_bytes(raw[:-1])


def handle(req:dict,raw:bytes)->None:
    save(IN,raw)
    method=req.get("method")
    rid=req.get("id")
    params=req.get("params") if isinstance(req.get("params"),dict) else {}
    stem=(str(method).replace("/","-") if isinstance(method,str) else "unknown")
    if method=="server/discover":
        result={
          "supportedVersions":["2026-07-28"],
          "capabilities":{
            "tools":{"listChanged":True},
            "prompts":{"listChanged":False},
            "resources":{"listChanged":False,"subscribe":False},
          },
          "instructions":"Fixed local modern MCP listener fixture.",
          "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"listener-fixture","version":"1.0.0"}},
          "resultType":"complete",
        }
        emit({"jsonrpc":"2.0","id":rid,"result":result},f"{stem}-response")
        return
    if method=="initialize":
        emit({"jsonrpc":"2.0","id":rid,"result":{
          "protocolVersion":params.get("protocolVersion","2025-06-18"),
          "capabilities":{"tools":{"listChanged":True},"prompts":{"listChanged":False},"resources":{"listChanged":False,"subscribe":False}},
          "serverInfo":{"name":"listener-fixture","version":"1.0.0"},
        }},f"{stem}-response")
        return
    if method=="tools/list":
        emit({"jsonrpc":"2.0","id":rid,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":[{
          "name":FIXTURE["tool_name"],
          "description":"Returns a fixed marker after the modern catalog listener reports ECONNRESET.",
          "inputSchema":{"type":"object","properties":{"marker":{"type":"string"}},"required":["marker"],"additionalProperties":False},
        }]}},f"{stem}-response")
        return
    if method=="subscriptions/listen":
        (TRACE/"subscriptions-listen-request.raw.jsonrpc").write_bytes(raw.rstrip(b"\r\n"))
        error=FIXTURE["listener_error"]
        emit({"jsonrpc":"2.0","id":rid,"error":{"code":error["code"],"message":error["message"]}},"subscriptions-listen-error-response")
        (TRACE/"subscriptions-listen-error-sent.signal").write_text("sent\n",encoding="ascii")
        return
    if method=="tools/call":
        result={"resultType":"complete","ttlMs":0,"cacheScope":"private",
                "content":[{"type":"text","text":FIXTURE["tool_result_text"]}],
                "isError":False,"structuredContent":{"marker":params.get("arguments",{}).get("marker")}}
        emit({"jsonrpc":"2.0","id":rid,"result":result},f"{stem}-response")
        return
    if method in ("prompts/list","resources/list","resources/templates/list"):
        key={"prompts/list":"prompts","resources/list":"resources","resources/templates/list":"resourceTemplates"}[method]
        emit({"jsonrpc":"2.0","id":rid,"result":{"resultType":"complete","ttlMs":0,"cacheScope":"private",key:[]}},f"{stem}-response")
        return
    if method in ("ping","logging/setLevel"):
        emit({"jsonrpc":"2.0","id":rid,"result":{}},f"{stem}-response")
        return
    if isinstance(method,str) and method.startswith("notifications/"):
        return
    if rid is not None:
        emit({"jsonrpc":"2.0","id":rid,"error":{"code":-32601,"message":"fixture method not found"}},f"{stem}-error")


for line in sys.stdin.buffer:
    try:
        req=json.loads(line)
    except (UnicodeDecodeError,json.JSONDecodeError):
        continue
    if isinstance(req,dict):
        handle(req,line)
