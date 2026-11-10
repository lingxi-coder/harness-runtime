#!/usr/bin/env python3
"""Hash-pinned native SDK MCP loopback acceptance probe.

Usage: python3 scripts/tests/mcp_sdk_322_live.py --output /tmp/sdk-native-322

Creates an isolated Claude config/cwd, uses fake credentials, and sends one
synthetic model turn exclusively to a local HTTP responder. Records raw control
stdin/stdout and mock model requests. No real provider or external MCP server
is used. The output directory must not already exist.
"""
import argparse, hashlib, json, os, queue, signal, subprocess, threading, time
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from pathlib import Path
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary',type=Path,default=Path.home()/'.local/share/claude/versions/2.1.322')
parser.add_argument('--output',type=Path,required=True)
parser.add_argument('--write-fixture',action='store_true',help='Regenerate the checked-in deterministic SDK control fixture')
parser.add_argument('--interrupt',action='store_true',help='Interrupt the held SDK call and record its cancellation frames instead of the success fixture')
options=parser.parse_args()
BINARY=options.binary.resolve()
SHA='a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450'
assert hashlib.sha256(BINARY.read_bytes()).hexdigest()==SHA
ROOT=options.output.resolve();ROOT.mkdir(parents=True,exist_ok=False)
for name in ['cwd','config','tmp']:(ROOT/name).mkdir()
class API(BaseHTTPRequestHandler):
 calls=0
 requests=0
 def log_message(self,*args):pass
 def do_CONNECT(self):self.send_error(403)
 def do_POST(self):
  raw=self.rfile.read(int(self.headers.get('Content-Length','0')));body=json.loads(raw)
  API.requests+=1
  (ROOT/f'api-{API.requests}.json').write_bytes(raw)
  if self.path.split('?')[0]=='/v1/messages/count_tokens':payload={'input_tokens':100}
  elif self.path.split('?')[0]=='/v1/messages':
   API.calls+=1
   block={'type':'tool_use','id':'toolu_sdk_probe','name':'mcp__fixture__hold','input':{}} if API.calls==1 else {'type':'text','text':'SDK_DONE'}
   payload={'id':f'msg_{API.calls}','type':'message','role':'assistant','model':body['model'],'content':[block], 'stop_reason':'tool_use' if API.calls==1 else 'end_turn','stop_sequence':None,'usage':{'input_tokens':100,'output_tokens':10}}
   if body.get('stream'):
    initial={**payload,'content':[],'stop_reason':None,'usage':{'input_tokens':100,'output_tokens':0}}
    begin={**block,'input':{}} if block['type']=='tool_use' else {'type':'text','text':''}
    delta={'type':'input_json_delta','partial_json':'{}'} if block['type']=='tool_use' else {'type':'text_delta','text':'SDK_DONE'}
    events=[{'type':'message_start','message':initial},{'type':'content_block_start','index':0,'content_block':begin},{'type':'content_block_delta','index':0,'delta':delta},{'type':'content_block_stop','index':0},{'type':'message_delta','delta':{'stop_reason':payload['stop_reason'],'stop_sequence':None},'usage':{'output_tokens':10}},{'type':'message_stop'}]
    data=''.join('event: '+e['type']+'\ndata: '+json.dumps(e)+'\n\n' for e in events).encode()
    self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers();self.wfile.write(data);return
  else:self.send_error(403);return
  data=json.dumps(payload).encode();self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(data)
api=ThreadingHTTPServer(('127.0.0.1',0),API);threading.Thread(target=api.serve_forever,daemon=True).start()
base=f'http://127.0.0.1:{api.server_port}'
env={'PATH':os.defpath,'TERM':'dumb','TMPDIR':str(ROOT/'tmp'),
 'CLAUDE_CONFIG_DIR':str(ROOT/'config'),'ANTHROPIC_API_KEY':'sk-ant-offline-sdk-control-probe',
 'ANTHROPIC_BASE_URL':base,'HTTP_PROXY':'http://127.0.0.1:9',
 'HTTPS_PROXY':'http://127.0.0.1:9','ALL_PROXY':'http://127.0.0.1:9','NO_PROXY':'127.0.0.1,localhost',
 'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC':'1','DISABLE_AUTOUPDATER':'1',
 'DISABLE_TELEMETRY':'1','DISABLE_ERROR_REPORTING':'1','MCP_PROTOCOL_NEGOTIATION':'legacy','ENABLE_TOOL_SEARCH':'false'}
args=[str(BINARY),'--bare','-p','--input-format','stream-json','--output-format','stream-json','--verbose','--permission-mode','bypassPermissions']
p=subprocess.Popen(args,cwd=ROOT/'cwd',env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
lines=queue.Queue();stderr=[]
def reader():
 for raw in p.stdout:
  (ROOT/'native.stdout.jsonl').open('ab').write(raw)
  try:
   frame=json.loads(raw)
   if frame.get('request',{}).get('subtype')=='mcp_message':
    text=raw.decode('utf8');start=text.index('\"message\":')+len('\"message\":');source=text[start:].lstrip();_value,end=json.JSONDecoder().raw_decode(source)
    report.setdefault('rawMessages',[]).append(source[:end])
   lines.put(frame)
  except Exception:lines.put({'raw':raw.decode('utf8','replace')})
 lines.put({'eof':p.poll()})
def error_reader():
 for raw in p.stderr:stderr.append(raw);(ROOT/'native.stderr').open('ab').write(raw)
threading.Thread(target=reader,daemon=True).start();threading.Thread(target=error_reader,daemon=True).start()
def send(value):
 raw=(json.dumps(value,ensure_ascii=True,separators=(',',':'))+'\n').encode()
 (ROOT/'host.stdin.jsonl').open('ab').write(raw);p.stdin.write(raw);p.stdin.flush()
def request(rid,payload):send({'type':'control_request','request_id':rid,'request':payload})
def reply(frame,result):send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{'mcp_response':result}}})
def next_frame(limit=20):
 try:return lines.get(timeout=limit)
 except queue.Empty:raise RuntimeError('native control timeout; '+b''.join(stderr).decode('utf8','replace')[-2000:])
report={'nativeSha256':SHA,'argv':args,'scope':'SDK stream-json control; fake credentials; synthetic model turn served only by loopback HTTP','frames':[]}
try:
 request('init',{'subtype':'initialize','sdkMcpServers':['fixture']})
 initialized=False;listing=None;deadline=time.monotonic()+40
 while time.monotonic()<deadline:
  f=next_frame();report['frames'].append(f)
  if f.get('eof') is not None:raise RuntimeError(str(f))
  r=f.get('request',{})
  if r.get('subtype')!='mcp_message':continue
  m=r['message'];method=m.get('method')
  if method=='initialize':
   report['initialize']=m
   reply(f,{'jsonrpc':'2.0','id':m['id'],'result':{'protocolVersion':m['params']['protocolVersion'],'capabilities':{'tools':{'listChanged':True}},'serverInfo':{'name':'fixture','version':'1'}}})
  elif method=='notifications/initialized':
   report['initializedNotification']=m;reply(f,{'jsonrpc':'2.0','id':0,'result':{}})
  elif method=='tools/list':listing=f;break
 assert listing,report
 reply(listing,{'jsonrpc':'2.0','id':listing['request']['message']['id'],'result':{'tools':[{'name':'hold','description':'Controlled SDK probe','inputSchema':{'type':'object'},'annotations':{'readOnlyHint':True}}]}})
 # Only a connected SDK client receives peer-originated messages.
 for attempt in range(10):
  rid='status-'+str(attempt);request(rid,{'subtype':'mcp_status'})
  while True:
   f=next_frame(10);report['frames'].append(f)
   if f.get('type')=='control_response' and f.get('response',{}).get('request_id')==rid:break
  status=f.get('response',{}).get('response',{})
  if any(row.get('status')=='connected' for row in status.get('mcpServers',[])):break
  time.sleep(.1)
 report['statusBeforeCall']=status
 report['outerEnvelopeReplies']={}
 envelopes=[('missing-server',{'message':{'jsonrpc':'2.0','id':4,'method':'ping'}}),('null-server',{'server_name':None,'message':{'jsonrpc':'2.0','id':4,'method':'ping'}}),('number-server',{'server_name':7,'message':{}}),('missing-message',{'server_name':'fixture'}),('null-message',{'server_name':'fixture','message':None})]
 for rid,payload in envelopes:
  request(rid,{'subtype':'mcp_message',**payload})
  deadline=time.monotonic()+10
  while time.monotonic()<deadline:
   f=next_frame(10);report['frames'].append(f)
   if f.get('type')=='control_response' and f.get('response',{}).get('request_id')==rid:report['outerEnvelopeReplies'][rid]=f;break
  assert rid in report['outerEnvelopeReplies'],report
  assert report['outerEnvelopeReplies'][rid]['response']['subtype']=='success',report
 report['hostMethodReplies']={}
 for method in ['roots/list','elicitation/create','fixture/unknown']:
  rid='peer-'+method;peer_id=rid+'\ud800'
  request(rid,{'subtype':'mcp_message','server_name':'fixture','message':{'jsonrpc':'2.0','id':peer_id,'method':method,'params':{}}})
  ack=False;result=None;deadline=time.monotonic()+10
  while time.monotonic()<deadline and not(ack and result is not None):
   f=next_frame(10);report['frames'].append(f)
   if f.get('type')=='control_response' and f.get('response',{}).get('request_id')==rid:ack=True
   if f.get('request',{}).get('subtype')=='mcp_message':
    m=f['request']['message']
    if m.get('id')==peer_id and ('result' in m or 'error' in m):result=m;reply(f,{'jsonrpc':'2.0','id':0,'result':{}})
  assert ack and result is not None,report
  report['hostMethodReplies'][method]=result
  assert result['error']=={'code':-32601,'message':'Method not found'},report
 send({'type':'user','message':{'role':'user','content':'Call the fixture hold tool once.'}})
 call=None;deadline=time.monotonic()+10
 while time.monotonic()<deadline:
  f=next_frame(10);report['frames'].append(f)
  if f.get('request',{}).get('subtype')=='mcp_message' and f['request']['message'].get('method')=='tools/call':call=f;break
  if f.get('response',{}).get('request_id')=='direct-call':report['directCallEarly']=f;break
 assert call,report
 report['outgoingCall']=call['request']['message']
 assert report['initialize']['id']==0,report
 assert report['initialize']['params']['capabilities']=={},report
 assert report['initializedNotification']=={'jsonrpc':'2.0','method':'notifications/initialized'},report
 assert listing['request']['message']['id']==1,report
 assert report['outgoingCall']['id']==2,report
 assert report['outgoingCall']['params']['_meta']=={'claudecode/toolUseId':'toolu_sdk_probe','progressToken':2},report
 request('peer-ping',{'subtype':'mcp_message','server_name':'fixture','message':{'jsonrpc':'2.0','id':'peer\ud800','method':'ping'}})
 seen_ack=False;seen_ping=False
 deadline=time.monotonic()+10
 while time.monotonic()<deadline and not(seen_ack and seen_ping):
  f=next_frame(10);report['frames'].append(f)
  if f.get('type')=='control_response' and f.get('response',{}).get('request_id')=='peer-ping':seen_ack=True;report['peerAck']=f
  if f.get('request',{}).get('subtype')=='mcp_message':
   m=f['request']['message']
   if m.get('id')=='peer\ud800' and 'result' in m:
    seen_ping=True;report['concurrentPingResponse']=m;reply(f,{'jsonrpc':'2.0','id':0,'result':{}})
 report['peerAckBeforeCallSettles']=seen_ack;report['peerReplyBeforeCallSettles']=seen_ping
 assert seen_ack and seen_ping,report
 if options.interrupt:
  request('interrupt',{'subtype':'interrupt'})
  deadline=time.monotonic()+15
  while time.monotonic()<deadline:
   try:f=next_frame(2)
   except RuntimeError:continue
   report['frames'].append(f)
   if f.get('request',{}).get('subtype')=='mcp_message':
    m=f['request']['message'];reply(f,{'jsonrpc':'2.0','id':0,'result':{}})
    if m.get('method')=='notifications/cancelled':report['cancellation']=m
   if report.get('cancellation') and f.get('type')=='result':break
  assert report.get('cancellation'),report
  assert report['cancellation']['params']['requestId']==call['request']['message']['id'],report
  report['passed']=True
  (ROOT/'report.json').write_text(json.dumps(report,ensure_ascii=True,indent=2)+'\n')
  print(json.dumps({'passed':True,'cancellation':report['cancellation'],'receipt':str(ROOT/'report.json')},ensure_ascii=True))
  raise SystemExit(0)
 reply(call,{'jsonrpc':'2.0','id':call['request']['message']['id'],'result':{'content':[{'type':'text','text':'SDK_PROBE_RESULT'}]}})
 for rid,server,message in [('missing','missing',{'jsonrpc':'2.0','id':1,'method':'ping'}),('invalid','fixture',{'invalid':True})]:
  request(rid,{'subtype':'mcp_message','server_name':server,'message':message})
  deadline=time.monotonic()+10
  while time.monotonic()<deadline:
   f=next_frame(10);report['frames'].append(f)
   if f.get('request',{}).get('subtype')=='mcp_message':reply(f,{'jsonrpc':'2.0','id':0,'result':{}})
   if f.get('type')=='control_response' and f.get('response',{}).get('request_id')==rid:report[rid]=f;break
  assert rid in report,report
  assert report[rid]['response']['subtype']=='success',report
 contracts={'version':'2.1.322','nativeSha256':SHA,'scope':'Raw nested SDK control message JSON, malformed outer-envelope ACKs and unsupported host requests from the complete native CLI with a loopback mock provider. Client identity remains native in this reference fixture.',
  'nativeMessages':report['rawMessages'],
  'ingressCases':[{'name':rid,'request':{'subtype':'mcp_message',**payload},'responseSubtype':report['outerEnvelopeReplies'][rid]['response']['subtype']} for rid,payload in envelopes]}
 fixture=Path(__file__).resolve().parents[2]/'crates/runtime/tests/fixtures/mcp_sdk_control_2_1_322.json'
 rendered=json.dumps(contracts,ensure_ascii=True,indent=2)+'\n'
 if options.write_fixture:fixture.write_text(rendered)
 else:assert json.loads(fixture.read_text())==contracts,'SDK native control fixture differs; inspect raw receipts before accepting changes'
 report['fixtureMatches']=True
 report['passed']=True
finally:
 (ROOT/'report.json').write_text(json.dumps(report,ensure_ascii=True,indent=2)+'\n')
 api.shutdown();api.server_close()
 if p.poll() is None:os.killpg(p.pid,signal.SIGTERM)
 try:p.wait(timeout=5)
 except subprocess.TimeoutExpired:os.killpg(p.pid,signal.SIGKILL);p.wait()
print(json.dumps({k:v for k,v in report.items() if k not in ['frames','argv']},ensure_ascii=True,indent=2))
