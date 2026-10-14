#!/usr/bin/env node
// Reviewable local-provider harness for a pinned Native Claude Code 2.1.289
// installed-Mod smoke. It does not invoke Native unless --execute is present.
//
// Usage:
//   node scripts/tests/mods_native_acceptance_289_loopback.mjs
//   node scripts/tests/mods_native_acceptance_289_loopback.mjs --execute <binary>

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {spawn} from 'node:child_process';
import {createServer} from 'node:http';
import {mkdir, mkdtemp, readFile, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';

const expectedBinarySha = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
const dummyApiKey = 'sk-ant-local-acceptance-only';
const responseText = 'NATIVE_289_LOOPBACK_RESPONSE';
const markerName = 'mods-native-acceptance.marker';
const args = process.argv.slice(2);
const executeIndex = args.indexOf('--execute');

if (executeIndex === -1) {
  console.log(JSON.stringify({
    mode: 'review-only',
    native_invoked: false,
    binary_sha256_required: expectedBinarySha,
    invocation: 'node scripts/tests/mods_native_acceptance_289_loopback.mjs --execute <binary>',
  }, null, 2));
  process.exit(0);
}

assert.equal(args.length, 2, 'expected only --execute <binary>');
const binaryPath = path.resolve(args[executeIndex + 1]);
const binary = await readFile(binaryPath);
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(hash(binary), expectedBinarySha, 'unexpected Native 2.1.289 binary');

const root = await mkdtemp(path.join(tmpdir(), 'mods-native-acceptance-289-'));
const pluginDir = path.join(root, 'plugin');
const sessionDir = path.join(root, 'session');
const configDir = path.join(root, 'config');
const tempDir = path.join(root, 'tmp');
for (const dir of [
  path.join(pluginDir, '.claude-plugin'),
  path.join(pluginDir, 'hooks'),
  sessionDir,
  configDir,
  tempDir,
]) await mkdir(dir, {recursive: true});

const pluginFiles = {
  '.claude-plugin/plugin.json': JSON.stringify({
    name: 'mods-native-local-acceptance',
    version: '1.0.0',
    description: 'Temporary fixture for one local-only Native Mods request',
    author: {name: 'Codex isolated fixture'},
  }) + '\n',
  'hooks/hooks.json': JSON.stringify({modules: ['./register.js']}) + '\n',
  'hooks/register.js': [
    'export function register(on) {',
    "  on('session.start', async ($, event, next) => {",
    `    await $.fs.write('${markerName}', 'session-start-hook-fired\\n');`,
    '    return next(event);',
    '  });',
    '}',
    '',
  ].join('\n'),
};
const fixtureHashes = {};
for (const [relative, contents] of Object.entries(pluginFiles)) {
  const filePath = path.join(pluginDir, relative);
  await mkdir(path.dirname(filePath), {recursive: true});
  await writeFile(filePath, contents);
  fixtureHashes[relative] = hash(Buffer.from(contents));
}

const requests = [];
const healthChecks = [];
let unexpectedRequest = null;
const server = createServer(async (request, response) => {
  if (request.socket.remoteAddress !== '127.0.0.1') {
    unexpectedRequest = {kind: 'non-loopback-peer', remoteAddress: request.socket.remoteAddress};
    response.writeHead(403).end();
    return;
  }
  if (request.method === 'HEAD' && request.url === '/api/hello') {
    healthChecks.push({method: request.method, path: request.url, peer: 'loopback'});
    response.writeHead(200).end();
    return;
  }
  if (request.method !== 'POST' || request.url !== '/v1/messages?beta=true') {
    unexpectedRequest = {kind: 'unexpected-route', method: request.method, path: request.url};
    response.writeHead(404).end();
    return;
  }
  const authMatches = request.headers['x-api-key'] === dummyApiKey;
  if (!authMatches) {
    unexpectedRequest = {kind: 'unexpected-auth'};
    response.writeHead(401).end();
    return;
  }
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  let body;
  try {
    body = JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch {
    unexpectedRequest = {kind: 'invalid-json-body'};
    response.writeHead(400).end();
    return;
  }
  requests.push({
    method: request.method,
    path: request.url,
    auth_matches_dummy: authMatches,
    model: typeof body.model === 'string' ? body.model : null,
    stream: body.stream === true,
    system_chars: typeof body.system === 'string' ? body.system.length : null,
    message_count: Array.isArray(body.messages) ? body.messages.length : null,
  });
  if (requests.length > 1) {
    unexpectedRequest = {kind: 'request-limit-exceeded'};
    response.writeHead(429).end();
    return;
  }

  const model = typeof body.model === 'string' ? body.model : 'claude-sonnet-5';
  if (body.stream === true) {
    response.writeHead(200, {
      'content-type': 'text/event-stream',
      'cache-control': 'no-cache',
      connection: 'keep-alive',
    });
    const events = [
      ['message_start', {type: 'message_start', message: {
        id: 'msg_native_local_acceptance', type: 'message', role: 'assistant', model,
        content: [], stop_reason: null, stop_sequence: null,
        usage: {input_tokens: 1, output_tokens: 0},
      }}],
      ['content_block_start', {type: 'content_block_start', index: 0,
        content_block: {type: 'text', text: ''}}],
      ['content_block_delta', {type: 'content_block_delta', index: 0,
        delta: {type: 'text_delta', text: responseText}}],
      ['content_block_stop', {type: 'content_block_stop', index: 0}],
      ['message_delta', {type: 'message_delta', delta: {stop_reason: 'end_turn', stop_sequence: null},
        usage: {output_tokens: 2}}],
      ['message_stop', {type: 'message_stop'}],
    ];
    for (const [event, data] of events) response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
    response.end();
    return;
  }

  response.writeHead(200, {'content-type': 'application/json'});
  response.end(JSON.stringify({
    id: 'msg_native_local_acceptance', type: 'message', role: 'assistant', model,
    content: [{type: 'text', text: responseText}], stop_reason: 'end_turn',
    stop_sequence: null, usage: {input_tokens: 1, output_tokens: 2},
  }));
});

await new Promise((resolve, reject) => {
  server.once('error', reject);
  server.listen(0, '127.0.0.1', resolve);
});
const address = server.address();
assert(address && typeof address === 'object');
const baseUrl = `http://127.0.0.1:${address.port}`;
const commandArgs = [
  '--print',
  '--verbose',
  '--output-format', 'stream-json',
  '--include-hook-events',
  '--no-session-persistence',
  '--plugin-dir', pluginDir,
  '--model', 'sonnet',
  '--permission-mode', 'dontAsk',
  '--permission-prompts', 'none',
  '--max-budget-usd', '0.01',
  'Reply with the exact text NATIVE_289_LOOPBACK_RESPONSE.',
];
const env = {
  PATH: '/usr/bin:/bin',
  TMPDIR: tempDir,
  LANG: 'C.UTF-8',
  CLAUDE_CONFIG_DIR: configDir,
  ANTHROPIC_API_KEY: dummyApiKey,
  ANTHROPIC_BASE_URL: baseUrl,
  DISABLE_TELEMETRY: '1',
  DO_NOT_TRACK: '1',
  CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1',
};
const stdout = [];
const stderr = [];
const child = spawn(binaryPath, commandArgs, {
  cwd: sessionDir,
  env,
  stdio: ['ignore', 'pipe', 'pipe'],
  detached: true,
});
child.stdout.on('data', chunk => stdout.push(chunk));
child.stderr.on('data', chunk => stderr.push(chunk));
let timedOut = false;
let timeoutKill = null;
let remainingProcessGroupKilled = false;
const signalProcessGroup = signal => {
  if (typeof child.pid !== 'number') return;
  try { process.kill(-child.pid, signal); }
  catch { child.kill(signal); }
};
const timeout = setTimeout(() => {
  timedOut = true;
  signalProcessGroup('SIGTERM');
  timeoutKill = setTimeout(() => signalProcessGroup('SIGKILL'), 3_000);
}, 60_000);
let exit;
try {
  exit = await new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', (code, signal) => resolve({code, signal}));
  });
} finally {
  clearTimeout(timeout);
  if (timeoutKill !== null) clearTimeout(timeoutKill);
  if (child.exitCode === null && child.signalCode === null) signalProcessGroup('SIGKILL');
  if (typeof child.pid === 'number') {
    try {
      process.kill(-child.pid, 0);
      process.kill(-child.pid, 'SIGKILL');
      remainingProcessGroupKilled = true;
    } catch (error) {
      if (error.code !== 'ESRCH') throw error;
    }
  }
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
}

const stdoutBytes = Buffer.concat(stdout);
const stderrBytes = Buffer.concat(stderr);
await writeFile(path.join(root, 'stdout.bin'), stdoutBytes);
await writeFile(path.join(root, 'stderr.bin'), stderrBytes);
const markerPath = path.join(sessionDir, markerName);
let marker = null;
try { marker = await readFile(markerPath, 'utf8'); } catch {}
const receipt = {
  native_invoked: true,
  binary_path: binaryPath,
  binary_sha256: hash(binary),
  cwd: sessionDir,
  config_dir: configDir,
  environment: {
    mode: 'constructed env; inherited environment is not forwarded; HOME and provider credentials are absent',
    anthropic_api_key: 'fixed dummy value; validated by loopback server and never serialized',
    anthropic_base_url: baseUrl,
    telemetry_flags: {
      DISABLE_TELEMETRY: '1',
      DO_NOT_TRACK: '1',
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1',
    },
  },
  argv: commandArgs,
  fixture_hashes: fixtureHashes,
  local_provider: {
    bind: '127.0.0.1 only',
    endpoints: ['HEAD /api/hello (source-pinned preconnect)', 'POST /v1/messages?beta=true'],
    request_count: requests.length,
    requests,
    health_checks: healthChecks,
    unexpected_request: unexpectedRequest,
    relayed_externally: false,
  },
  hook_marker: marker,
  exit,
  timed_out: timedOut,
  remaining_process_group_killed: remainingProcessGroupKilled,
  stdout: {path: path.join(root, 'stdout.bin'), bytes: stdoutBytes.length, sha256: hash(stdoutBytes)},
  stderr: {path: path.join(root, 'stderr.bin'), bytes: stderrBytes.length, sha256: hash(stderrBytes)},
  output_contains_stub_text: stdoutBytes.toString('utf8').includes(responseText),
  acceptance: {
    passed: !timedOut && exit.code === 0 && unexpectedRequest === null &&
      healthChecks.length === 1 && requests.length === 1 &&
      marker === 'session-start-hook-fired\n' && stdoutBytes.toString('utf8').includes(responseText),
    checks: ['exit code zero before timeout', 'one source-pinned loopback HEAD health check',
      'one loopback POST with exact dummy auth', 'no unexpected route/auth/peer',
      'session.start wrote the isolated marker', 'print output contains the fixed local reply'],
  },
};
await writeFile(path.join(root, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
console.log(JSON.stringify({root, receipt: path.join(root, 'receipt.json'), acceptance: receipt.acceptance}, null, 2));
process.exitCode = receipt.acceptance.passed ? 0 : 1;
