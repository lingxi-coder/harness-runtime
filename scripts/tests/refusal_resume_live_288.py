#!/usr/bin/env python3
"""Native resume header acquisition from an isolated flat refusal transcript."""
import hashlib,json,os,signal,subprocess,sys,tempfile,threading,uuid
from pathlib import Path
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from compact_live_oracle import isolated_env
binary=Path(sys.argv[1]);mode=sys.argv[2] if len(sys.argv)>2 else 'armed';assert mode in ['armed','neutralized']
native_sha=hashlib.sha256(binary.read_bytes()).hexdigest();assert native_sha=='bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750'
root=Path(tempfile.mkdtemp(prefix='harness-288-refusal-resume-'))
for name in ['config','work','tmp']:(root/name).mkdir()
model='claude-opus-4-7';sid=str(uuid.uuid4());ids=[str(uuid.uuid4()) for _ in range(3)];cwd=str(root/'work')
base={'sessionId':sid,'timestamp':'2026-10-03T00:00:00.000Z','cwd':cwd,'version':'2.1.288','isSidechain':False,'userType':'external'}
transcript=[
 dict(base,type='user',uuid=ids[0],parentUuid=None,message={'role':'user','content':[{'type':'text','text':'saved fixture'}]}),
 dict(base,type='system',uuid=ids[1],parentUuid=ids[0],subtype='model_refusal_fallback',direction='retry',scope='session',content='native refusal notice',level='warning',trigger='refusal',originalModel='claude-sonnet-4-6',fallbackModel=model,requestId='resume-origin ',apiRefusalCategory='cyber',apiRefusalExplanation=None,sawCyberRefusal=True,neutralizedByFork=mode=='neutralized',isMeta=False),
 dict(base,type='assistant',uuid=ids[2],parentUuid=ids[1],message={'id':'msg_saved','type':'message','role':'assistant','model':model,'content':[{'type':'text','text':'saved success'}],'stop_reason':'end_turn','usage':{'input_tokens':1,'output_tokens':1}}),
]
path=root/'saved.jsonl';path.write_text(''.join(json.dumps(row)+'\n' for row in transcript))
rows=[]
class Mock(BaseHTTPRequestHandler):
 def log_message(self,*_):pass
 def reject(self):self.send_error(403,'Only loopback oracle is available')
 do_CONNECT=reject;do_GET=reject
 def do_POST(self):
  body=json.loads(self.rfile.read(int(self.headers.get('Content-Length','0'))));assert self.path.split('?')[0]=='/v1/messages'
  rows.append({'body':body,'headers':list(self.headers.items())});(root/'requests.json').write_text(json.dumps(rows,indent=2))
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
  frames=[{'type':'message_start','message':{'id':'msg_success','type':'message','role':'assistant','model':body['model'],'content':[],'usage':{'input_tokens':1,'output_tokens':0}}},{'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}},{'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'success'}},{'type':'content_block_stop','index':0},{'type':'message_delta','delta':{'stop_reason':'end_turn'},'usage':{'output_tokens':1}},{'type':'message_stop'}]
  for frame in frames:self.wfile.write(('event: '+frame['type']+'\ndata: '+json.dumps(frame)+'\n\n').encode())
  self.wfile.flush();self.close_connection=True
server=ThreadingHTTPServer(('127.0.0.1',0),Mock);threading.Thread(target=server.serve_forever,daemon=True).start()
env=isolated_env(root,f'http://127.0.0.1:{server.server_port}');env.update(CLAUDE_CODE_MAX_RETRIES='0',_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL='1')
command=[str(binary),'--bare','-p','--resume',str(path),'--input-format','stream-json','--output-format','stream-json','--verbose','--model',model,'--system-prompt','fixture','--setting-sources','','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--tools','','--debug-file',str(root/'debug.log')]
process=subprocess.Popen(command,cwd=root/'work',env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
try:
 output,error=process.communicate(json.dumps({'type':'user','session_id':'','message':{'role':'user','content':'next fixture'}})+'\n',timeout=40)
finally:
 if process.poll() is None:os.killpg(process.pid,signal.SIGTERM);process.wait(timeout=5)
 server.shutdown();server.server_close()
(root/'stdout.jsonl').write_text(output);(root/'stderr.txt').write_text(error)
assert process.returncode==0,{'output':str(root),'exit':process.returncode,'stderr':error[-2500:],'stdout':output[-1500:]}
assert len(rows)==1,{'output':str(root),'requests':len(rows)}
headers={k.lower():v for k,v in rows[0]['headers']};expected=mode=='armed'
assert (headers.get('x-is-refusal-fallback')=='true')==expected,{'output':str(root),'headers':headers}
assert headers.get('x-cc-fallback-latched-by')==('resume-origin' if expected else None),{'output':str(root),'headers':headers}
report={'passed':True,'mode':mode,'exit_code':process.returncode,'native_sha256':native_sha,'requests':len(rows),'refusal_header':headers.get('x-is-refusal-fallback'),'origin_header':headers.get('x-cc-fallback-latched-by'),'output':str(root),'scope':'Actual native --bare --resume loads a flat cyber refusal transcript and performs one subsequent query; a neutralized notice does not arm the outgoing headers. First-party routing is explicitly assumed over the isolated loopback gateway. Fake credentials, no live-provider acceptance or complete server-swapped response acquisition.'}
(root/'report.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
