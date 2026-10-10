import test from 'node:test';
import assert from 'node:assert/strict';
import {mkdir, mkdtemp, readFile, rm, symlink, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {captureProcess, environmentFor, sessionIdFor, snapshotSessions} from './capture.mjs';
import {fixtures, lifecycleProbes, validateFixtures} from './matrix.mjs';
import {providerBody, selectResponse, dummyApiKey} from './loopback-provider.mjs';

test('declarative matrix rejects duplicate ids and arbitrary shell commands', () => {
  assert.doesNotThrow(() => validateFixtures());
  assert.doesNotThrow(() => validateFixtures(lifecycleProbes));
  assert.throws(() => validateFixtures([fixtures[0], fixtures[0]]), /duplicate/);
  assert.throws(() => validateFixtures([{...fixtures[0], responses: [{tool: {name: 'Bash', input: {command: 'curl https://example.com'}}}]}]), /Uncontrolled/);
  assert.equal(sessionIdFor('same'), sessionIdFor('same'));
  assert.match(sessionIdFor('same'), /^[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-8[a-f0-9]{3}-[a-f0-9]{12}$/);
});

test('burst recorder writes both external UUID frames together without merging their contents', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'headless-burst-recorder-test-'));
  try {
    const locations = {workspace: path.join(root, 'workspace'), state: path.join(root, 'state'), tmp: path.join(root, 'tmp')};
    for (const dir of Object.values(locations)) await mkdir(dir);
    const fixture = lifecycleProbes.find(row => row.id === 'probe-input-burst');
    const captureDir = path.join(root, 'capture');
    const observation = await captureProcess({engine: {command: process.execPath, args: [fileURLToPath(new URL('./capture-host.mjs', import.meta.url))]}, fixture, locations, captureDir, provider: {baseUrl: 'http://127.0.0.1:1'}, providerState: {}, timeoutMs: 5000});
    assert.equal(observation.resultCount, 2);
    const input = (await readFile(path.join(captureDir, 'stdin.bin'), 'utf8')).trim().split('\n').map(JSON.parse);
    assert.deepEqual(input.filter(row => row.type === 'user').map(row => [row.uuid, row.message.content]), fixture.turns.map((content, index) => [fixture.userUuids[index], content]));
    const events = (await readFile(path.join(captureDir, 'events.jsonl'), 'utf8')).trim().split('\n').map(JSON.parse);
    assert.equal(events.filter(row => row.kind === 'stdin-write').length, 2); // initialize + one burst write
    assert.equal(events.filter(row => row.kind === 'burst-submitted').length, 1);
  } finally { await rm(root, {recursive: true, force: true}); }
});

test('mixed plain stdin is captured unchanged beside the unchanged positional prompt', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'headless-plain-recorder-test-'));
  try {
    const locations = {workspace: path.join(root, 'workspace'), state: path.join(root, 'state'), tmp: path.join(root, 'tmp')};
    for (const dir of Object.values(locations)) await mkdir(dir);
    const fixture = {...lifecycleProbes.find(row => row.id === 'probe-parser-mixed-stdin'), format: 'text'};
    const captureDir = path.join(root, 'capture');
    const code = "process.stdin.on('data', b => process.stdout.write(b));";
    const observation = await captureProcess({engine: {command: process.execPath, args: ['-e', code, '--']}, fixture, locations, captureDir, provider: {baseUrl: 'http://127.0.0.1:1'}, providerState: {}, timeoutMs: 5000});
    assert.equal(observation.exit.code, 0);
    assert.equal(observation.argv.at(-1), fixture.turns[0]);
    assert.equal(await readFile(path.join(captureDir, 'stdin.bin'), 'utf8'), fixture.stdinText);
    assert.equal(await readFile(path.join(captureDir, 'stdout.bin'), 'utf8'), fixture.stdinText);
  } finally { await rm(root, {recursive: true, force: true}); }
});

test('constructed environment excludes inherited credentials and remote overrides', () => {
  const locations = {state: '/owned/state', workspace: '/owned/workspace', tmp: '/owned/tmp'};
  const provider = {baseUrl: 'http://127.0.0.1:1234'};
  const env = environmentFor({env: {LINGXI_DATA_DIR: '{{state}}', OPENAI_BASE_URL: '{{provider}}'}}, locations, provider);
  assert.equal(env.HOME, path.join(locations.state, 'isolated-home'));
  assert.equal(env.ANTHROPIC_API_KEY, dummyApiKey);
  assert.equal(env.LINGXI_DATA_DIR, locations.state);
  for (const extra of [{ANTHROPIC_API_KEY: 'real'}, {HOME: '/user'}, {ANTHROPIC_BASE_URL: 'http://remote'}, {OPENAI_BASE_URL: 'http://remote'}, {HTTPS_PROXY: 'http://remote'}]) assert.throws(() => environmentFor({env: extra}, locations, provider));
});

test('SSE fixture preserves deterministic message/tool framing and unicode', () => {
  const text = providerBody({model: 'local', stream: true}, {text: '😀 Ω'}, 1).bytes.toString();
  assert.ok(text.startsWith('event: message_start\n'));
  assert.ok(text.endsWith('event: message_stop\ndata: {"type":"message_stop"}\n\n'));
  assert.ok(text.includes('😀 Ω'));
  const tool = providerBody({stream: true}, {tool: {name: 'Bash', input: {command: 'printf HEADLESS_TOOL'}}}, 1).bytes.toString();
  assert.ok(tool.includes('input_json_delta'));
  assert.ok(tool.includes('"stop_reason":"tool_use"'));
});

test('recorder observes each result before EOF and keeps raw split UTF8 bytes', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'headless-recorder-test-'));
  try {
    const locations = {workspace: path.join(root, 'workspace'), state: path.join(root, 'state'), tmp: path.join(root, 'tmp')};
    for (const dir of Object.values(locations)) await mkdir(dir);
    const fixture = fixtures.find(row => row.id === 'sdk-multi-turn');
    const captureDir = path.join(root, 'capture');
    const observation = await captureProcess({engine: {command: process.execPath, args: [fileURLToPath(new URL('./capture-host.mjs', import.meta.url))]}, fixture, locations, captureDir, provider: {baseUrl: 'http://127.0.0.1:1'}, providerState: {}, timeoutMs: 5000});
    assert.deepEqual(observation.exit, {code: 0, signal: null});
    assert.equal(observation.resultCount, 2);
    const stdout = await readFile(path.join(captureDir, 'stdout.bin'), 'utf8');
    assert.equal(stdout.includes('\ufffd'), false);
    assert.ok(stdout.includes('😀 Ω'));
    assert.equal(await readFile(path.join(captureDir, 'stderr.bin'), 'utf8'), 'observed-eof-after-2-results\n');
    const events = (await readFile(path.join(captureDir, 'events.jsonl'), 'utf8')).trim().split('\n').map(JSON.parse);
    const resultIndices = events.flatMap((row, index) => row.kind === 'result' ? [index] : []);
    assert.equal(resultIndices.length, 2);
    assert.ok(events.findIndex(row => row.kind === 'stdin-eof') > resultIndices[1]);
    assert.ok(events.findIndex(row => row.kind === 'close') > resultIndices[1]);
    assert.equal((await readFile(path.join(captureDir, 'stdin.bin'), 'utf8')).trim().split('\n').length, 2);
  } finally { await rm(root, {recursive: true, force: true}); }
});

test('session snapshots exclude other sessions and symlinks', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'headless-session-snapshot-test-'));
  try {
    const state = path.join(root, 'state'); await mkdir(state);
    const session = sessionIdFor('snapshot');
    await writeFile(path.join(state, `${session}.jsonl`), '{"type":"user"}\n');
    await writeFile(path.join(state, 'other.jsonl'), 'other\n');
    await symlink(path.join(state, 'other.jsonl'), path.join(state, 'symlink.jsonl'));
    const files = await snapshotSessions(state, path.join(root, 'captured'), session);
    assert.equal(files.length, 1);
    assert.equal(files[0].path, `${session}.jsonl`);
    assert.equal(await readFile(path.join(root, 'captured', `${session}.jsonl`), 'utf8'), '{"type":"user"}\n');
  } finally { await rm(root, {recursive: true, force: true}); }
});


test('parallel parent child routes match request content and fail closed on ambiguity or limits', () => {
  const fixture = {responseRoutes: true, maxProviderRequests: 3, responses: [
    {match: {lastUserTextIncludes: 'parent'}, text: 'parent'},
    {match: {lastUserTextIncludes: 'child'}, text: 'child', delayMs: 200},
    {match: {lastUserHasToolResult: true}, text: 'after'},
  ]};
  const body = content => ({messages: [{role: 'user', content: 'parent'}, {role: 'user', content}]});
  assert.equal(selectResponse(fixture, body('child'), 0).response.text, 'child');
  assert.equal(selectResponse(fixture, body([{type: 'tool_result', content: 'child'}]), 1).response.text, 'after');
  assert.equal(selectResponse(fixture, body('parent child'), 1).response, undefined);
  assert.equal(selectResponse(fixture, body('child'), 3).response, undefined);
});
