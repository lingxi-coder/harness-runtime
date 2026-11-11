#!/usr/bin/env node
// Replay the unchanged native chain writer with explicit host/store dependencies.
// Only inputs, serialized outputs, and byte-range receipts are persisted.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'), 'Unknown option');
assert(args.filter(arg => !arg.startsWith('--')).length <= 1, 'Expected at most one binary path');
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.321');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, 'abc9feeeca8091d51b623291e044cf0528c2f6b74e0c1d6ef671d52c73f7cd3c',
  'Unrecognized Claude Code 2.1.321 binary');
const sourceByteRanges = [];
function take(name, start, end, digest, first, next) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 5000, `${name}: unbounded extraction`);
  assert.equal(sha256(bytes), digest, `${name}: source digest changed`);
  assert(bytes.toString('utf8').startsWith(first), `${name}: start changed`);
  assert.equal(binary.subarray(end, end + Buffer.byteLength(next)).toString('utf8'), next,
    `${name}: end changed`);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}
const chainWriter = take('chain insertion and outer field order', 271306713, 271309177,
  '9635904f6e51c0be252162315aea24b7e4942d62b2ae17cd076481ca6ea4a43b',
  'async insertMessageChain(', 'async insertFileHistorySnapshot(');
const metadata = take('current session metadata', 271263547, 271263661,
  '79fc9fc61e52660e70c485619150e37b21c3e2ca95fee9f8e01a362573393d1f',
  'function tco(', 'function gGt(');
const version = take('source row version selection', 271263661, 271263803,
  '9cc68086b58822f62e7ba1b33f33d767f309a1143bd84180f86907b59d756878',
  'function gGt(', 'function tfs(');
const userType = take('user type and entrypoint', 271263847, 271263936,
  '646f0d0ca8916fa48a066abfc958dc4e301b06e5b594bd63125b48529832d4e8',
  'function nco(', 'async function _Gt(');
const portableOrigin = take('origin sanitization dispatch', 271360141, 271360393,
  'c5e37c588b37990a6cc2160cc04a682aa1402301f511c4be1878fbd0a6b71fe1',
  'function h_a(', 'function E1t(');
const participant = take('chain participant predicate', 271255101, 271255144,
  '75ddd4009c9a2ba8053bb98bede9b5596f66aff746bea816f473aded8b30275f',
  'function lse(', 'function Nft(');
const boundary = take('compact boundary predicate', 269240029, 269240102,
  '84790a488f35ab2cb9a107085b4c227d67a478c1f2b7b7b7cac133247ce36ad5',
  'function Vr(', 'function fLe(');
const boundaryFactory = take('compact boundary message factory', 276298877, 276299153,
  '9b364f8c572ced63d042a92bd7951348fec55384c4a89f86551bc0175586f261',
  'function t6(', 'function O0t(');
const attachmentFactory = take('attachment message factory', 274492546, 274492768,
  '46fd3cd5758dc28db5fc4f4915000cadf5775787bf455d8c8323db7cbe14f1d8',
  'function on(', 'function X3r(');

const sid = '11111111-1111-4111-8111-111111111111';
const timestamp = '2026-11-09T12:00:00.000Z';
const user = (id = 'user-1', content = 'hello') => ({
  type: 'user', message: { role: 'user', content }, uuid: id, timestamp,
});
const assistant = (id = 'assistant-1') => ({
  message: { id: 'msg-1', type: 'message', role: 'assistant', content: [{ type: 'text', text: 'hi' }] },
  requestId: 'request-1', type: 'assistant', uuid: id, timestamp, effort: 'high',
});
const compact = () => vm.runInNewContext(boundaryFactory
  + '; t6("auto", 123, undefined, undefined, 2)', {
  Date: class { toISOString() { return timestamp; } },
  MS: () => 'boundary-1',
});

async function replay(input) {
  const output = [], events = [];
  const context = vm.createContext({
    INPUT: input,
    process: { env: { CLAUDE_CODE_ENTRYPOINT: 'cli' } },
    iGt: '2.1.321',
    G: () => sid,
    T0t: () => undefined,
    Ki: value => value,
    WJt: () => undefined,
    EZ: () => undefined,
    w$: () => input.promptId,
    _f: async () => 'main',
    ehs: () => {},
    p1e: () => new Map([[sid, 'test-plan']]),
    lde: () => undefined,
    l: (name, fields) => events.push({ name, fields }),
    T4: () => input.sessionKind,
    wl: () => '/workspace/project',
    Cae: Symbol('controlled portable-origin marker'),
    // Tool-result sanitization is outside this replay; cases contain no raw
    // toolUseResult. The origin marker above is absent from every input.
    Lzt: () => { throw new Error('Unexpected tool-result sanitizer'); },
    Gjo: () => { throw new Error('Unexpected first-row materialization'); },
    B_r: () => '',
    jK: 50 * 1024 * 1024,
  });
  vm.runInContext(metadata + version + userType + portableOrigin + participant + boundary
    + `\nvar writer = {${chainWriter}};`, context);
  const writer = context.writer;
  Object.assign(writer, {
    sessionFile: '/oracle/session.jsonl',
    awaitingFirstUserRow: false,
    heldReplayRecords: [],
    store: { sessionMessages: async () => new Set(input.knownUuids ?? []) },
    trackWrite: fn => fn(),
    shouldSkipPersistence: () => false,
    appendToolsUsedEntryIfGrown: async () => {},
    appendEntry: async entry => output.push(JSON.stringify(entry)),
    requestCompact: () => {},
    normalizeLastPrompt: value => value,
  });
  const messages = vm.runInContext('JSON.parse(INPUT.messagesJson)', context);
  await writer.insertMessageChain(messages, input.sidechain ?? false, input.agentId,
    input.parentUuid ?? null, input.team);
  return { rows: output, events, leafUuid: writer.currentSessionLeafUuid ?? null };
}

const cases = [];
async function add(name, messages, options = {}) {
  const { fresh = true, ...configuration } = options;
  const input = { messagesJson: JSON.stringify(messages), ...configuration };
  cases.push({ name, fresh, input, expected: await replay(input) });
}
await add('main user', [user()]);
await add('main assistant', [assistant()]);
await add('main user with prompt and team', [user()], {
  promptId: 'prompt-1', agentId: 'agent-1', team: { teamName: 'team', agentName: 'worker' },
});
await add('sidechain assistant with team', [assistant()], {
  sidechain: true, agentId: 'agent-1', team: { teamName: 'team', agentName: 'worker' },
});
await add('session kind trailer', [user()], { sessionKind: 'bg' });
await add('sidechain assistant session kind', [assistant()], {
  sidechain: true, agentId: 'agent-1', sessionKind: 'daemon-worker',
});
await add('compact boundary resets parent', [compact()], { parentUuid: 'old-parent' });
await add('sidechain compact boundary', [compact()], {
  parentUuid: 'old-parent', sidechain: true, agentId: 'agent-1',
});
await add('compact boundary and summary', [compact(), user('summary-1', 'summary')], {
  parentUuid: 'old-parent',
});
await add('attachment with rendered fields', [vm.runInNewContext(attachmentFactory
  + '; on({type:"additional_instructions", content:"instructions"},'
  + '{now:()=>TIME, uuid:()=>"attachment-1"})', {
  TIME: timestamp,
  Gle: () => ({ rendered: 'visible', renderedRole: 'user' }),
})], { agentId: 'agent-1' });
await add('Unicode and JavaScript JSON values', [user('unicode', [
  { type: 'text', text: '中文 😀 \ud800\u2028\u2029' },
  { type: 'tool_result', tool_use_id: 't', content: { n: 9007199254740993, negativeZero: -0 } },
])]);
await add('valid source-tool assistant parent', [{
  ...user('result-1', [{ type: 'tool_result', tool_use_id: 't', content: 'ok' }]),
  sourceToolAssistantUUID: 'tool-assistant',
}], { parentUuid: 'ordinary-parent', knownUuids: ['tool-assistant'] });
await add('phantom source-tool assistant keeps prior parent', [{
  ...user('result-1', [{ type: 'tool_result', tool_use_id: 't', content: 'ok' }]),
  sourceToolAssistantUUID: 'tool-assistant',
}], { parentUuid: 'ordinary-parent' });
await add('progress does not become chain parent', [user(), {
  type: 'progress', uuid: 'progress-1', timestamp, data: { type: 'test' },
}, assistant()], { fresh: false });
await add('source version preserved', [{ ...user(), version: '2.1.319' }], { fresh: false });
await add('source identity without version stays absent', [{ ...user(), sessionId: sid }], {
  fresh: false,
});
await add('source sidechain flag is overwritten', [{ ...user(), isSidechain: true }]);
await add('sidechain agent identity is forced', [{ ...assistant(), agentId: 'source-agent' }], {
  sidechain: true, agentId: 'target-agent',
});

const fixture = {
  schemaVersion: 1,
  claudeCodeVersion: '2.1.321', nativeSha256, sourceByteRanges,
  coverage: 'Unchanged chain writer with controlled store, host metadata, filesystem-independent appends, and ordinary prepared message envelopes. Compact and attachment factories run unchanged with fixed clock/UUID and supplied attachment rendering. No disk, V5 backend, request replay, origin sanitization, or tool-result sanitization is exercised.',
  cases,
};
const fixturePath = new URL('../../crates/session/tests/fixtures/session_rows_2_1_321.json', import.meta.url);
const bytes = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) {
  assert.equal(readFileSync(fixturePath, 'utf8'), bytes, 'Session row fixture differs from native replay');
} else {
  writeFileSync(fixturePath, bytes);
}
console.log(`Claude Code 2.1.321 session rows: ${cases.length} cases; ${sourceByteRanges.length} verified ranges${args.includes('--check') ? ' checked' : ' generated'}`);
