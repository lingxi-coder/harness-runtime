#!/usr/bin/env python3
"""Bounded Native 2.1.295 probe for modern MCP subscriptions/listen JSON-RPC errors."""
from http.server import BaseHTTPRequestHandler, HTTPServer
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time

ROOT=Path(__file__).resolve().parent
FIXTURE_PATH=ROOT/"cases.json"
FIXTURE=json.loads(FIXTURE_PATH.read_text(encoding="utf-8"))
SERVER_PATH=ROOT/"stdio_server.py"
OUTPUT=ROOT/"run-output"
DUMMY_KEY="native-mcp-listener-295-dummy-key"
TOOL_USE_ID=FIXTURE["tool_use_id"]
EXPECTED_SHA=FIXTURE["native_sha256"]


def sha(data:bytes)->str:
    return hashlib.sha256(data).hexdigest()


def sse(events:list[tuple[str,dict]])->bytes:
    return "".join(f"event: {name}\ndata: {json.dumps(data,ensure_ascii=True,separators=(',',':'))}\n\n" for name,data in events).encode()


def sse_text(text:str,message_id:str)->bytes:
    return sse([
      ("message_start",{"type":"message_start","message":{"id":message_id,"type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":None,"stop_sequence":None,"usage":{"input_tokens":10,"output_tokens":0}}}),
      ("content_block_start",{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
      ("content_block_delta",{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
      ("content_block_stop",{"type":"content_block_stop","index":0}),
      ("message_delta",{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":None},"usage":{"output_tokens":5}}),
      ("message_stop",{"type":"message_stop"}),
    ])


def sse_tool(name:str)->bytes:
    args={"marker":"listener-probe"}
    return sse([
      ("message_start",{"type":"message_start","message":{"id":"msg_native_mcp_listener_tool","type":"message","role":"assistant","model":"claude-sonnet-4-5-20250929","content":[],"stop_reason":None,"stop_sequence":None,"usage":{"input_tokens":10,"output_tokens":0}}}),
      ("content_block_start",{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":TOOL_USE_ID,"name":name,"input":{}}}),
      ("content_block_delta",{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":json.dumps(args,separators=(",",":"))}}),
      ("content_block_stop",{"type":"content_block_stop","index":0}),
      ("message_delta",{"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":None},"usage":{"output_tokens":5}}),
      ("message_stop",{"type":"message_stop"}),
    ])


def main()->int:
    binary_value=os.environ.get("CLAUDE_NATIVE_BINARY")
    if not binary_value:
        raise SystemExit("Set CLAUDE_NATIVE_BINARY to the pinned Native 2.1.295 executable")
    binary=Path(binary_value).expanduser().resolve()
    binary_bytes=binary.read_bytes()
    if sha(binary_bytes)!=EXPECTED_SHA:
        raise SystemExit(f"Native SHA mismatch: {sha(binary_bytes)}")
    version=subprocess.run([str(binary),"--version"],capture_output=True,timeout=15,check=False)
    version_text=(version.stdout+version.stderr).decode("utf-8",errors="backslashreplace").strip()
    if version.returncode!=0 or version_text!="2.1.295 (Claude Code)":
        raise SystemExit(f"Native version mismatch: {version_text!r}")

    OUTPUT.mkdir(parents=True,exist_ok=True)
    run_root=Path(tempfile.mkdtemp(prefix="native-mcp-listen-error-295-",dir=OUTPUT))
    for name in ("home","config","tmp","mcp-trace"):
        (run_root/name).mkdir()
    external_cwd=Path(tempfile.mkdtemp(prefix="native-mcp-listen-error-295-cwd-",dir="/private/tmp"))
    mcp_config={"mcpServers":{FIXTURE["server_name"]:{
        "command":sys.executable,
        "args":[str(SERVER_PATH)],
        "env":{"MCP_LISTENER_TRACE_DIR":str(run_root/"mcp-trace"),"MCP_LISTENER_CASES_PATH":str(FIXTURE_PATH)},
    }}}
    config_path=run_root/"mcp-config.json"
    config_path.write_text(json.dumps(mcp_config,ensure_ascii=True,indent=2)+"\n",encoding="utf-8")
    traces=[]
    responses=[]
    counts=[0]
    signal=run_root/"mcp-trace"/"subscriptions-listen-error-sent.signal"

    class Handler(BaseHTTPRequestHandler):
        protocol_version="HTTP/1.0"
        def log_message(self,_format:str,*_args:object)->None:
            return
        def reply(self,status:int,body:bytes,content_type:str)->None:
            self.send_response(status); self.send_header("Content-Length",str(len(body))); self.send_header("Content-Type",content_type); self.end_headers(); self.wfile.write(body)
        def do_POST(self)->None:
            body=self.rfile.read(int(self.headers.get("content-length","0")))
            accepted=self.client_address[0]=="127.0.0.1" and self.path=="/v1/messages?beta=true" and self.headers.get("x-api-key")==DUMMY_KEY
            row={"method":"POST","path":self.path,"accepted":accepted,"dummy_key_matched":self.headers.get("x-api-key")==DUMMY_KEY,"request_bytes":len(body),"request_sha256":sha(body)}
            traces.append(row)
            idx=len(traces)
            (run_root/f"model-request-{idx:02}.raw.json").write_bytes(body)
            if not accepted:
                self.reply(404 if self.path!="/v1/messages?beta=true" else 401,b"{}","application/json"); return
            try:
                req=json.loads(body)
            except (UnicodeDecodeError,json.JSONDecodeError):
                self.reply(400,b"{}","application/json"); return
            row["model"]=req.get("model"); row["stream"]=req.get("stream")
            tools=req.get("tools",[])
            names=[tool.get("name") for tool in tools if isinstance(tool,dict) and isinstance(tool.get("name"),str)]
            row["mcp_tool_names"]=[name for name in names if "listener_probe" in name]
            if idx==1:
                deadline=time.monotonic()+FIXTURE["listener_wait_seconds"]
                while time.monotonic()<deadline and not signal.exists():
                    time.sleep(0.02)
                row["subscriptions_listen_error_seen_before_model_response"]=signal.exists()
                tool_name=next((name for name in names if "listener_probe" in name),None)
                if tool_name:
                    out=sse_tool(tool_name)
                    self.reply(200,out,"text/event-stream")
                    (run_root/f"model-response-{idx:02}.raw.sse").write_bytes(out)
                    responses.append({"index":idx,"kind":"tool_use","name":tool_name,"bytes":len(out),"sha256":sha(out)})
                else:
                    out=sse_text("MCP_TOOL_NOT_ADVERTISED_AFTER_LISTENER_ERROR","msg_native_mcp_listener_no_tool")
                    self.reply(200,out,"text/event-stream")
                    (run_root/f"model-response-{idx:02}.raw.sse").write_bytes(out)
                    responses.append({"index":idx,"kind":"no_tool_final","bytes":len(out),"sha256":sha(out)})
                return
            result_block=None
            for message in req.get("messages",[]):
                content=message.get("content")
                if isinstance(content,list):
                    for block in content:
                        if isinstance(block,dict) and block.get("type")=="tool_result" and block.get("tool_use_id")==TOOL_USE_ID:
                            result_block=block
            row["fixture_tool_result_present"]=result_block is not None
            if result_block is not None:
                (run_root/"model-tool-result.raw.json").write_bytes(json.dumps(result_block,ensure_ascii=True,separators=(",",":")).encode())
            out=sse_text(FIXTURE["final_text"],"msg_native_mcp_listener_final")
            self.reply(200,out,"text/event-stream")
            (run_root/f"model-response-{idx:02}.raw.sse").write_bytes(out)
            responses.append({"index":idx,"kind":"final","bytes":len(out),"sha256":sha(out)})

    server=HTTPServer(("127.0.0.1",0),Handler)
    thread=threading.Thread(target=server.serve_forever,daemon=True); thread.start()
    env={"PATH":"/usr/bin:/bin","TMPDIR":str(run_root/"tmp"),"LANG":"C.UTF-8","TZ":"UTC","TERM":"dumb",
         "HOME":str(run_root/"home"),"CLAUDE_CONFIG_DIR":str(run_root/"config"),
         "ANTHROPIC_API_KEY":DUMMY_KEY,"ANTHROPIC_BASE_URL":f"http://127.0.0.1:{server.server_port}",
         "ANTHROPIC_MAX_RETRIES":"0","CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC":"1","DISABLE_TELEMETRY":"1","DO_NOT_TRACK":"1"}
    args=["--setting-sources","project","--strict-mcp-config","--mcp-config",str(config_path),"--print","--output-format","json","--tools","default","--max-turns","2","--model",FIXTURE["model"],"--session-id",FIXTURE["session_id"],"--debug","mcp","--dangerously-skip-permissions","--allow-dangerously-skip-permissions",FIXTURE["prompt"]]
    started=time.monotonic(); timed_out=False
    try:
        completed=subprocess.run([str(binary),*args],cwd=external_cwd,env=env,capture_output=True,timeout=FIXTURE["timeout_seconds"],check=False)
        stdout,stderr,exit_code=completed.stdout,completed.stderr,completed.returncode
    except subprocess.TimeoutExpired as error:
        stdout,stderr,exit_code,timed_out=error.stdout or b"",error.stderr or b"",None,True
    finally:
        server.shutdown(); server.server_close(); thread.join(timeout=2)
    (run_root/"native.stdout.raw.json").write_bytes(stdout)
    (run_root/"native.stderr.raw").write_bytes(stderr)
    try: parsed=json.loads(stdout)
    except (UnicodeDecodeError,json.JSONDecodeError): parsed=None
    trace=run_root/"mcp-trace"
    mcp_in=(trace/"mcp-stdio-in.raw.jsonl").read_bytes() if (trace/"mcp-stdio-in.raw.jsonl").exists() else b""
    mcp_out=(trace/"mcp-stdio-out.raw.jsonl").read_bytes() if (trace/"mcp-stdio-out.raw.jsonl").exists() else b""
    debug_files=list((run_root/"home").rglob("mcp-logs-*/*.jsonl"))
    debug=debug_files[0].read_bytes() if len(debug_files)==1 else b""
    if debug: (run_root/"native-mcp-debug.raw.jsonl").write_bytes(debug)
    incoming=[]; outgoing=[]
    for source,target in ((mcp_in,incoming),(mcp_out,outgoing)):
        for line in source.splitlines():
            try: target.append(json.loads(line))
            except json.JSONDecodeError: pass
    methods=[item.get("method") for item in incoming]
    listener_requests=[item for item in incoming if item.get("method")=="subscriptions/listen"]
    calls=[item for item in incoming if item.get("method")=="tools/call"]
    list_requests=[item for item in incoming if item.get("method")=="tools/list"]
    receipt={
      "native_version":version_text,"native_sha256":sha(binary_bytes),"runner_sha256":sha(Path(__file__).read_bytes()),"stdio_server_sha256":sha(SERVER_PATH.read_bytes()),"fixture_sha256":sha(FIXTURE_PATH.read_bytes()),"native_path":str(binary),
      "public_entrypoint":"Native 2.1.295 --print --output-format json, strict isolated --mcp-config, local stdio MCP modern discovery with tools.listChanged=true, loopback Messages stub",
      "fixed_inputs":FIXTURE,
      "network_boundary":{"model_endpoint":"127.0.0.1 loopback only","mcp_transport":"local stdio subprocess only","dummy_api_key_only":True,"isolated_home_config":True,"no_upstream_relay":True},
      "timeout_seconds":FIXTURE["timeout_seconds"],"elapsed_seconds":round(time.monotonic()-started,3),"native_exit_code":exit_code,"timed_out":timed_out,
      "native_result":({k:parsed.get(k) for k in ("type","subtype","is_error","result","num_turns","stop_reason","session_id")} if isinstance(parsed,dict) else None),
      "native_stdout":{"path":"native.stdout.raw.json","bytes":len(stdout),"sha256":sha(stdout)},"native_stderr":{"path":"native.stderr.raw","bytes":len(stderr),"sha256":sha(stderr)},
      "mcp":{"incoming":{"path":"mcp-trace/mcp-stdio-in.raw.jsonl","bytes":len(mcp_in),"sha256":sha(mcp_in)},"outgoing":{"path":"mcp-trace/mcp-stdio-out.raw.jsonl","bytes":len(mcp_out),"sha256":sha(mcp_out)},"methods":methods,"subscriptions_listen_requests":len(listener_requests),"subscriptions_listen_request_paths":["mcp-trace/subscriptions-listen-request.raw.jsonrpc"] if listener_requests else [],"listener_error_signal_seen":signal.exists(),"tools_list_request_count":len(list_requests),"tools_call_requests":len(calls),"tools_call_names":[((item.get("params") or {}).get("name")) for item in calls]},
      "anthropic_messages_requests":traces,"responses":responses,
      "mcp_debug_log":{"path":"native-mcp-debug.raw.jsonl","bytes":len(debug),"sha256":sha(debug)} if debug else None,
      "artifacts":sorted(str(p.relative_to(run_root)) for p in run_root.rglob("*") if p.is_file()),"raw_outputs_relative_path":str(run_root.relative_to(ROOT)),
    }
    (run_root/"receipt.json").write_text(json.dumps(receipt,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
    try: shutil.rmtree(external_cwd)
    except OSError: pass
    print(json.dumps({"native_version":version_text,"native_exit_code":exit_code,"timed_out":timed_out,"native_result":receipt["native_result"],"mcp":receipt["mcp"],"anthropic_messages_requests":traces,"receipt":str((run_root/"receipt.json").relative_to(ROOT))},ensure_ascii=False,indent=2))
    return 1 if timed_out else 0

if __name__=="__main__":
    raise SystemExit(main())
