#!/usr/bin/env python3
"""Probe SDK/direct MCP controls with isolated stdio peers, no model service."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import signal
import subprocess
import threading
import time

SHA = "a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450"
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, default=Path.home()/".local/share/claude/versions/2.1.322")
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--write-fixture", action="store_true")
parser.add_argument("--schema", action="store_true", help="Probe staged request schema boundaries")
parser.add_argument("--raw-errors", action="store_true", help="Probe source UTF-16 error diagnostics")
parser.add_argument("--sandbox-profile", type=Path, help="Run native CLI under a verified macOS sandbox profile")
options = parser.parse_args()
assert hashlib.sha256(options.binary.read_bytes()).hexdigest() == SHA
root = options.output.resolve()
root.mkdir(parents=True, exist_ok=False)
for folder in ["cwd", "config", "tmp"]:
    (root/folder).mkdir()
peer = root/"peer.py"
peer.write_text(r'''
import json,sys,threading,time
from pathlib import Path
root=Path(sys.argv[1]);lock=threading.Lock()
def send(value):
 with lock:
  raw=json.dumps(value,ensure_ascii=True,separators=(',',':'))+'\n'
  with (root/'peer.stdout.jsonl').open('a') as out:out.write(raw)
  sys.stdout.write(raw);sys.stdout.flush()
def respond(frame):
 method=frame.get('method');rid=frame.get('id');p=frame.get('params',{})
 if rid is None:return
 if method=='initialize':result={'protocolVersion':p['protocolVersion'],'capabilities':{'tools':{'listChanged':True}},'serverInfo':{'name':'ordinary','version':'1'}}
 elif method=='tools/list':result={'tools':[{'name':n,'inputSchema':{'type':'object'},'annotations':{'readOnlyHint':True}} for n in ['echo','slow','logical','rpc','files']]}
 elif method=='tools/call':
  name=p['name'];args=p.get('arguments',{})
  if name=='rpc':send({'jsonrpc':'2.0','id':rid,'error':{'code':-32003,'message':args.get('message','fixture rpc error')}});return
  if name=='slow':time.sleep(2)
  if name=='files':
   if isinstance(args,dict) and args.get('out'):Path(args['out']).write_text('STAGED_OUTPUT')
   text='files complete'
  else:text=json.dumps(args,ensure_ascii=True,separators=(',',':'))
  result={'content':[{'type':'text','text':text}],'structuredContent':{'kind':name},'_meta':{'fixture':True}}
  if name=='logical':
   result['isError']=True
   if isinstance(args,dict) and 'message' in args:result['content'][0]['text']=args['message']
 else:result={}
 send({'jsonrpc':'2.0','id':rid,'result':result})
for line in sys.stdin:
 with (root/'peer.stdin.jsonl').open('a') as out:out.write(line)
 try:frame=json.loads(line)
 except Exception:continue
 if frame.get('method')=='tools/call':threading.Thread(target=respond,args=(frame,),daemon=True).start()
 else:respond(frame)
''')
config = root/"mcp.json"
config.write_text(json.dumps({"mcpServers":{"ordinary":{"command":"/usr/bin/python3","args":[str(peer),str(root)]}}}))
env = {"PATH":os.defpath, "TERM":"dumb", "TMPDIR":str(root/"tmp"),
       "CLAUDE_CONFIG_DIR":str(root/"config"), "ANTHROPIC_API_KEY":"sk-ant-offline-sdk-gap-probe",
       "ANTHROPIC_BASE_URL":"http://127.0.0.1:9", "HTTP_PROXY":"http://127.0.0.1:9",
       "HTTPS_PROXY":"http://127.0.0.1:9", "ALL_PROXY":"http://127.0.0.1:9", "NO_PROXY":"127.0.0.1,localhost",
       "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC":"1", "DISABLE_AUTOUPDATER":"1", "DISABLE_TELEMETRY":"1",
       "DISABLE_ERROR_REPORTING":"1", "MCP_PROTOCOL_NEGOTIATION":"legacy", "ENABLE_TOOL_SEARCH":"false"}
if "HOME" in os.environ:
    env["HOME"] = os.environ["HOME"]
args = [str(options.binary),"--bare","-p","--input-format","stream-json","--output-format","stream-json","--verbose",
        "--permission-mode","bypassPermissions","--strict-mcp-config","--mcp-config",str(config)]
if options.sandbox_profile:
    args = ["/usr/bin/sandbox-exec", "-f", str(options.sandbox_profile.resolve()), *args]
process = subprocess.Popen(args,cwd=root/"cwd",env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
frames = queue.Queue()
report = {"nativeSha256":SHA,"scope":"SDK/control and ordinary stdio direct MCP calls; isolated config, fake credentials, no model turn or external service", "requests":[],"responses":{},"frames":[]}
if options.sandbox_profile:
    report["sandboxProfile"] = str(options.sandbox_profile.resolve())
    report["sandboxProfileSha256"] = hashlib.sha256(options.sandbox_profile.read_bytes()).hexdigest()
def read_stdout():
    with (root/"native.stdout.jsonl").open("ab") as capture:
        for raw in process.stdout:
            capture.write(raw);capture.flush()
            try:frames.put(json.loads(raw))
            except Exception:frames.put({"raw":raw.decode("utf8","replace")})
    frames.put({"eof":process.poll()})
def read_stderr():
    with (root/"native.stderr").open("ab") as capture:
        for raw in process.stderr:capture.write(raw);capture.flush()
threading.Thread(target=read_stdout,daemon=True).start()
threading.Thread(target=read_stderr,daemon=True).start()
def send(frame):
    raw=(json.dumps(frame,ensure_ascii=True,separators=(",",":"))+"\n").encode()
    with (root/"host.stdin.jsonl").open("ab") as out:out.write(raw)
    process.stdin.write(raw);process.stdin.flush()
def request(name,body):
    report["requests"].append({"name":name,"request":body})
    send({"type":"control_request","request_id":name,"request":body})
def next_frame(timeout=15):
    frame=frames.get(timeout=timeout)
    report["frames"].append(frame)
    if "eof" in frame:raise RuntimeError("native EOF: "+str(frame))
    request=frame.get("request",{})
    if request.get("subtype")=="mcp_message":
        message=request["message"];method=message.get("method")
        if method=="initialize":
            result={"protocolVersion":message["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}
        elif method=="tools/list":result={"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}
        else:result={}
        payload={"mcp_response":{"jsonrpc":"2.0","id":message.get("id",0),"result":result}}
        send({"type":"control_response","response":{"subtype":"success","request_id":frame["request_id"],"response":payload}})
    if frame.get("type")=="control_response":
        name=frame.get("response",{}).get("request_id")
        report["responses"][name]=frame["response"]
    return frame
def wait(name,timeout=20):
    deadline=time.monotonic()+timeout
    while name not in report["responses"]:
        next_frame(max(.1,deadline-time.monotonic()))
    return report["responses"][name]
try:
    request("initialize",{"subtype":"initialize","sdkMcpServers":["fixture"]})
    wait("initialize")
    for attempt in range(50):
        name="status-"+str(attempt);request(name,{"subtype":"mcp_status"});status=wait(name)
        rows=status.get("response",{}).get("mcpServers",[])
        if all(any(row.get("name")==server and row.get("status")=="connected" for row in rows) for server in ["ordinary","fixture"]):break
        time.sleep(.05)
    report["connected"]=rows
    cases=[
      ("tool-missing",{}),("tool-null",{"tool":None}),("tool-number",{"tool":3}),
      ("tool-unqualified",{"tool":"echo"}),("tool-server-only",{"tool":"mcp__ordinary"}),
      ("tool-empty-tool",{"tool":"mcp__ordinary__"}),
      ("check-null",{"tool":"mcp__fixture__echo","check_permissions":None}),
      ("check-string",{"tool":"mcp__fixture__echo","check_permissions":"true"}),
      ("sdk",{"tool":"mcp__fixture__echo"}),("sdk-checked",{"tool":"mcp__fixture__echo","check_permissions":True}),
      ("unknown",{"tool":"mcp__missing__echo"}),
      ("echo",{"tool":"mcp__ordinary__echo","arguments":{"value":"source"}}),
      ("echo-null",{"tool":"mcp__ordinary__echo","arguments":None}),
      ("echo-checked",{"tool":"mcp__ordinary__echo","arguments":{"value":"checked"},"check_permissions":True}),
      ("echo-program",{"tool":"mcp__ordinary__echo","arguments":{"value":"program"},"result_not_for_model":True}),
      ("logical",{"tool":"mcp__ordinary__logical"}),("rpc",{"tool":"mcp__ordinary__rpc"}),
      ("staged-timeout-only",{"tool":"mcp__ordinary__echo","timeout_ms":5000}),
      ("staged-bad-expiry",{"tool":"mcp__ordinary__echo","expires_at":"not-a-date"}),
      ("staged-expired",{"tool":"mcp__ordinary__echo","expires_at":"2000-01-01T00:00:00Z"}),
      ("staged-outputs-without-expiry",{"tool":"mcp__ordinary__files","output_files":[]}),
      ("staged-invalid-input-path",{"tool":"mcp__ordinary__files","input_files":[{"name":"a","lane_path":"relative"}]}),
      ("staged-input-missing",{"tool":"mcp__ordinary__files","input_files":[{"name":"a","lane_path":"/working/sdk-probe-missing"}]}),
      ("staged-output-unused",{"tool":"mcp__ordinary__files","expires_at":"2099-01-01T00:00:00Z","output_files":[{"name":"a","lane_path":"/working/output.txt"}]}),
      ("staged-token-unknown",{"tool":"mcp__ordinary__files","input_files":[],"arguments":{"value":"{{in:missing}}"}}),
      ("staged-logical",{"tool":"mcp__ordinary__logical","timeout_ms":5000}),
      ("staged-timeout",{"tool":"mcp__ordinary__slow","timeout_ms":1000}),
    ]
    if options.schema:
        base={"tool":"mcp__ordinary__echo","timeout_ms":5000}
        cases=[(name,{**base,**body}) for name,body in [
          ("arguments-number",{"arguments":7}),("arguments-array",{"arguments":[]}),("arguments-null",{"arguments":None}),
          ("timeout-string",{"timeout_ms":"5000"}),("timeout-null",{"timeout_ms":None}),
          ("expires-number",{"expires_at":7}),("program-string",{"result_not_for_model":"true"}),
          ("input-null",{"input_files":None}),("input-object",{"input_files":{}}),("input-entry-null",{"input_files":[None]}),
          ("input-missing-members",{"input_files":[{}]}),("input-invalid-name",{"input_files":[{"name":"a b","lane_path":"/working/a"}]}),
          ("input-lane-number",{"input_files":[{"name":"a","lane_path":7}]}),
          ("output-if-match-empty",{"output_files":[{"name":"a","lane_path":"/working/a","if_match":""}]}),
          ("output-if-match-null",{"output_files":[{"name":"a","lane_path":"/working/a","if_match":None}]}),
          ("input-too-many",{"input_files":[{"name":"a","lane_path":"/working/a"}]*65}),
          ("schema-many-issues",{"arguments":7,"expires_at":7,"timeout_ms":"x","result_not_for_model":7,"input_files":[{}],"output_files":[{}]}),
        ]]
    if options.raw_errors:
        cases=[
          ("raw-rpc",{"tool":"mcp__ordinary__rpc","arguments":{"message":"peer\ud800error\udfff"}}),
          ("raw-rpc-checked",{"tool":"mcp__ordinary__rpc","check_permissions":True,"arguments":{"message":"peer\ud800error\udfff"}}),
          ("raw-rpc-staged",{"tool":"mcp__ordinary__rpc","timeout_ms":5000,"arguments":{"message":"peer\ud800error\udfff"}}),
          ("raw-logical",{"tool":"mcp__ordinary__logical","arguments":{"message":"logical\ud800error\udfff"}}),
          ("raw-rpc-truncate-pair",{"tool":"mcp__ordinary__rpc","arguments":{"message":"a"*1999+"💡tail"}}),
          ("raw-rpc-trailing-low",{"tool":"mcp__ordinary__rpc","arguments":{"message":"a"*1999+"\udfff-tail"}}),
          ("raw-not-connected",{"tool":"mcp__\ud800missing\u200b__read"}),
          ("raw-unqualified",{"tool":"\ud800unqualified\u200b"}),
        ]
    for name,body in cases:
        request(name,{"subtype":"mcp_call",**body});wait(name)
    request("cancelled",{"subtype":"mcp_call","tool":"mcp__ordinary__slow"})
    time.sleep(.2)
    send({"type":"control_cancel_request","request_id":"cancelled"})
    request("after-cancel",{"subtype":"mcp_call","tool":"mcp__ordinary__echo"});wait("after-cancel")
    request("replaced",{"subtype":"mcp_call","tool":"mcp__ordinary__slow"})
    time.sleep(.2)
    request("replaced",{"subtype":"mcp_call","tool":"mcp__ordinary__echo"});wait("replaced")
    end=time.monotonic()+2.5
    while time.monotonic()<end:
        try:next_frame(.15)
        except queue.Empty:pass
    assert report["responses"]["cancelled"]["error"]=="mcp_call cancelled by client: ordinary"
    assert sum(frame.get('response',{}).get('request_id')=='replaced' for frame in report['frames'])==1,"superseded call must not reply"
    contracts={"version":"2.1.322","nativeSha256":SHA,"cases":[{"name":name,"request":{"subtype":"mcp_call",**body},"response":{k:v for k,v in report["responses"][name].items() if k!="request_id"}} for name,body in cases],"cancelError":"mcp_call cancelled by client: ordinary","supersededResponseSuppressed":True}
    fixture=Path(__file__).resolve().parents[2]/("crates/runtime/tests/fixtures/mcp_sdk_raw_errors_2_1_322.json" if options.raw_errors else "crates/runtime/tests/fixtures/mcp_sdk_staging_schema_2_1_322.json" if options.schema else "crates/runtime/tests/fixtures/mcp_sdk_gap_control_2_1_322.json")
    if options.write_fixture:fixture.write_text(json.dumps(contracts,ensure_ascii=True,indent=2)+"\n")
    else:assert json.loads(fixture.read_text())==contracts,"native SDK gap fixture differs"
    report["passed"]=True
finally:
    (root/"report.json").write_text(json.dumps(report,ensure_ascii=True,indent=2)+"\n")
    if process.poll() is None:os.killpg(process.pid,signal.SIGTERM)
    try:process.wait(timeout=5)
    except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait()
print(json.dumps({"passed":report.get("passed",False),"requests":len(report.get("requests",[])),"directCallCases":len(cases),"receipt":str(root/"report.json")},indent=2))
