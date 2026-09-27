import http.server, threading, json, subprocess, os, tempfile, pathlib, sys, hashlib
# Capture an actual native SDK request using only a localhost endpoint and isolated config.
binary = pathlib.Path(sys.argv[1]) / "package/claude"
binary_sha256 = hashlib.sha256(binary.read_bytes()).hexdigest()
assert binary_sha256 == "a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807"
captured=[]
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers.get('content-length',0))))
  if 'tools' in body:
   captured.append({'path':self.path,'tools':body['tools']})

  response={'id':'msg_oracle','type':'message','role':'assistant','model':'claude-sonnet-4-6','content':[{'type':'text','text':'ok'}],'stop_reason':'end_turn','stop_sequence':None,'usage':{'input_tokens':1,'output_tokens':1}}
  if body.get('stream'):
   events=[('message_start',{'type':'message_start','message':{**response,'content':[],'stop_reason':None}}),('content_block_start',{'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}}),('content_block_delta',{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'ok'}}),('content_block_stop',{'type':'content_block_stop','index':0}),('message_delta',{'type':'message_delta','delta':{'stop_reason':'end_turn','stop_sequence':None},'usage':{'output_tokens':1}}),('message_stop',{'type':'message_stop'})]
   raw=''.join('event: '+kind+'\ndata: '+json.dumps(event)+'\n\n' for kind,event in events).encode();ctype='text/event-stream'
  else: raw=json.dumps(response if 'messages' in body else {'input_tokens':1}).encode();ctype='application/json'
  self.send_response(200);self.send_header('content-type',ctype);self.send_header('content-length',str(len(raw)));self.end_headers();self.wfile.write(raw)
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
with tempfile.TemporaryDirectory(prefix='loop-wire-oracle-') as temp:
 env={**os.environ,'ANTHROPIC_API_KEY':'parity-local-only','ANTHROPIC_BASE_URL':f'http://127.0.0.1:{server.server_port}','CLAUDE_CONFIG_DIR':temp,'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1','DISABLE_TELEMETRY':'1'}
 for key in ['ANTHROPIC_AUTH_TOKEN','CLAUDE_CODE_OAUTH_TOKEN']: env.pop(key,None)
 try:
  result=subprocess.run([str(binary),'--bare','--setting-sources','','--no-session-persistence','--model','claude-sonnet-4-6','--tools','Bash,CronCreate,CronDelete,CronList,ScheduleWakeup,Monitor','--print','Reply ok'],cwd=temp,env=env,capture_output=True,text=True,timeout=45)
  print('exit',result.returncode,'stdout',result.stdout[:500],'stderr',result.stderr[:1000])
 except subprocess.TimeoutExpired: print('native timed out after capture')
server.shutdown()
for request in captured:
 print('captured',request['path'],[(t.get('name'),t.get('input_schema',{}).get('$schema')) for t in request['tools']])

assert captured and result.returncode == 0, "Native SDK request was not captured successfully"
summary = {"version":"2.1.270", "binarySha256":binary_sha256, "requests":[{"path":request["path"], "toolSchemas":[{"name":tool["name"], "$schema":tool["input_schema"].get("$schema")} for tool in request["tools"]]} for request in captured]}
output = pathlib.Path(__file__).resolve().parent.parent / "fixtures/provider_schema_metadata_2_1_270.json"
output.write_text(json.dumps(summary,indent=2)+"\n")
