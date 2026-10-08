#!/usr/bin/env python3
"""Capture isolated native hosted Messages fields against a loopback mock."""
from pathlib import Path
import hashlib,json,sys,threading,subprocess,os,signal
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from compact_live_oracle import isolated_env
binary=Path(sys.argv[3]);assert hashlib.sha256(binary.read_bytes()).hexdigest()=='bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750'
scenario=sys.argv[1];assert scenario in ['bedrock-explicit','bedrock-baseline','vertex-explicit'];root=Path(sys.argv[2]);root.mkdir(exist_ok=False)
for d in ['config','work','tmp']:(root/d).mkdir(exist_ok=True)
rows=[]
class Mock(BaseHTTPRequestHandler):
 def log_message(self,*_):pass
 def reject(self):self.send_error(403,'loopback oracle only')
 do_CONNECT=reject;do_GET=reject
 def do_POST(self):
  raw=self.rfile.read(int(self.headers.get('Content-Length','0')));rows.append({'path':self.path,'headers':list(self.headers.items()),'raw':raw.decode(),'body':json.loads(raw)});(root/'requests.json').write_text(json.dumps(rows,indent=2));self.send_response(400);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(b'{"type":"error","error":{"type":"invalid_request_error","message":"fixture rejection"}}')
server=ThreadingHTTPServer(('127.0.0.1',0),Mock);threading.Thread(target=server.serve_forever,daemon=True).start();url=f'http://127.0.0.1:{server.server_port}'
env=isolated_env(root,url);env.update(CLAUDE_CODE_USE_BEDROCK='1',CLAUDE_CODE_SKIP_BEDROCK_AUTH='1',ANTHROPIC_BEDROCK_BASE_URL=url,AWS_REGION='us-east-1',AWS_ACCESS_KEY_ID='fixture-access',AWS_SECRET_ACCESS_KEY='fixture-secret',CLAUDE_CODE_EXTRA_BODY=json.dumps({'betas':['structured-outputs-2025-12-15'],'output_config':{'format':{'type':'json_schema','schema':{'type':'object','properties':{'ok':{'type':'boolean'}},'required':['ok'],'additionalProperties':False}}}}))
if scenario=='bedrock-baseline':
 extra=json.loads(env['CLAUDE_CODE_EXTRA_BODY']);del extra['betas'];env['CLAUDE_CODE_EXTRA_BODY']=json.dumps(extra)
if scenario=='vertex-explicit':
 for k in ['CLAUDE_CODE_USE_BEDROCK','CLAUDE_CODE_SKIP_BEDROCK_AUTH','ANTHROPIC_BEDROCK_BASE_URL','AWS_REGION','AWS_ACCESS_KEY_ID','AWS_SECRET_ACCESS_KEY']:env.pop(k,None)
 env.update(CLAUDE_CODE_USE_VERTEX='1',CLAUDE_CODE_SKIP_VERTEX_AUTH='1',ANTHROPIC_VERTEX_BASE_URL=url,ANTHROPIC_VERTEX_PROJECT_ID='fixture-project',CLOUD_ML_REGION='us-east5')
cmd=[str(binary),'--bare','-p','wire probe','--model','claude-sonnet-4-6','--system-prompt','fixture','--output-format','stream-json','--verbose','--setting-sources','','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--tools','','--debug-file',str(root/'debug.log')]
p=subprocess.Popen(cmd,cwd=root/'work',env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,start_new_session=True)
try:o,e=p.communicate(timeout=45)
except subprocess.TimeoutExpired:
 os.killpg(p.pid,signal.SIGTERM);o,e=p.communicate(timeout=3)
finally:server.shutdown();server.server_close()
(root/'stdout.jsonl').write_bytes(o);(root/'stderr.txt').write_bytes(e);print(json.dumps({'exit_code':p.returncode,'requests':len(rows),'output':str(root),'stderr':e.decode(errors='replace')[-1000:]}))
assert p.returncode==1 and len(rows)==1, 'expected one scripted invalid-request rejection'
r=rows[0];headers={n.lower():v for n,v in r['headers']};body=r['body']
assert headers['anthropic-version']=='2023-06-01'
assert headers['anthropic-beta']==('claude-code-20250219,effort-2025-11-24' if scenario=='bedrock-baseline' else 'structured-outputs-2025-12-15')
assert 'betas' not in body and 'model' not in body
assert body['output_config']['format']['type']=='json_schema'
if scenario=='vertex-explicit':
 assert body['anthropic_version']=='vertex-2023-10-16' and 'anthropic_beta' not in body
 assert r['path'].endswith(':streamRawPredict')
else:
 assert body['anthropic_version']=='bedrock-2023-05-31' and 'stream' not in body
 assert body['anthropic_beta']==['interleaved-thinking-2025-05-14']
 assert r['path'].endswith('/invoke-with-response-stream')
report={'scenario':scenario,'passed':True,'native_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'exit_code':p.returncode,'sdk_version_header':headers['anthropic-version'],'betas':headers['anthropic-beta'],'body_betas':body.get('anthropic_beta'),'scope':'Actual --bare native cloud request with isolated fake credentials, skip cloud auth, loopback scripted 400. Header/format/version placement, not signed/live cloud acceptance.'}
(root/'report.json').write_text(json.dumps(report,indent=2)+'\n');print('native hosted request projection OK')
