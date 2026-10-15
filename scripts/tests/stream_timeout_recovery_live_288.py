#!/usr/bin/env python3
"""Exercise native long-server stream recovery against an isolated loopback API."""
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
root = Path(tempfile.mkdtemp(prefix='harness-288-stream-timeout-'))
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
        if body.get('stream') is not True:
            # Native AWo's timer expires before this header wait completes.
            time.sleep(1)
            self.close_connection = True
            return
        try:
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            start = {'type': 'message_start', 'message': {'id': 'msg_fixture', 'type': 'message',
                     'role': 'assistant', 'model': 'claude-sonnet-4-6', 'content': [],
                     'stop_reason': None, 'stop_sequence': None,
                     'usage': {'input_tokens': 1, 'output_tokens': 0}}}
            self.wfile.write(('event: message_start\ndata: ' + json.dumps(start) + '\n\n').encode())
            self.wfile.flush()
            time.sleep(0.3)
            error = {'type': 'error', 'error': {'type': 'api_error', 'message': 'fixture long stream'}}
            self.wfile.write(('event: error\ndata: ' + json.dumps(error) + '\n\n').encode())
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        self.close_connection = True


server = ThreadingHTTPServer(('127.0.0.1', 0), Mock)
threading.Thread(target=server.serve_forever, daemon=True).start()
url = f'http://127.0.0.1:{server.server_port}'
env = isolated_env(root, url)
env.update(API_TIMEOUT_MS='200', CLAUDE_CODE_MAX_RETRIES='10', CLAUDE_CODE_RETRY_WATCHDOG='1')
command = [str(binary), '--bare', '-p', 'timeout probe', '--model', 'claude-sonnet-4-6',
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
assert process.returncode == 1, process.returncode
assert phases == [True, True, True, False, False, False], phases
assert all(row['body']['stream'] is False for row in rows[3:])
assert 'timeout' in stdout.decode(errors='replace').lower()
report = {'native_sha256': native_sha, 'passed': True, 'phases': phases, 'output': str(root),
          'scope': 'Actual native --bare CLI with fake credentials, three long started/server-error streams followed by exactly three nonstream header timeouts. Isolated loopback only; signed/live providers and complete transcript/cost parity excluded.'}
(root / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report))
