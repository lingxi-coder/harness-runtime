#!/usr/bin/env python3
"""Check native invalid timeout rejection with isolated fake credentials."""
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from compact_live_oracle import isolated_env

binary = Path(sys.argv[1])
native_sha = hashlib.sha256(binary.read_bytes()).hexdigest()
assert native_sha == 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750'
root = Path(tempfile.mkdtemp(prefix='harness-288-invalid-timeout-'))
rows = []


class Mock(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reject(self):
        self.send_error(403, 'Only the loopback oracle is available')

    do_CONNECT = reject
    do_GET = reject

    def do_POST(self):
        rows.append(self.path)
        self.send_error(400, 'Unexpected model dispatch')


server = ThreadingHTTPServer(('127.0.0.1', 0), Mock)
threading.Thread(target=server.serve_forever, daemon=True).start()
url = f'http://127.0.0.1:{server.server_port}'
results = []
try:
    for index, (value, expected) in enumerate([
        ('-1', 'timeout must be a positive integer'),
        ('-1_000', 'timeout must be a positive integer'),
        ('-1e3', 'timeout must be a positive integer'),
        ('-1' + '0' * 400, 'timeout must be an integer'),
    ]):
        case = root / str(index)
        for name in ['config', 'work', 'tmp']:
            (case / name).mkdir(parents=True)
        env = isolated_env(case, url)
        env.update(API_TIMEOUT_MS=value, CLAUDE_CODE_MAX_RETRIES='0')
        command = [str(binary), '--bare', '-p', 'timeout probe', '--model', 'claude-sonnet-4-6',
                   '--system-prompt', 'fixture', '--output-format', 'stream-json', '--verbose',
                   '--setting-sources', '', '--strict-mcp-config', '--mcp-config', '{"mcpServers":{}}',
                   '--tools', '', '--debug-file', str(case / 'debug.log')]
        process = subprocess.Popen(command, cwd=case / 'work', env=env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
        try:
            stdout, stderr = process.communicate(timeout=25)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=3)
            raise AssertionError('native timeout validation did not terminate')
        (case / 'stdout.jsonl').write_bytes(stdout)
        (case / 'stderr.txt').write_bytes(stderr)
        assert process.returncode == 1, process.returncode
        assert not rows, rows
        assert expected in (stdout + stderr).decode(errors='replace'), (stdout + stderr)[-2000:]
        results.append({'index': index, 'exit_code': process.returncode, 'requests': len(rows),
                        'expected_error': expected, 'passed': True})
        print(f'native invalid timeout scenario {index} passed', flush=True)
finally:
    server.shutdown()
    server.server_close()

report = {'native_sha256': native_sha, 'cases': results, 'output': str(root),
          'scope': 'Actual native --bare CLI invalid timeout rejection, fake credentials and loopback proxy. No live-provider or positive-deadline timing acceptance.'}
(root / 'report.json').write_text(json.dumps(report, indent=2) + '\n')
print(json.dumps(report))
