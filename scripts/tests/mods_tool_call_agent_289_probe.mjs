#!/usr/bin/env node
// Focused Native 2.1.289 Agent $.tool.call() result/settlement probe.
// Runs pinned Native eU/UOt helpers with a controlled W1 frame, registry,
// clock, and cancellation signal; no real Agent or tool is launched.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node scripts/tests/mods_tool_call_agent_289_probe.mjs <2.1.289-binary>');
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const binary = readFileSync(binaryPath);
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256',
);

const chunks = {
  helpers: {
    start: 182637043, end: 182864930, length: 227886,
    sha256: 'b8b83549f7b4273ed3f6b4669d666e535f9abfd831ea3952e9387ac73500e85c',
  },
  primary: {
    start: 187181803, end: 191691521, length: 4509717,
    sha256: '47b62b9ac561f0cc5cd75ab6e7b9e14c1b027cccebfb900b56f07685c69fa825',
  },
};

function chunk(name) {
  const pin = chunks[name];
  const raw = binary.subarray(pin.start, pin.end);
  assert.equal(raw.at(-1), 0, `${name}: expected trailing NUL`);
  const source = raw.subarray(0, -1);
  assert.equal(source.length, pin.length, `${name}: source length changed`);
  assert.equal(sha256(source), pin.sha256, `${name}: source chunk hash changed`);
  assert.deepEqual(binary.subarray(pin.start, pin.end - 1), source,
    `${name}: source mapping no longer matches executable bytes`);
  return source;
}

const helpers = chunk('helpers');
const primary = chunk('primary');
function slice(bytes, start, end, expectedHash, marker) {
  const source = bytes.subarray(start, end);
  assert.equal(source.length, end - start, 'pinned Native source range length changed');
  assert.equal(sha256(source), expectedHash, 'pinned Native source range hash changed');
  if (marker) assert(source.toString().startsWith(marker), `expected source marker ${marker}`);
  return source.toString();
}

const helperSource = {
  n5r: slice(helpers, 97589, 97719,
    '97bf28497dd65200e0a242062325a5432ae5f5f4b5505f4a381a56ad2fc37933', 'var n5r='),
  shadowInput: slice(helpers, 97719, 97821,
    '5bd63444d844fa9131e1eaf820ddf5d57212fcf472794ec2f1a245b89adf8702', 'function _se('),
  resultText: slice(helpers, 97878, 97934,
    '3c1cf0c0fced0ed217d10afd8a44a8d74874a7c95176225e695cb6569ccd2894', 'function r5r('),
  resolvedModel: slice(helpers, 207088, 207174,
    'd5e529c1e38c148dd82169f5b3386ed096ffa039e2d3ad397d32047b36e79566', 'function ptr('),
  millis: slice(helpers, 127617, 127629,
    '696b6e3fd67765a5c34d84341b5660ca48be01b9d0a58ae82e6ce4f1f050cc4d', 'var kK='),
};
const primarySource = {
  lAt: slice(primary, 1326417, 1326483,
    '068f137299474ee14942ab54ff5282227f936d9811a7e3f5186d0b852b5c7601', 'var LAt='),
  fAt: slice(primary, 1326507, 1326553,
    '045f105e8a6e8de36b56189cfb4a4a40534f75ce98a68423376f9fd1777cf51f', 'var FAt='),
  toolUse: slice(primary, 1326591, 1326696,
    '4e933392955334171fd9efc7dd0a1ab917ecebf507c0037e30e6c010318ee6f7', 'var hfo='),
  agentId: slice(primary, 1326696, 1326809,
    'ad60de1ed98d460840f58b1427f6bc00a15e9b526078b73548a8fbd73e1ec948', 'function _fo('),
  hookOrigin: slice(primary, 1326809, 1326890,
    '728dd2ce55ef1c33361b9377a85efb109048e09ca338f7258e2c689e0db4e4ee', 'var $At='),
  agentText: slice(primary, 1326890, 1327043,
    '78e9dd61f073543aea37d66be2c7c85e96afdc91be5b16c04c3f91ad7a513de4', 'function kfo('),
  userResultFrame: slice(primary, 1327043, 1327129,
    'fe7a3d4a251aa07165ee0a7aa300e4ec6aea2efd2d05349a8a24044c217b4858', 'function bfo('),
  cloneResult: slice(primary, 550411, 550449,
    '825dbf7480279df62d806647d028466d4c531bb1f139dae21a93b68f78057aff', 'var Aq='),
  launchWait: slice(primary, 1671300, 1671409,
    'caa26e4acdc771ea59d5ecfdc7ab29d7d447a55ce62bfaa6537b302aa5a7b0ad', 'function BOt('),
  toolCall: slice(primary, 1671409, 1673850,
    'a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66', 'async function eU('),
  transcriptText: slice(primary, 1674251, 1674304,
    '265154c447ced94580ecce813b4cf513cdaf35a4c3bcd728a99d2fbfd9555e13', 'var pae='),
  pollMillis: slice(primary, 1674304, 1674316,
    '8b4c9339444ed89d503e6cfe22ec521f9d5e8edad93767eb921f874f1c06c334', 'var HOt='),
  timeoutMinutes: slice(primary, 1674316, 1674327,
    'd360de8b1c91e3129e5059c5c7cc0c6808ad7210f8156cfbe018e21133a2f3a7', 'var VDe='),
  seconds: slice(primary, 1674327, 1674338,
    'd3171ded13242b6e55e821b8b22464af571e894e8df8079732f62e204d6f16df', 'var jOt='),
  waitBudget: slice(primary, 1674338, 1674357,
    'c720f6ae3a68a1bcfbd200036191f7bf4bfffc514e796f99dfbb5bd05d394aeb', 'var WOt='),
  evictedRecord: slice(primary, 1674357, 1674517,
    '30d7dd20293bd627587fdf411b5344f3bbb9c238824ce3734a4cf1d3a7475511', 'function GOt('),
  settledWait: slice(primary, 1674517, 1674614,
    '7ec1018cc90e22070084fb7237ab2d5f3c6de157baaa037bd093201a22917cae', 'async function zOt('),
  timedOutSettlement: slice(primary, 1674614, 1674716,
    '9d15eaf1720748a63642208cd3a45cb127d205ee7896717d200da89705343912', 'function qOt('),
  agentWait: slice(primary, 1674716, 1675427,
    '1bc56030e87f66744b2c581b49bf60678f36ba8bff3f1e5e40722c0c8fc90bd0', 'async function UOt('),
};
const terminalStatus = binary.subarray(185139913, 185139979);
assert.equal(terminalStatus.toString(),
  'function Rs(e){return e==="completed"||e==="failed"||e==="killed"}');
assert.equal(sha256(terminalStatus),
  '97b7f941c1c09455cb734a557cb33b37fcdd82297be2af30b4d77296dd6ccbcd');

const context = vm.createContext({
  clock: {now: 0, step: 0, onSleep: undefined},
  Date: {now: () => context.clock.now},
  ee: async () => {
    context.clock.now += context.clock.step;
    await Promise.resolve();
    await context.clock.onSleep?.();
  },
  K3: () => {},
  HOt: 150,
  VDe: 10,
  jOt: 60,
  kK: 1000,
  WOt: 10 * 60 * 1000,
  LAt: undefined,
  FAt: undefined,
  hfo: undefined,
  _fo: undefined,
  $At: undefined,
  Aq: undefined,
  n5r: undefined,
  r5r: undefined,
  bfo: undefined,
  ptr: undefined,
  kfo: undefined,
  pae: undefined,
  BOt: undefined,
  UOt: undefined,
  eU: undefined,
  N: value => value !== null && typeof value === 'object',
  Pe: '__shadowed',
  gfo: () => '12345678-1234-1234-1234-123456789012',
  NAt: {type: 'direct'},
  ht: 'Agent',
  Ie: class NativeHookError extends Error {},
  t: () => {},
  BAt: (_plugin, error) => { throw error; },
  _s: () => {},
  Mx: () => false,
  yn: (tools, name) => tools.find(tool => tool.name === name),
  Uoe: () => false,
  av: () => () => {},
  wc: value => value,
  S: value => value,
  J: value => structuredClone(value),
  $At: undefined,
  AbortController,
  setImmediate,
});
vm.runInContext(terminalStatus.toString(), context);
for (const source of [
  helperSource.n5r,
  helperSource.shadowInput,
  helperSource.resultText,
  helperSource.resolvedModel,
  primarySource.lAt,
  primarySource.fAt,
  primarySource.toolUse,
  primarySource.agentId,
  primarySource.hookOrigin,
  primarySource.agentText,
  primarySource.userResultFrame,
  primarySource.cloneResult,
  primarySource.launchWait,
  primarySource.transcriptText,
  primarySource.pollMillis,
  primarySource.timeoutMinutes,
  primarySource.seconds,
  primarySource.waitBudget,
  primarySource.evictedRecord,
  primarySource.settledWait,
  primarySource.timedOutSettlement,
  primarySource.agentWait,
  primarySource.toolCall,
]) {
  vm.runInContext(source, context);
}
context.registryFactory = undefined;

function makeRegistry({rows = [], transcripts = {}, onAll, onSleep} = {}) {
  const byTaskId = new Map(rows.map(row => [row.id, {...row}]));
  const registry = {
    all() {
      onAll?.(byTaskId);
      return Object.fromEntries(byTaskId.entries());
    },
    get(taskId) { return byTaskId.get(taskId); },
    update(taskId, update) {
      const next = update(byTaskId.get(taskId));
      if (next === undefined) byTaskId.delete(taskId);
      else byTaskId.set(taskId, next);
      return next;
    },
    getTranscript(agentId) { return transcripts[agentId]; },
  };
  return {registry, rows: byTaskId, onSleep};
}

function assistantText(...blocks) {
  return {messages: [{type: 'assistant', message: {content: blocks.map(text => ({type: 'text', text}))}}]};
}

function setupClock({now = 0, step = 0, onSleep} = {}) {
  context.clock = {now, step, onSleep};
}

const completedAgent = makeRegistry({
  rows: [{id: 'task-complete', type: 'local_agent', agentId: 'agent-complete', status: 'completed'}],
  transcripts: {'agent-complete': {messages: [
    {type: 'assistant', message: {content: [{type: 'text', text: 'older'}]}},
    {type: 'assistant', message: {content: [
      {type: 'text', text: 'completed'}, {type: 'text', text: ' transcript'},
    ]}},
  ]}},
});
setupClock();
assert.deepEqual(JSON.parse(JSON.stringify(await context.UOt({
  registry: completedAgent.registry, agentId: 'agent-complete', pluginName: 'plug', signal: {aborted: false},
}))), {text: 'completed transcript'});

for (const [status, error, expected] of [
  ['failed', 'native failure', 'plug: $.agent.spawn: the subagent failed: native failure'],
  ['killed', undefined, 'plug: $.agent.spawn: the subagent killed: last output'],
]) {
  const agentId = `agent-${status}`;
  const testRegistry = makeRegistry({
    rows: [{id: `task-${status}`, type: 'local_agent', agentId, status, ...(error !== undefined && {error})}],
    transcripts: {[agentId]: assistantText('last output')},
  });
  setupClock();
  assert.deepEqual(JSON.parse(JSON.stringify(await context.UOt({
    registry: testRegistry.registry, agentId, pluginName: 'plug', signal: {aborted: false},
  }))), {text: expected, isError: true});
}

const terminalStatuses = ['completed', 'failed', 'killed'].map(status => [status, context.Rs(status)]);
assert.deepEqual(terminalStatuses, [['completed', true], ['failed', true], ['killed', true]]);
assert.equal(context.Rs('cancelled'), false, 'Native cancelled spelling is not terminal in Rs');

const cancelledRecord = makeRegistry({
  rows: [{id: 'task-cancelled', type: 'local_agent', agentId: 'agent-cancelled', status: 'cancelled'}],
});
setupClock({step: 150, onSleep: async () => {
  const current = cancelledRecord.rows.get('task-cancelled');
  if (current?.status === 'cancelled') cancelledRecord.rows.set('task-cancelled', {...current, status: 'killed'});
}});
const cancelledWait = await context.UOt({
  registry: cancelledRecord.registry,
  agentId: 'agent-cancelled', pluginName: 'plug', signal: {aborted: true},
});
assert.equal(cancelledWait.text, 'plug: $.agent.spawn aborted');
assert.equal(cancelledWait.isError, true);
assert.equal(typeof cancelledWait.settled?.then, 'function',
  'an already-seen Agent returns a settlement promise after caller abort');
await cancelledWait.settled;

const cancelledWithoutSignal = makeRegistry({
  rows: [{id: 'task-cancelled-no-signal', type: 'local_agent', agentId: 'agent-cancelled-no-signal', status: 'cancelled'}],
});
setupClock({step: 150, onSleep: async () => {
  const current = cancelledWithoutSignal.rows.get('task-cancelled-no-signal');
  if (current?.status === 'cancelled') {
    cancelledWithoutSignal.rows.set('task-cancelled-no-signal', {...current, status: 'killed'});
  }
}});
const cancelledTimeout = await context.UOt({
  registry: cancelledWithoutSignal.registry,
  agentId: 'agent-cancelled-no-signal', pluginName: 'plug',
  signal: {aborted: false}, settleWaitMs: 0,
});
assert.equal(cancelledTimeout.text, 'plug: $.agent.spawn: no answer within 10 minutes');
assert.equal(cancelledTimeout.isError, true,
  'a cancelled status itself is not terminal and follows timeout behavior');
await cancelledTimeout.settled;

const noRecord = makeRegistry();
setupClock({step: 600000});
assert.deepEqual(JSON.parse(JSON.stringify(await context.UOt({
  registry: noRecord.registry, agentId: 'never-seen', pluginName: 'plug', signal: {aborted: false},
}))), {text: 'plug: $.agent.spawn: no answer within 10 minutes', isError: true});

const evicted = makeRegistry({
  rows: [{id: 'task-evicted', type: 'local_agent', agentId: 'agent-evicted', status: 'running'}],
  transcripts: {'agent-evicted': assistantText('answer survived eviction')},
});
setupClock({step: 150, onSleep: async () => { evicted.rows.delete('task-evicted'); }});
assert.deepEqual(JSON.parse(JSON.stringify(await context.UOt({
  registry: evicted.registry, agentId: 'agent-evicted', pluginName: 'plug', signal: {aborted: false},
}))), {text: 'answer survived eviction'});

const evictedWithoutText = makeRegistry({
  rows: [{id: 'task-evicted-empty', type: 'local_agent', agentId: 'agent-evicted-empty', status: 'running'}],
});
setupClock({step: 150, onSleep: async () => { evictedWithoutText.rows.delete('task-evicted-empty'); }});
assert.deepEqual(JSON.parse(JSON.stringify(await context.UOt({
  registry: evictedWithoutText.registry, agentId: 'agent-evicted-empty', pluginName: 'plug', signal: {aborted: false},
}))), {
  text: "plug: $.agent.spawn: the subagent's record was evicted before its answer was read",
  isError: true,
});

function userResultFrame(toolUse, toolUseResult, {error = false} = {}) {
  return {
    message: {
      type: 'user',
      message: {role: 'user', content: [{
        type: 'tool_result', content: error ? '<tool_use_error>denied</tool_use_error>' : 'started',
        is_error: error, tool_use_id: toolUse.id,
      }]},
      toolUseResult,
      ...(error && {toolDenialKind: 'permission-rule'}),
    },
  };
}

async function callAgent({agentId, status, error, signal, isLaunchOnly = false, registry, leftRunning = []}) {
  const nativeResult = {status: 'async_launched', agentId, resolvedModel: 'claude-sonnet-4-5'};
  context.W1 = async function* (toolUse) { yield userResultFrame(toolUse, nativeResult); };
  context.Hu = (_plugin, _api) => ({
    tools: () => [{name: 'Agent'}],
    canUseTool: async () => ({behavior: 'allow'}),
    toolContext: () => ({taskRegistry: registry.registry, messages: []}),
  });
  const result = await context.eU(
    {tool: 'Agent', prompt: 'test'},
    {plugin: 'plug', signal, origin: ['fixture'], isLaunchOnly,
      leftRunning: promise => leftRunning.push(promise)},
  );
  return {result, nativeResult, leftRunning};
}

const wrapperCompleted = makeRegistry({
  rows: [{id: 'task-wrapper-complete', type: 'local_agent', agentId: 'agent-wrapper-complete', status: 'completed'}],
  transcripts: {'agent-wrapper-complete': assistantText('outer result')},
});
setupClock();
const completeCall = await callAgent({agentId: 'agent-wrapper-complete', registry: wrapperCompleted,
  signal: new AbortController().signal});
assert.deepEqual(JSON.parse(JSON.stringify(completeCall.result)), {
  result: {agentId: 'agent-wrapper-complete', resolvedModel: 'claude-sonnet-4-5'},
  text: 'outer result',
});

const wrapperFailed = makeRegistry({
  rows: [{id: 'task-wrapper-fail', type: 'local_agent', agentId: 'agent-wrapper-fail', status: 'failed', error: 'native failure'}],
  transcripts: {'agent-wrapper-fail': assistantText('ignored transcript')},
});
setupClock();
const failCall = await callAgent({agentId: 'agent-wrapper-fail', registry: wrapperFailed,
  signal: new AbortController().signal});
assert.equal(failCall.result.result, undefined);
assert.equal(Object.hasOwn(failCall.result, 'result'), true);
assert.equal(failCall.result.text,
  'plug: $.agent.spawn: the subagent failed: native failure');
assert.equal(failCall.result.isError, true);
assert.equal(Object.hasOwn(JSON.parse(JSON.stringify(failCall.result)), 'result'), false,
  'Native undefined result is omitted by JSON serialization');

const wrapperKilled = makeRegistry({
  rows: [{id: 'task-wrapper-killed', type: 'local_agent', agentId: 'agent-wrapper-killed', status: 'killed'}],
  transcripts: {'agent-wrapper-killed': assistantText('last output')},
});
setupClock();
const killedCall = await callAgent({agentId: 'agent-wrapper-killed', registry: wrapperKilled,
  signal: new AbortController().signal});
assert.equal(killedCall.result.text,
  'plug: $.agent.spawn: the subagent killed: last output');
assert.equal(killedCall.result.isError, true);

const wrapperNoRecord = makeRegistry();
setupClock({step: 600000});
const noRecordCall = await callAgent({agentId: 'never-created', registry: wrapperNoRecord,
  signal: new AbortController().signal});
assert.equal(noRecordCall.result.text, 'plug: $.agent.spawn: no answer within 10 minutes');
assert.equal(noRecordCall.result.isError, true);
assert.equal(Object.hasOwn(JSON.parse(JSON.stringify(noRecordCall.result)), 'result'), false);

const wrapperNoRecordAbort = makeRegistry();
setupClock();
const noRecordAbortController = new AbortController();
noRecordAbortController.abort();
const noRecordAbort = await callAgent({agentId: 'never-created-aborted', registry: wrapperNoRecordAbort,
  signal: noRecordAbortController.signal});
assert.equal(noRecordAbort.result.text, 'plug: $.agent.spawn aborted');
assert.equal(noRecordAbort.result.isError, true);
assert.equal(noRecordAbort.leftRunning.length, 0,
  'abort before an Agent registry record exists has no settlement promise');

const wrapperCancelled = makeRegistry({
  rows: [{id: 'task-wrapper-cancelled', type: 'local_agent', agentId: 'agent-wrapper-cancelled', status: 'cancelled'}],
});
setupClock({onSleep: async () => {
  const current = wrapperCancelled.rows.get('task-wrapper-cancelled');
  if (current?.status === 'cancelled') {
    wrapperCancelled.rows.set('task-wrapper-cancelled', {...current, status: 'killed'});
  }
}});
const abortController = new AbortController();
abortController.abort();
const cancelledCall = await callAgent({agentId: 'agent-wrapper-cancelled', registry: wrapperCancelled,
  signal: abortController.signal});
assert.equal(cancelledCall.result.text, 'plug: $.agent.spawn aborted');
assert.equal(cancelledCall.result.isError, true);
assert.equal(cancelledCall.leftRunning.length, 1,
  'abort after observing the Agent registers its still-running settlement');
await cancelledCall.leftRunning[0];

const wrapperEvicted = makeRegistry({
  rows: [{id: 'task-wrapper-evicted', type: 'local_agent', agentId: 'agent-wrapper-evicted', status: 'running'}],
  transcripts: {'agent-wrapper-evicted': assistantText('surviving transcript')},
});
setupClock({step: 150, onSleep: async () => { wrapperEvicted.rows.delete('task-wrapper-evicted'); }});
const evictedCall = await callAgent({agentId: 'agent-wrapper-evicted', registry: wrapperEvicted,
  signal: new AbortController().signal});
assert.deepEqual(JSON.parse(JSON.stringify(evictedCall.result)), {
  result: {agentId: 'agent-wrapper-evicted', resolvedModel: 'claude-sonnet-4-5'},
  text: 'surviving transcript',
});

const wrapperLaunch = makeRegistry({
  rows: [{id: 'task-wrapper-launch', type: 'local_agent', agentId: 'agent-wrapper-launch', status: 'completed'}],
  transcripts: {'agent-wrapper-launch': assistantText('settled later')},
});
setupClock();
const launchCall = await callAgent({agentId: 'agent-wrapper-launch', registry: wrapperLaunch,
  signal: new AbortController().signal, isLaunchOnly: true});
assert.deepEqual(JSON.parse(JSON.stringify(launchCall.result)), {
  result: {agentId: 'agent-wrapper-launch', resolvedModel: 'claude-sonnet-4-5'}, text: '',
});
assert.equal(launchCall.leftRunning.length, 1,
  'launch-only response registers its background settle promise');
await launchCall.leftRunning[0];

console.log('passed 2.1.289 Agent API result projection and UOt completion/failure/kill/cancel/timeout/eviction probes (controlled registry/W1 only)');
