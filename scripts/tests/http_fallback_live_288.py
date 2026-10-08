#!/usr/bin/env python3
"""Exercise native configured model fallback against an isolated loopback API."""
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from compact_live_oracle import isolated_env

binary = Path(sys.argv[1])
native_sha = hashlib.sha256(binary.read_bytes()).hexdigest()
assert native_sha == 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750'
root = Path(tempfile.mkdtemp(prefix='harness-288-http-fallback-'))
for name in ['config', 'work', 'tmp']:
    (root / name).mkdir()
rows = []
lock = threading.Lock()


class Mock(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reject(self):
        self.send_error(403, 'Only the loopback oracle is available')

    do_CONNECT = reject
    do_GET = reject

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get('Content-Length', '0')))
        body = json.loads(raw)
        assert self.path.split('?')[0] == '/v1/messages', self.path
        with lock:
            rows.append({'path': self.path, 'body': body, 'raw': raw.decode(),
                         'headers': list(self.headers.items()), 'received_at': time.monotonic()})
            (root / 'requests.json').write_text(json.dumps(rows, indent=2))
        model=body['model']
        if model=='claude-opus-4-7' and (not (body_first or disconnect_first) or len(rows)>1):
            status=int(sys.argv[2]);kind='overloaded_error' if payload_overload or status==529 and not api_payload else 'api_error'
            self.send_response(status);self.send_header('Content-Type','application/json')
            if len(sys.argv)>4 and sys.argv[4]=='declined':self.send_header('x-should-retry','false')
            self.end_headers();self.wfile.write(json.dumps({'type':'error','error':{'type':kind,'message':'fixture HTTP failure'}}).encode());return
        if model=='claude-opus-4-7' and (body_first or disconnect_first):
            frames=[{'type':'message_start','message':{'id':'msg_failed','type':'message','role':'assistant','model':model,'content':[],'usage':{'input_tokens':1,'output_tokens':0}}}]
            if body_first:
                frames += [
                    {'type':'content_block_start','index':0,'content_block':{'type':'thinking','thinking':'','signature':''}},
                    {'type':'content_block_delta','index':0,'delta':{'type':'thinking_delta','thinking':'abandoned reasoning'}},
                    {'type':'content_block_delta','index':0,'delta':{'type':'signature_delta','signature':'fixture-signature'}},
                    {'type':'content_block_stop','index':0},
                    {'type':'error','error':{'type':'overloaded_error','message':'fixture stream overloaded'}},
                ]
            encoded=''.join('event: '+f['type']+'\ndata: '+json.dumps(f)+'\n\n' for f in frames).encode()
            self.send_response(200);self.send_header('Content-Type','text/event-stream')
            if disconnect_first:self.send_header('Content-Length',str(len(encoded)+100))
            self.end_headers();self.wfile.write(encoded);self.wfile.flush();self.close_connection=True;return
        self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
        frames=[{'type':'message_start','message':{'id':'msg_fixture','type':'message','role':'assistant','model':model,'content':[],'usage':{'input_tokens':1,'output_tokens':1}}}]
        frames += [
            {'type':'content_block_start','index':0,'content_block':{'type':'text','text':''}},
            {'type':'content_block_delta','index':0,'delta':{'type':'text_delta','text':'fallback success'}},
            {'type':'content_block_stop','index':0},
            {'type':'message_delta','delta':{'stop_reason':'end_turn','stop_sequence':None},'usage':{'output_tokens':3}},
            {'type':'message_stop'},
        ]
        for frame in frames:
            self.wfile.write(('event: '+frame['type']+'\ndata: '+json.dumps(frame)+'\n\n').encode())
        self.wfile.flush();self.close_connection=True



server = ThreadingHTTPServer(('127.0.0.1', 0), Mock)
threading.Thread(target=server.serve_forever, daemon=True).start()
url = f'http://127.0.0.1:{server.server_port}'
env = isolated_env(root, url)
status=int(sys.argv[2]);persistent=sys.argv[3]=='1';declined=len(sys.argv)>4 and sys.argv[4]=='declined'
payload_overload='overload-payload' in sys.argv[4:];api_payload='api-payload' in sys.argv[4:];body_exhausted='body-exhausted' in sys.argv[4:]
body_first='body-first' in sys.argv[4:] or body_exhausted;disconnect_first='disconnect-first' in sys.argv[4:]
env.update(CLAUDE_CODE_MAX_RETRIES='0' if body_exhausted else '2' if status==529 or payload_overload else '1', CLAUDE_CODE_RETRY_WATCHDOG=sys.argv[3])
command = [str(binary), '--bare', '-p', 'HTTP fallback probe', '--model', 'claude-opus-4-7', '--fallback-model', 'claude-sonnet-4-6',
           '--system-prompt', 'fixture', '--output-format', 'stream-json', '--verbose',
           '--setting-sources', '', '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}',
           '--tools', '', '--debug-file', str(root / 'debug.log')]
process = subprocess.Popen(command, cwd=root / 'work', env=env, stdout=subprocess.PIPE,
                           stderr=subprocess.PIPE, start_new_session=True)
try:
    stdout, stderr = process.communicate(timeout=40)
except subprocess.TimeoutExpired:
    os.killpg(process.pid, signal.SIGTERM)
    try:
        stdout, stderr = process.communicate(timeout=3)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        stdout, stderr = process.communicate(timeout=3)
    raise AssertionError('native HTTP fallback exceeded the bounded probe')
finally:
    server.shutdown()
    server.server_close()
(root / 'stdout.jsonl').write_bytes(stdout)
(root / 'stderr.txt').write_bytes(stderr)
phases = [row['body'].get('stream') is True for row in rows]
print(json.dumps({'exit_code': process.returncode, 'phases': phases, 'output': str(root)}), flush=True)
overload_gate=status==529 or payload_overload and (persistent or not 500<=status<600)
expected_hop=not body_exhausted and (overload_gate or not persistent)
expected_primary=2 if body_exhausted else 5 if status==400 and payload_overload else (3 if overload_gate else (2 if disconnect_first else (1 if not persistent or declined else 2)))
assert process.returncode==(0 if expected_hop else 1),process.returncode
models=[r['body']['model'] for r in rows]
assert models==['claude-opus-4-7']*expected_primary+(['claude-sonnet-4-6'] if expected_hop else []),models
assert phases==[True,False] if body_exhausted else all(phases)
if status==400 and payload_overload:
    assert rows[0]['body']['thinking']['display']=='updates'
    assert 'display' not in rows[1]['body']['thinking']
    betas=[dict((k.lower(),v) for k,v in row['headers']).get('anthropic-beta','') for row in rows]
    assert 'dangerous-tool-use-2026-09-03' in betas[1] and 'dangerous-tool-use-2026-09-03' not in betas[2]
assert all(row['body']['messages']==rows[0]['body']['messages'] for row in rows)
assert all('abandoned reasoning' not in json.dumps(row['body']['messages']) for row in rows)
events=[json.loads(line) for line in stdout.splitlines() if line.startswith(b'{')]
notice=[e for e in events if e.get('type')=='system' and e.get('subtype')=='model_fallback']
transcripts=[]
for file in (root/'config').rglob('*.jsonl'):
    for line in file.read_text().splitlines():
        try: record=json.loads(line)
        except json.JSONDecodeError: continue
        if record.get('subtype')=='model_fallback':transcripts.append({'path':str(file),'record':record})
if expected_hop:
    assert len(notice)==len(transcripts)==1,(notice,transcripts)
    assert list(notice[0])==['type','subtype','uuid','trigger','original_model','fallback_model','content','session_id']
    assert notice[0]['content']=='Switched to Sonnet 4.6 due to high demand for Opus 4.7 · context window 1M → 200K tokens'
    assert list(transcripts[0]['record'])[:12]==['parentUuid','isSidechain','type','subtype','content','level','trigger','originalModel','fallbackModel','isMeta','uuid','timestamp']
    assert transcripts[0]['record']['level']=='warning' and transcripts[0]['record']['isMeta'] is False
    assert notice[0]['trigger']==('overloaded' if overload_gate else 'server_error')
    assert notice[0]['original_model']=='claude-opus-4-7' and notice[0]['fallback_model']=='claude-sonnet-4-6'
    assert transcripts[0]['record']['uuid']==notice[0]['uuid']
    assert transcripts[0]['record']['content']==notice[0]['content']
else:
    assert not notice and not transcripts,(notice,transcripts)
report={'native_sha256':native_sha,'passed':True,'status':status,'persistent':persistent,'declined':declined,'payload_overload':payload_overload,'api_payload':api_payload,'body_exhausted':body_exhausted,'native_400_beta_repairs':2 if status==400 and payload_overload else 0,'body_first':body_first,'disconnect_first':disconnect_first,'exit_code':process.returncode,'models':models,'phases':phases,'notices':notice,'transcripts':transcripts,'output':str(root),'scope':'Actual native --bare configured HTTP server/overload fallback with raw status/watchdog/decline inputs, fake credentials and loopback-only proxy. Signed/live-provider and complete output host acceptance excluded.'}
(root/'report.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps(report))
