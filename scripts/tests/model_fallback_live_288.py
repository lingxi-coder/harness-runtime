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
root = Path(tempfile.mkdtemp(prefix='harness-288-model-fallback-'))
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
        model_rows=[r for r in rows if r['body']['model']==model]
        if len(model_rows)==1 and sys.argv[2]=='budget-reset':
            self.send_response(429 if model=='claude-opus-4-7' else 500);self.send_header('Content-Type','application/json');self.end_headers()
            self.wfile.write(json.dumps({'type':'error','error':{'type':'rate_limit_error' if model=='claude-opus-4-7' else 'api_error','message':'fixture HTTP retry'}}).encode());return
        self.send_response(200);self.send_header('Content-Type','text/event-stream');self.end_headers()
        frames=[{'type':'message_start','message':{'id':'msg_fixture','type':'message','role':'assistant','model':model,'content':[],'usage':{'input_tokens':1,'output_tokens':1}}}]
        if model=='claude-opus-4-7' and not (sys.argv[2]=='same-model' and len(model_rows)>1):
            frames += [
                {'type':'content_block_start','index':0,'content_block':{'type':'thinking','thinking':'','signature':''}},
                {'type':'content_block_delta','index':0,'delta':{'type':'thinking_delta','thinking':'abandoned reasoning'}},
                {'type':'content_block_delta','index':0,'delta':{'type':'signature_delta','signature':'fixture-signature'}},
                {'type':'content_block_stop','index':0},
                {'type':'error','error':{'type':sys.argv[2] if sys.argv[2] not in ('budget-reset','same-model') else 'api_error','message':'fixture stream error'}},
            ]
        else:
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
env.update(CLAUDE_CODE_MAX_RETRIES=sys.argv[3] if len(sys.argv)>3 else ('1' if sys.argv[2]=='budget-reset' else '0'), CLAUDE_CODE_RETRY_WATCHDOG='0')
command = [str(binary), '--bare', '-p', 'timeout probe', '--model', 'claude-opus-4-7', '--fallback-model', 'claude-opus-4-7' if sys.argv[2]=='same-model' else 'claude-sonnet-4-6',
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
    raise AssertionError('native timeout recovery exceeded the bounded probe')
finally:
    server.shutdown()
    server.server_close()
(root / 'stdout.jsonl').write_bytes(stdout)
(root / 'stderr.txt').write_bytes(stderr)
phases = [row['body'].get('stream') is True for row in rows]
print(json.dumps({'exit_code': process.returncode, 'phases': phases, 'output': str(root)}), flush=True)
same_without_retry=sys.argv[2]=='same-model' and (len(sys.argv)<4 or sys.argv[3]=='0')
assert process.returncode == (1 if same_without_retry else 0), process.returncode
models=[r['body']['model'] for r in rows]
expected=(['claude-opus-4-7']*(1 if same_without_retry else 2)) if sys.argv[2]=='same-model' else ['claude-opus-4-7']*(2 if sys.argv[2]=='budget-reset' else 1)+['claude-sonnet-4-6']*(2 if sys.argv[2]=='budget-reset' else 1)
assert models==expected, models
assert all(phases), phases
assert all(row['body']['messages']==rows[0]['body']['messages'] for row in rows), 'model hops preserve the original input snapshot'
assert all('abandoned reasoning' not in json.dumps(r['body']['messages']) for r in rows if r['body']['model']=='claude-sonnet-4-6')
events=[json.loads(line) for line in stdout.splitlines() if line.startswith(b'{')]
notice=[e for e in events if e.get('type')=='system' and e.get('subtype')=='model_fallback']
transcripts=[]
for file in (root/'config').rglob('*.jsonl'):
    for line in file.read_text().splitlines():
        try: record=json.loads(line)
        except json.JSONDecodeError: continue
        if record.get('subtype')=='model_fallback':transcripts.append({'path':str(file),'record':record})
if sys.argv[2]=='same-model':
    assert not notice and not transcripts, (notice,transcripts)
else:
    expected_content='Switched to Sonnet 4.6 due to high demand for Opus 4.7 · context window 1M → 200K tokens'
    assert len(notice)==len(transcripts)==1, (notice,transcripts)
    assert list(notice[0])==['type','subtype','uuid','trigger','original_model','fallback_model','content','session_id']
    assert notice[0]['content']==expected_content and notice[0]['trigger']=='server_error'
    record=transcripts[0]['record']
    assert list(record)==['parentUuid','isSidechain','type','subtype','content','level','trigger','originalModel','fallbackModel','isMeta','uuid','timestamp','userType','entrypoint','cwd','sessionId','version','gitBranch'], list(record)
    assert record['content']==expected_content and record['level']=='warning' and record['isMeta'] is False
    assert record['originalModel']==notice[0]['original_model'] and record['fallbackModel']==notice[0]['fallback_model']
    assert record['uuid']==notice[0]['uuid'] and record['sessionId']==notice[0]['session_id']
report={'native_sha256':native_sha,'passed':True,'scenario':sys.argv[2],'max_retries':sys.argv[3] if len(sys.argv)>3 else ('1' if sys.argv[2]=='budget-reset' else '0'),'exit_code':process.returncode,'models':models,'phases':phases,'notices':notice,'transcripts':transcripts,'output':str(root),'scope':'Actual native --bare configured thinking-only server/timeout fallback and fresh model HTTP retry budget, fake credentials and loopback-only proxy. Full cloud/admission/cost acceptance excluded.'}
(root/'report.json').write_text(json.dumps(report,indent=2)+'\n')
print(json.dumps(report))
