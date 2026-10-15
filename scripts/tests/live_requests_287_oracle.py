#!/usr/bin/env python3
"""Capture actual SHA-pinned 2.1.287 CLI requests against a loopback mock.

--bare skips user/keychain discovery; the child gets only the isolated environment
from compact_live_oracle. Proxy CONNECT and unknown paths are refused. Model
responses are scripted, including unavailable-tool recovery; no remote LLM runs or child-agent acceptance.
"""
from __future__ import annotations
import argparse, hashlib, json, os, signal, subprocess, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from compact_live_oracle import isolated_env

SHA='6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea'
SCHEMA={'type':'json_schema','schema':{'type':'object','properties':{'ok':{'type':'boolean'}},'required':['ok'],'additionalProperties':False}}
SCENARIOS={
 'baseline':{},
 'numeric':{'extra':{'numbers':{'large':1e21,'small':1e-7,'zero':-0.0,'integer':18446744073709551615,'2':1.0,'0':-0.0}}},
 'parameter-headers':{'extra':{'betas':['AFK-MODE','custom'],'user_profile_id':' profile ','workspace_id':' workspace '}},
 'output-format':{'extra':{'output_format':SCHEMA,'output_config':{'effort':'low'}}},
 'output-conflict':{'extra':{'output_format':SCHEMA,'output_config':{'format':SCHEMA}}},
 'metadata-lone':{'extra':{'metadata':{'user_id':'{"tk":"secret","id":"\\ud800","\\udc00":"value"}'}}},
 'schema':{'schema':True},
 'unavailable-agent':{'agent':True},
}
def dump(path,value):path.write_text(json.dumps(value,ensure_ascii=True,indent=2)+'\n')
def user_blocks(body):
 for message in reversed(body.get('messages',[])):
  if message.get('role')=='user':
   content=message.get('content',[]);return [{'type':'text','text':content}] if isinstance(content,str) else content
 return []
def make_handler(root,scenario,requests,refused):
 lock=threading.Lock()
 class Mock(BaseHTTPRequestHandler):
  def log_message(self,*args):pass
  def reject(self):
   refused.append(self.path);self.send_error(403,'Only the loopback oracle is available')
  do_CONNECT=reject
  do_GET=reject
  def do_POST(self):
   if self.path.split('?')[0] not in ['/v1/messages','/v1/messages/count_tokens']:return self.reject()
   raw=self.rfile.read(int(self.headers.get('Content-Length','0')));body=json.loads(raw)
   with lock:
    index=len(requests)+1
    requests.append({'path':self.path,'headers':list(self.headers.items()),'body':body,'keys':list(body),'body_sha256':hashlib.sha256(raw).hexdigest()})
    (root/f'request-{index:02}.json').write_bytes(raw);dump(root/'requests.json',requests)
   if 'count_tokens' in self.path:return self.respond_json({'input_tokens':100})
   blocks=user_blocks(body);text='\n'.join(b.get('text','') for b in blocks if b.get('type')=='text')
   has_result=any(b.get('type')=='tool_result' for b in blocks)
   if scenario=='unavailable-agent' and 'WIRE_ROOT_AGENT' in text and not has_result:
    content=[{'type':'tool_use','id':'toolu_wire_agent','name':'Agent','input':{'description':'Wire oracle child','prompt':'WIRE_CHILD_AGENT','subagent_type':'wire-child'}}];reason='tool_use'
   elif scenario=='schema' and not has_result and any(t.get('name')=='StructuredOutput' for t in body.get('tools',[])):
    content=[{'type':'tool_use','id':'toolu_wire_schema','name':'StructuredOutput','input':{'ok':True}}];reason='tool_use'
   else:content=[{'type':'text','text':'WIRE_CHILD_DONE' if 'WIRE_CHILD_AGENT' in text else 'WIRE_DONE'}];reason='end_turn'
   message={'id':f'msg_wire_{index}','type':'message','role':'assistant','model':body['model'],'content':[],'stop_reason':None,'stop_sequence':None,'usage':{'input_tokens':100,'output_tokens':0,'cache_creation_input_tokens':0,'cache_read_input_tokens':0}}
   if not body.get('stream'):
    message.update(content=content,stop_reason=reason);message['usage']['output_tokens']=10;return self.respond_json(message)
   events=[{'type':'message_start','message':message}]
   for i,block in enumerate(content):
    start={**block,'input':{}} if block['type']=='tool_use' else {'type':'text','text':''}
    delta={'type':'input_json_delta','partial_json':json.dumps(block['input'])} if block['type']=='tool_use' else {'type':'text_delta','text':block['text']}
    events.extend([{'type':'content_block_start','index':i,'content_block':start},{'type':'content_block_delta','index':i,'delta':delta},{'type':'content_block_stop','index':i}])
   events.extend([{'type':'message_delta','delta':{'stop_reason':reason,'stop_sequence':None},'usage':{'output_tokens':10}},{'type':'message_stop'}])
   self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
   try:
    for event in events:self.wfile.write(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n".encode());self.wfile.flush()
   except (BrokenPipeError,ConnectionResetError):pass
  def respond_json(self,body):
   self.send_response(200);self.send_header('Content-Type','application/json');self.end_headers();self.wfile.write(json.dumps(body).encode())
 return Mock

def run(binary,root,name,settings,timeout):
 root.mkdir();[(root/d).mkdir() for d in ['config','work','tmp']]
 requests=[];refused=[];dump(root/'requests.json',requests)
 server=ThreadingHTTPServer(('127.0.0.1',0),make_handler(root,name,requests,refused));thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
 url=f'http://127.0.0.1:{server.server_port}';env=isolated_env(root,url)
 if 'extra' in settings:env['CLAUDE_CODE_EXTRA_BODY']=json.dumps(settings['extra'],ensure_ascii=True)
 prompt='WIRE_ROOT_AGENT' if settings.get('agent') else 'WIRE_ROOT_PROBE'
 command=[str(binary),'--bare','-p',prompt,'--model','claude-sonnet-4-6','--system-prompt','WIRE_SYSTEM','--output-format','stream-json','--verbose','--setting-sources','','--strict-mcp-config','--mcp-config','{"mcpServers":{}}','--tools','Agent' if settings.get('agent') else '', '--debug-file',str(root/'debug.log')]
 if settings.get('schema'):command+=['--json-schema',json.dumps(SCHEMA['schema'])]
 if settings.get('agent'):command+=['--permission-mode','bypassPermissions','--dangerously-skip-permissions','--agents',json.dumps({'wire-child':{'description':'Local wire probe','prompt':'WIRE_CHILD_AGENT','tools':[]}})]
 dump(root/'metadata.json',{'command':command,'synthetic_model_usage':True,'isolated_environment':True,'extra':settings.get('extra')})
 process=subprocess.Popen(command,cwd=root/'work',env=env,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
 try:
  stdout,stderr=process.communicate(timeout=timeout)
 except subprocess.TimeoutExpired:
  os.killpg(process.pid,signal.SIGTERM)
  try:stdout,stderr=process.communicate(timeout=3)
  except subprocess.TimeoutExpired:os.killpg(process.pid,signal.SIGKILL);stdout,stderr=process.communicate(timeout=3)
  (root/'stdout.jsonl').write_text(stdout);(root/'stderr.txt').write_text(stderr);raise
 finally:server.shutdown();server.server_close();thread.join(timeout=2)
 (root/'stdout.jsonl').write_text(stdout);(root/'stderr.txt').write_text(stderr)
 messages=[r for r in requests if 'count_tokens' not in r['path']]
 if name=='output-conflict':
  assert not messages and 'Both output_format and output_config.format' in stdout+stderr
 else:
  assert process.returncode==0,(name,process.returncode,stderr[-1500:]);assert messages
  events=[json.loads(line) for line in stdout.splitlines() if line.strip()]
  results=[event for event in events if event.get('type')=='result']
  assert results and results[-1].get('subtype')=='success' and results[-1].get('is_error') is False
  if name=='schema':assert results[-1].get('structured_output')=={'ok':True}
 if name=='metadata-lone':assert messages[0]['body']['metadata']['user_id']=='{"id":"\\ud800","\\udc00":"value"}'
 if name=='parameter-headers':
  h={k.lower():v for k,v in messages[0]['headers']};assert h['anthropic-beta']=='custom';assert h['anthropic-user-profile-id']=='profile';assert h['anthropic-workspace-id']=='workspace'
  assert not any(k in messages[0]['body'] for k in ['betas','user_profile_id','workspace_id'])
 if name=='output-format':assert 'output_format' not in messages[0]['body'] and messages[0]['body']['output_config']['format']==SCHEMA
 if name=='unavailable-agent':
  assert any(b.get('type')=='tool_result' and b.get('is_error') and 'No such tool available: Agent' in b.get('content','') for r in messages for b in user_blocks(r['body']))
 report={'scenario':name,'passed':True,'exit_code':process.returncode,'request_count':len(requests),'refused_paths':refused,'request_shapes':[{'path':r['path'],'keys':r['keys'],'stream':r['body'].get('stream'),'tools':[t.get('name') for t in r['body'].get('tools',[])]} for r in requests]};dump(root/'report.json',report);return report

def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('binary',type=Path);p.add_argument('--output',type=Path,required=True);p.add_argument('--scenario',choices=['all',*SCENARIOS],default='all');p.add_argument('--timeout',type=float,default=45);args=p.parse_args()
 binary=args.binary.resolve(strict=True);assert hashlib.sha256(binary.read_bytes()).hexdigest()==SHA
 args.output.mkdir(parents=True);reports=[]
 for name,settings in SCENARIOS.items():
  if args.scenario!='all' and args.scenario!=name:continue
  report=run(binary,args.output/name,name,settings,args.timeout);reports.append(report);print(f'PASS {name}: {report["request_count"]} requests',flush=True)
 dump(args.output/'report.json',{'version':'2.1.287','binary_sha256':SHA,'scope':'Actual --bare CLI with isolated fake credentials and loopback-only scripted API; not live-provider or all native session acceptance.','scenarios':reports})
if __name__=='__main__':main()
