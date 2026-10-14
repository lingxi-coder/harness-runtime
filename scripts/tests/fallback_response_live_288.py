#!/usr/bin/env python3
"""Native unarmed fallback control handling and subsequent replay, loopback only."""
import hashlib,json,os,queue,signal,subprocess,sys,tempfile,threading,time
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from compact_live_oracle import isolated_env
binary=Path(sys.argv[1]);native_sha=hashlib.sha256(binary.read_bytes()).hexdigest();assert native_sha=='bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750'
mode=sys.argv[2] if len(sys.argv)>2 else 'valid';assert mode in ['valid','malformed']
root=Path(tempfile.mkdtemp(prefix='harness-288-fallback-response-'))
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
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  frames=[{'type':'message_start','message':{'id':f'msg_{index}','type':'message','role':'assistant','model':body['model'],'content':[],'usage':{'input_tokens':1,'output_tokens':0}}},{'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}},{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'success'}},{'type':'content_block_stop','index':0},{'type':'message_delta','delta':{'stop_reason':'end_turn'},'usage':{'output_tokens':1}},{'type':'message_stop'}]
  if index==0:
   for frame in frames:
    if 'index' in frame:frame['index']+=1
   control={'type':'fallback','from':{'model':body['model']},'to':{'model':'served-target' if mode=='valid' else ''},'trigger':{'type':'refusal','category':'cyber'}}
   frames.insert(1,{'type':'content_block_start','index':0,'content_block':control})
   frames.insert(2,{'type':'content_block_stop','index':0})
   frames[-2]['usage']['iterations']=[{'type':'fallback_message','model':'usage-target','input_tokens':1.5}]
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
def prompt(text):
 process.stdin.write(json.dumps({'type':'user','session_id':'','message':{'role':'user','content':text}})+'\n');process.stdin.flush()
 deadline=time.monotonic()+35
 while time.monotonic()<deadline:
  item=events.get(timeout=max(.1,deadline-time.monotonic()))
  if item.get('type')=='result':assert not item.get('is_error'),item;return item
 raise AssertionError('native fallback response probe exceeded bound')
try:
 first=prompt('first fixture')
 second=prompt('second fixture')
 process.stdin.close();exit_code=process.wait(timeout=5)
 assert exit_code==0,exit_code
finally:
 if process.poll() is None:os.killpg(process.pid,signal.SIGTERM);process.wait(timeout=5)
 server.shutdown();server.server_close()
(root/'stdout.jsonl').write_text(''.join(output));(root/'stderr.txt').write_text(process.stderr.read())
assert len(rows)==2,rows
assert all(row['body'].get('stream') is True for row in rows)
assert rows[1]['body']['model']==rows[0]['body']['model']
assert all(block.get('type')!='fallback' for message in rows[1]['body']['messages'] for block in message['content'] if isinstance(block,dict))
report={'passed':True,'mode':mode,'exit_code':exit_code,'native_sha256':native_sha,'requests':len(rows),'output':str(root),'scope':'Actual native --bare unarmed fallback controls, valid or malformed, followed by a second user query. Control blocks do not enter replay and the requested model remains unchanged. Fake credentials and isolated loopback only; admission, server swaps, salvage and cost are not established.'}
(root/'report.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
