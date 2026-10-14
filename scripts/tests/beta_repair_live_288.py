#!/usr/bin/env python3
"""Native healed display probe and a later user query, using only a loopback API."""
import hashlib,json,os,queue,signal,subprocess,sys,tempfile,threading,time
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from compact_live_oracle import isolated_env
binary=Path(sys.argv[1]);native_sha=hashlib.sha256(binary.read_bytes()).hexdigest();assert native_sha=='bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750'
mode=sys.argv[2] if len(sys.argv)>2 else 'strong';assert mode in ['strong','weak','shape'];simulated=False
root=Path(tempfile.mkdtemp(prefix='harness-288-display-'))
for name in ['config','work','tmp']:(root/name).mkdir()
rows=[];lock=threading.Lock()
class Mock(BaseHTTPRequestHandler):
 def log_message(self,*_):pass
 def reject(self):self.send_error(403,'Only loopback oracle is available')
 do_CONNECT=reject;do_GET=reject
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))));assert self.path.split('?')[0]=='/v1/messages'
  with lock:
   index=len(rows);rows.append({'body':body,'headers':list(self.headers.items())});(root/'requests.json').write_text(json.dumps(rows,indent=2))
  if index==0:
   self.send_response(400);self.send_header('Content-Type','application/json');self.send_header('x-should-retry','false');self.end_headers();self.wfile.write(json.dumps({'type':'error','error':{'type':'invalid_request_error','message':{'strong':'Unexpected value(s) `thinking-display-updates-2026-08-18` for the `anthropic-beta` header','weak':'Unrecognized thinking-display-updates-2026-08-18 in anthropic-beta','shape':'thinking.adaptive.display: Input should be omitted'}[mode]}}).encode());return
  if index==1:
   self.send_response(418);self.send_header('Content-Type','application/json');self.send_header('x-should-retry','false');self.end_headers();self.wfile.write(json.dumps({'type':'error','error':{'type':'api_error','message':'fixture terminal refusal'}}).encode());return
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  frames=[{'type':'message_start','message':{'id':f'msg_{index}','type':'message','role':'assistant','model':body['model'],'content':[],'usage':{'input_tokens':1,'output_tokens':0}}},{'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}},{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'success'}},{'type':'content_block_stop','index':0},{'type':'message_delta','delta':{'stop_reason':'end_turn'},'usage':{'output_tokens':1}},{'type':'message_stop'}]
  for frame in frames:self.wfile.write(('event: '+frame['type']+'\ndata: '+json.dumps(frame)+'\n\n').encode())
  self.wfile.flush();self.close_connection=True
server=ThreadingHTTPServer(('127.0.0.1',0),Mock);threading.Thread(target=server.serve_forever,daemon=True).start()
env=isolated_env(root,f'http://127.0.0.1:{server.server_port}');env['CLAUDE_CODE_MAX_RETRIES']='0'

command=[str(binary),'--bare','-p','--input-format','stream-json','--output-format','stream-json','--verbose','--model','claude-opus-4-7','--system-prompt','fixture','--setting-sources','','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--tools','','--debug-file',str(root/'debug.log')]
process=subprocess.Popen(command,cwd=root/'work',env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
events=queue.Queue();output=[]
def read():
 for line in process.stdout:
  output.append(line)
  try:events.put(json.loads(line))
  except json.JSONDecodeError:pass
threading.Thread(target=read,daemon=True).start()
def prompt(text,failed=False):
 process.stdin.write(json.dumps({'type':'user','session_id':'','message':{'role':'user','content':text}})+'\n');process.stdin.flush()
 deadline=time.monotonic()+35
 while time.monotonic()<deadline:
  item=events.get(timeout=max(.1,deadline-time.monotonic()))
  if item.get('type')=='result':assert bool(item.get('is_error'))==failed,item;return item
 raise AssertionError('native display probe exceeded bound')
try:
 first=prompt('first fixture',True)
 if not simulated:second=prompt('second fixture')
 process.stdin.close();exit_code=process.wait(timeout=5)
 assert exit_code==(1 if simulated else 0),exit_code
finally:
 if process.poll() is None:os.killpg(process.pid,signal.SIGTERM);process.wait(timeout=5)
 server.shutdown();server.server_close()
(root/'stdout.jsonl').write_text(''.join(output));(root/'stderr.txt').write_text(process.stderr.read())
assert len(rows)==(1 if simulated else 3),rows
assert all(row['body'].get('stream') is True for row in rows)
if not simulated:assert rows[0]['body']['messages']==rows[1]['body']['messages']
for index,row in enumerate(rows):
 betas=dict((k.lower(),v) for k,v in row['headers']).get('anthropic-beta','').split(',')
 assert (row['body']['thinking'].get('display')=='updates')==(not simulated and index==0),rows
 assert ('thinking-display-updates-2026-08-18' in betas)==(not simulated and index==0),rows
if not simulated:assert rows[2]['body']['messages']!=rows[0]['body']['messages']
report={'passed':True,'mode':mode,'exit_code':exit_code,'native_sha256':native_sha,'requests':len(rows),'updates':[row['body']['thinking'].get('display') for row in rows],'output':str(root),'scope':'Native --bare named display beta refusal, terminal failed retry, then a later query; immediate retirement is independent of success. Fake credentials and loopback-only network; full beta/classifier/provider envelopes excluded.'}
(root/'report.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
