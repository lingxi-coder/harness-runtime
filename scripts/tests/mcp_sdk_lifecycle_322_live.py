#!/usr/bin/env python3
"""Pin SDK initialize ordering, reconnect and dynamic control behavior."""
import argparse, hashlib, json, os, queue, signal, subprocess, threading, time
from pathlib import Path

p=argparse.ArgumentParser(description=__doc__)
p.add_argument("--binary",type=Path,default=Path.home()/".local/share/claude/versions/2.1.322")
p.add_argument("--output",type=Path,required=True)
p.add_argument("--write-fixture",action="store_true")
p.add_argument("--manifest",action="store_true")
p.add_argument("--hold-new",action="store_true",help="Leave the added server initializing while removing it")
o=p.parse_args()
sha="a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450"
assert hashlib.sha256(o.binary.read_bytes()).hexdigest()==sha
root=o.output.resolve();root.mkdir(parents=True,exist_ok=False)
for name in ["cwd","config","tmp"]:(root/name).mkdir()
env={"PATH":os.defpath,"TERM":"dumb","TMPDIR":str(root/"tmp"),"CLAUDE_CONFIG_DIR":str(root/"config"),
     "ANTHROPIC_API_KEY":"sk-ant-offline-sdk-lifecycle","ANTHROPIC_BASE_URL":"http://127.0.0.1:9",
     "HTTP_PROXY":"http://127.0.0.1:9","HTTPS_PROXY":"http://127.0.0.1:9","ALL_PROXY":"http://127.0.0.1:9",
     "NO_PROXY":"127.0.0.1,localhost","CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC":"1",
     "DISABLE_AUTOUPDATER":"1","DISABLE_TELEMETRY":"1","DISABLE_ERROR_REPORTING":"1","MCP_PROTOCOL_NEGOTIATION":"auto"}
process=subprocess.Popen([str(o.binary),"--bare","-p","--input-format","stream-json","--output-format","stream-json","--verbose","--permission-mode","bypassPermissions"],cwd=root/"cwd",env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
frames=queue.Queue();report={"nativeSha256":sha,"manifest":o.manifest,"frames":[],"requests":[],"responses":{},"sdkMessages":[]}
def stdout():
 with (root/"native.stdout.jsonl").open("ab") as out:
  for raw in process.stdout:
   out.write(raw);out.flush()
   try:frames.put(json.loads(raw))
   except Exception:frames.put({"raw":raw.decode("utf8","replace")})
 frames.put({"eof":process.poll()})
def stderr():
 with (root/"native.stderr").open("ab") as out:
  for raw in process.stderr:out.write(raw);out.flush()
threading.Thread(target=stdout,daemon=True).start();threading.Thread(target=stderr,daemon=True).start()
def send(frame):
 raw=(json.dumps(frame,ensure_ascii=True,separators=(",",":"))+"\n").encode()
 with (root/"host.stdin.jsonl").open("ab") as out:out.write(raw)
 process.stdin.write(raw);process.stdin.flush()
def request(rid,body):
 report["requests"].append({"id":rid,"request":body})
 send({"type":"control_request","request_id":rid,"request":body})
def next_frame(timeout=15,drive=True):
 f=frames.get(timeout=timeout);report["frames"].append(f)
 if "eof" in f:raise RuntimeError("native EOF "+str(f))
 if f.get("type")=="control_response":report["responses"][f["response"]["request_id"]]=f["response"]
 if f.get("request",{}).get("subtype")=="mcp_message":
  m=f["request"]["message"];report["sdkMessages"].append({"server":f["request"]["server_name"],"message":m})
  if drive:
   method=m.get("method")
   if o.hold_new and not report.get("heldNew") and f["request"]["server_name"]=="third" and method=="initialize":report["heldNew"]=True;return f
   if method=="initialize":r={"protocolVersion":m["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":f["request"]["server_name"],"version":"1"}}
   elif method=="tools/list":r={"tools":[{"name":"hold","inputSchema":{"type":"object"}}]}
   else:r={}
   send({"type":"control_response","response":{"subtype":"success","request_id":f["request_id"],"response":{"mcp_response":{"jsonrpc":"2.0","id":m.get("id",0),"result":r}}}})
 return f
def wait(rid):
 end=time.monotonic()+30
 while rid not in report["responses"]:next_frame(max(.1,end-time.monotonic()))
 return report["responses"][rid]
try:
 init={"subtype":"initialize","sdkMcpServers":["first","second"]}
 if o.manifest:init["sdkMcpServerManifests"]={name:{"initializeResult":{"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":name,"version":"1"}},"toolsListResult":{"tools":[{"name":"hold","inputSchema":{"type":"object"}}]}} for name in ["first","second"]}
 request("initialize",init)
 parked=[]
 while "initialize" not in report["responses"]:
  f=next_frame(drive=False)
  if f.get("request",{}).get("subtype")=="mcp_message":parked.append(f)
 report["handshakesBeforeInitializeReply"]=[f["request"]["server_name"] for f in parked]
 if o.manifest:assert not parked,report
 else:assert sorted(report["handshakesBeforeInitializeReply"])==["first","second"],report
 for f in parked:
  m=f["request"]["message"]
  assert m["method"]=="initialize" and m["id"]==0 and m["params"]["capabilities"]=={},report
  send({"type":"control_response","response":{"subtype":"success","request_id":f["request_id"],"response":{"mcp_response":{"jsonrpc":"2.0","id":m["id"],"result":{"protocolVersion":m["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":f["request"]["server_name"],"version":"1"}}}}}})
 for attempt in range(40):
  rid="initial-status-"+str(attempt);request(rid,{"subtype":"mcp_status"});response=wait(rid)
  rows=response.get("response",{}).get("mcpServers",[])
  if len(rows)==2 and all(row["status"]=="connected" for row in rows):break
  time.sleep(.05)
 assert len(rows)==2 and all(row["status"]=="connected" for row in rows),report
 cases=[
  ("reconnect",{"subtype":"mcp_reconnect","serverName":"first"}),
  ("reconnect-missing",{"subtype":"mcp_reconnect","serverName":"missing"}),
  ("set-invalid",{"subtype":"mcp_set_servers","servers":None}),
  ("set-sdk",{"subtype":"mcp_set_servers","servers":{"third":{"type":"sdk","name":"third","connectorId":"connector"}}}),
  ("status-after-set",{"subtype":"mcp_status"}),
  ("set-invalid-sdk",{"subtype":"mcp_set_servers","servers":{"bad":{"type":"sdk"}}}),
  ("set-invalid-transport",{"subtype":"mcp_set_servers","servers":{"bad":{"type":"unknown"}}}),
  ("set-invalid-stdio",{"subtype":"mcp_set_servers","servers":{"bad":{"type":"stdio","command":7}}}),
  ("set-sdk-alias",{"subtype":"mcp_set_servers","servers":{"third":{"type":"sdk","name":"different"}}}),
  ("toggle-sdk",{"subtype":"mcp_toggle","serverName":"third","enabled":False}),
  ("toggle-sdk-on",{"subtype":"mcp_toggle","serverName":"third","enabled":True}),
  ("set-empty",{"subtype":"mcp_set_servers","servers":{}}),
  ("status-after-empty",{"subtype":"mcp_status"}),
 ]
 for rid,body in cases:request(rid,body);wait(rid)
 end=time.monotonic()+1
 while time.monotonic()<end:
  try:next_frame(.1)
  except queue.Empty:pass
 contract={"version":"2.1.322","nativeSha256":sha,"manifest":o.manifest,"handshakesBeforeInitializeReply":report["handshakesBeforeInitializeReply"],"cases":[{"name":rid,"request":body,"response":{k:v for k,v in report["responses"][rid].items() if k!="request_id"}} for rid,body in cases],"sdkMessages":report["sdkMessages"]}
 fixture=Path(__file__).resolve().parents[2]/("crates/runtime/tests/fixtures/mcp_sdk_pending_lifecycle_2_1_322.json" if o.hold_new else "crates/runtime/tests/fixtures/mcp_sdk_manifest_lifecycle_2_1_322.json" if o.manifest else "crates/runtime/tests/fixtures/mcp_sdk_lifecycle_2_1_322.json")
 if o.write_fixture:fixture.write_text(json.dumps(contract,ensure_ascii=True,indent=2)+"\n")
 else:assert json.loads(fixture.read_text())==contract,"SDK lifecycle fixture differs"
 report["passed"]=True
finally:
 (root/"report.json").write_text(json.dumps(report,ensure_ascii=True,indent=2)+"\n")
 if process.poll() is None:os.killpg(process.pid,signal.SIGTERM)
 try:process.wait(timeout=5)
 except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);process.wait()
print(json.dumps({"passed":report.get("passed",False),"handshakesBeforeInitializeReply":report.get("handshakesBeforeInitializeReply"),"receipt":str(root/"report.json")}))
