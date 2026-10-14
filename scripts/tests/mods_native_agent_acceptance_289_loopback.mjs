#!/usr/bin/env node
// Bounded local-provider exercise of Native 2.1.289 $.agent.list/spawn.
// No Native process starts unless the caller supplies --execute <binary>.
//
// Usage:
//   node scripts/tests/mods_native_agent_acceptance_289_loopback.mjs
//   node scripts/tests/mods_native_agent_acceptance_289_loopback.mjs --execute <binary>

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {spawn} from 'node:child_process';
import {createServer} from 'node:http';
import {mkdir, mkdtemp, readFile, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';

const expectedBinarySha = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
const dummyApiKey = 'sk-ant-local-agent-acceptance-only';
const childPromptMarker = 'NATIVE_AGENT_CHILD_PROMPT_UNIQUE_289';
const childReply = 'NATIVE_AGENT_CHILD_RESPONSE_289';
const rootPrompt = 'Reply with the exact text NATIVE_AGENT_ROOT_RESPONSE_289.';
const rootReply = 'NATIVE_AGENT_ROOT_RESPONSE_289';
const observationName = 'agent-observations.json';
const maxMessageRequests = 2;
const args = process.argv.slice(2);
const executeIndex = args.indexOf('--execute');

if (executeIndex === -1) {
  console.log(JSON.stringify({
    mode: 'review-only',
    native_invoked: false,
    binary_sha256_required: expectedBinarySha,
    max_message_requests: maxMessageRequests,
    invocation: 'node scripts/tests/mods_native_agent_acceptance_289_loopback.mjs --execute <binary>',
  }, null, 2));
  process.exit(0);
}

assert.equal(args.length, 2, 'expected only --execute <binary>');
const binaryPath = path.resolve(args[executeIndex + 1]);
const binary = await readFile(binaryPath);
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(hash(binary), expectedBinarySha, 'unexpected Native 2.1.289 binary');

const root = await mkdtemp(path.join(tmpdir(), 'mods-native-agent-acceptance-289-'));
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
    name: 'mods-native-agent-local-acceptance',
    version: '1.0.0',
    description: 'Temporary fixture for one Native Agent child and local replies',
    author: {name: 'Codex isolated fixture'},
  }) + '\n',
  'hooks/hooks.json': JSON.stringify({modules: ['./register.js']}) + '\n',
  'hooks/register.js': [
    'let spawnedOnce = false;',
    'export function register(on) {',
    "  on('session.start', async ($, event, next) => {",
    '    if (spawnedOnce) return next(event);',
    '    spawnedOnce = true;',
    '    let before = [], afterLaunch = [], afterWait = [];',
    '    let spawned = null, spawnError = null, waitError = null;',
    '    try { before = await $.agent.list(); } catch (error) { spawnError = String(error); }',
    '    if (spawnError === null) {',
    '      try {',
    '        spawned = await $.agent.spawn({',
    `          prompt: '${childPromptMarker} Return exactly ${childReply}. Do not call tools.',`,
    "          description: 'Native Agent list and spawn local acceptance child',",
    "          subagentType: 'Explore',",
    "          model: 'sonnet',",
    '        });',
    '      } catch (error) { spawnError = String(error); }',
    '    }',
    '    try { afterLaunch = await $.agent.list(); } catch (error) { waitError = String(error); }',
    `    await $.fs.write('${observationName}', JSON.stringify({before, spawned, spawnError, afterLaunch, waitError}));`,
    '    if (spawned && typeof spawned.agentId === "string") {',
    '      try { await $.clock.sleep(3500); } catch (error) { waitError = String(error); }',
    '      try { afterWait = await $.agent.list(); } catch (error) { waitError = String(error); }',
    `      await $.fs.write('${observationName}', JSON.stringify({before, spawned, spawnError, afterLaunch, afterWait, waitError}));`,
    '    }',
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
let childRequestCount = 0;
let rootRequestCount = 0;
const responseSse = (response, model, id, text) => {
  response.writeHead(200, {
    'content-type': 'text/event-stream',
    'cache-control': 'no-cache',
    connection: 'keep-alive',
  });
  const events = [
    ['message_start', {type: 'message_start', message: {
      id, type: 'message', role: 'assistant', model, content: [],
      stop_reason: null, stop_sequence: null,
      usage: {input_tokens: 1, output_tokens: 0},
    }}],
    ['content_block_start', {type: 'content_block_start', index: 0,
      content_block: {type: 'text', text: ''}}],
    ['content_block_delta', {type: 'content_block_delta', index: 0,
      delta: {type: 'text_delta', text}}],
    ['content_block_stop', {type: 'content_block_stop', index: 0}],
    ['message_delta', {type: 'message_delta', delta: {stop_reason: 'end_turn', stop_sequence: null},
      usage: {output_tokens: 2}}],
    ['message_stop', {type: 'message_stop'}],
  ];
  for (const [event, data] of events) response.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
  response.end();
};
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
  if (request.headers['x-api-key'] !== dummyApiKey) {
    unexpectedRequest = {kind: 'unexpected-auth'};
    response.writeHead(401).end();
    return;
  }
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  let body;
  try { body = JSON.parse(Buffer.concat(chunks).toString('utf8')); }
  catch {
    unexpectedRequest = {kind: 'invalid-json-body'};
    response.writeHead(400).end();
    return;
  }
  const childRequest = JSON.stringify(body).includes(childPromptMarker);
  const rootRequest = JSON.stringify(body).includes(rootPrompt);
  if (!childRequest && !rootRequest) {
    unexpectedRequest = {kind: 'unclassified-message-request'};
    response.writeHead(400).end();
    return;
  }
  if (childRequest) childRequestCount++;
  else rootRequestCount++;
  requests.push({
    method: request.method,
    path: request.url,
    auth_matches_dummy: true,
    request_kind: childRequest ? 'agent_child' : 'root_prompt',
    model: typeof body.model === 'string' ? body.model : null,
    stream: body.stream === true,
    message_count: Array.isArray(body.messages) ? body.messages.length : null,
  });
  if (requests.length > maxMessageRequests || childRequestCount > 1 || rootRequestCount > 1) {
    unexpectedRequest = {kind: 'bounded-request-count-exceeded'};
    response.writeHead(429).end();
    return;
  }

  const model = typeof body.model === 'string' ? body.model : 'claude-sonnet-5-5';
  if (childRequest) await new Promise(resolve => setTimeout(resolve, 2_500));
  responseSse(response, model,
    childRequest ? 'msg_native_agent_child' : 'msg_native_agent_root',
    childRequest ? childReply : rootReply);
});

await new Promise((resolve, reject) => {
  server.once('error', reject);
  server.listen(0, '127.0.0.1', resolve);
});
const address = server.address();
assert(address && typeof address === 'object');
const baseUrl = `http://127.0.0.1:${address.port}`;
const commandArgs = [
  '--print', '--verbose', '--output-format', 'stream-json', '--include-hook-events',
  '--no-session-persistence', '--plugin-dir', pluginDir, '--model', 'sonnet',
  '--permission-mode', 'acceptEdits', '--permission-prompts', 'none',
  '--max-budget-usd', '0.01', rootPrompt,
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
let observation = null;
try { observation = JSON.parse(await readFile(path.join(sessionDir, observationName), 'utf8')); }
catch {}
const agentId = observation?.spawned?.agentId;
const rowFor = rows => Array.isArray(rows) ? rows.find(row => row.id === agentId) ?? null : null;
const launchedRow = rowFor(observation?.afterLaunch);
const completedRow = rowFor(observation?.afterWait);
const inFlightStatuses = new Set(['running', 'idle', 'waiting']);
const acceptance = {
  passed: !timedOut && exit.code === 0 && unexpectedRequest === null &&
    healthChecks.length === 1 && requests.length === 2 && childRequestCount === 1 &&
    rootRequestCount === 1 && observation?.spawnError === null &&
    typeof agentId === 'string' && typeof observation?.spawned?.model === 'string' &&
    launchedRow !== null && inFlightStatuses.has(launchedRow.status) &&
    completedRow?.status === 'completed' && stdoutBytes.toString('utf8').includes(rootReply),
  checks: [
    'one source-pinned loopback HEAD health check',
    'exactly one child and one root model request with the dummy auth',
    'Native $.agent.spawn returned a model and agent ID',
    'Native $.agent.list observed that ID in an in-flight status',
    'Native $.agent.list observed the same ID completed after the child reply',
    'root print output contains the fixed local reply',
  ],
};
const receipt = {
  native_invoked: true,
  binary_path: binaryPath,
  binary_sha256: hash(binary),
  cwd: sessionDir,
  config_dir: configDir,
  environment: {
    mode: 'constructed env; inherited environment is not forwarded; HOME and provider credentials are absent',
    anthropic_api_key: 'fixed dummy value; exact-match checked by local stub and never serialized',
    anthropic_base_url: baseUrl,
    telemetry_flags: {
      DISABLE_TELEMETRY: '1',
      DO_NOT_TRACK: '1',
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: '1',
    },
  },
  argv: commandArgs,
  fixture_hashes: fixtureHashes,
  native_agent_observation: observation,
  selected_rows: {agent_id: agentId ?? null, after_launch: launchedRow, after_wait: completedRow},
  local_provider: {
    bind: '127.0.0.1 only',
    endpoints: ['HEAD /api/hello (source-pinned preconnect)', 'POST /v1/messages?beta=true'],
    request_limit: maxMessageRequests,
    request_count: requests.length,
    child_request_count: childRequestCount,
    root_request_count: rootRequestCount,
    requests,
    health_checks: healthChecks,
    unexpected_request: unexpectedRequest,
    relayed_externally: false,
  },
  exit,
  timed_out: timedOut,
  remaining_process_group_killed: remainingProcessGroupKilled,
  stdout: {path: path.join(root, 'stdout.bin'), bytes: stdoutBytes.length, sha256: hash(stdoutBytes)},
  stderr: {path: path.join(root, 'stderr.bin'), bytes: stderrBytes.length, sha256: hash(stderrBytes)},
  root_output_contains_fixed_reply: stdoutBytes.toString('utf8').includes(rootReply),
  acceptance,
};
await writeFile(path.join(root, 'receipt.json'), JSON.stringify(receipt, null, 2) + '\n');
console.log(JSON.stringify({root, receipt: path.join(root, 'receipt.json'), acceptance}, null, 2));
process.exitCode = acceptance.passed ? 0 : 1;
