#!/usr/bin/env node
// Execute Claude Code 2.1.289's exact StreamingToolExecutor class with only
// its tool-definition, W1 execution, abort, and row-projection imports replaced
// by controlled test seams. No CLI session or provider is started.
//
// Usage:
//   node scripts/tests/mods_agent_executor_schedule_289_oracle.mjs <2.1.289-binary>

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const [binaryPath] = process.argv.slice(2);
assert(binaryPath, 'usage: node scripts/tests/mods_agent_executor_schedule_289_oracle.mjs <2.1.289-binary>');

const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 binary SHA-256',
);

const sourceWindows = [];
function take(label, start, end, expectedHash, startText) {
  assert(start >= 0 && end > start, `${label}: invalid byte range`);
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: short source range`);
  if (startText) {
    assert.equal(
      bytes.subarray(0, Buffer.byteLength(startText)).toString(),
      startText,
      `${label}: source marker changed`,
    );
  }
  assert.equal(sha256(bytes), expectedHash, `${label}: source hash changed`);
  sourceWindows.push({label, start, end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

const executorSource = take(
  'Native ds StreamingToolExecutor class',
  195349196, 195357760,
  '6459257ae75dd02d6b6f06df7f14bf7ce0468ebea7d3ce7909e239bdd527cba8',
  'class ds{toolDefinitions;canUseTool;now;tools=[]',
);
const addToolSource = take(
  'Native addTool method',
  195350179, 195351516,
  '92b720b6a34784e97521f5d290052a628d34aa365dc8b674d7e6fde78f810b2a',
  'addTool(e,o){',
);
const completedResultsSource = take(
  'Native getCompletedResults method',
  195355929, 195356509,
  '39bd63336ce18174bef5b3867e0e6b721153fb64194a1ce144cd8418f2aa2bed',
  'getCompletedResults(){',
);
const pollAndErrorFinalizerSource = take(
  'Native Tn completed-result poll and adjacent finalizer',
  195439370, 195440609,
  'd082ae78e6737bc5c45d5adda2d515c8406231b5f3d8e63d23f0ac0041ce3ce7',
  'function*Tn(h=!0){',
);
const afterProviderEventSource = take(
  'Native provider-event yield, tool enqueue, then Tn poll',
  195466181, 195469377,
  '380eb29894b93878ef7fc2d3b75c57ffc73b9d331a838520dff9b949e656570c',
  'if(M.type==="assistant"&&!M.isApiErrorMessage&&!mr([M]))',
);
const finalDrainSource = take(
  'Native post-stream getRemainingResults drain',
  195485961, 195489200,
  '7fcac4553bbb01791b1e0da0e5005fdc6c061a051391aa00de6bc79d42995a47',
  'let fd=ue.streamingToolExecutor.getRemainingResults();',
);

assert(executorSource.includes(addToolSource), 'addTool extraction is not inside the pinned class');
assert(executorSource.includes(completedResultsSource), 'getCompletedResults extraction is not inside the pinned class');
const method = (startMarker, endMarker) => {
  const start = executorSource.indexOf(startMarker);
  const end = executorSource.indexOf(endMarker, start + startMarker.length);
  assert(start >= 0 && end > start, `Native class method missing: ${startMarker}`);
  return executorSource.slice(start, end);
};
const addTool = method('addTool(e,o){', 'canExecuteTool(e){');
const processQueue = method('async processQueue(){', 'createSyntheticErrorMessage(e,o,n){');
const executeTool = method('async executeTool(e){', '*getCompletedResults(){');
const getCompletedResults = method('*getCompletedResults(){', 'hasPendingProgress(){');
const getRemainingResults = method('async*getRemainingResults(){', 'applyEndedRunLayers(){');
const discard = method('discard(){', 'discardAndAbortInFlight(e){');

assert.equal(addTool, addToolSource, 'Native addTool source window differs from class method');
assert.equal(
  getCompletedResults.slice(1),
  completedResultsSource,
  'Native getCompletedResults source window differs from class method',
);
assert.match(addTool, /this\.processQueue\(\)/);
assert.match(processQueue, /await this\.executeTool\(e\)/);
assert.match(processQueue, /else if\(!e\.isConcurrencySafe\)break/);
assert.match(executeTool, /s\.finally\(\(\)=>\{this\.processQueue\(\)\}\)/);
assert.doesNotMatch(getCompletedResults, /processQueue|executeTool/);
assert.match(getCompletedResults,
  /else if\(e\.status==="executing"&&!e\.isConcurrencySafe\)break/);
assert.match(getRemainingResults, /await this\.processQueue\(\)/);
assert.match(discard, /this\.discarded=!0/);

const tNEnd = pollAndErrorFinalizerSource.indexOf('function*wr(');
assert(tNEnd > 0, 'Native Tn method terminator is missing');
const nativeTnSource = pollAndErrorFinalizerSource.slice(0, tNEnd).trim();
assert.match(nativeTnSource, /^function\*Tn\(h=!0\)\{/);
assert.match(nativeTnSource, /ue\.streamingToolExecutor\.getCompletedResults\(\)/);
const eventYield = afterProviderEventSource.indexOf('yield Me');
const enqueue = afterProviderEventSource.indexOf('ue.streamingToolExecutor.addTool(ve,X)');
const eventPoll = afterProviderEventSource.indexOf('yield*Tn()');
assert(eventYield >= 0 && enqueue > eventYield && eventPoll > enqueue,
  'Native must yield the provider row before enqueueing its tools and polling Tn');
assert.match(finalDrainSource, /getRemainingResults\(\)/);

function deferred() {
  let resolve;
  const promise = new Promise(done => { resolve = done; });
  return {promise, resolve};
}

async function within(promise, label, timeoutMs = 1500) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${label}: timed out after ${timeoutMs}ms`)), timeoutMs);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function waitFor(predicate, label, timeoutMs = 1500) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error(`${label}: timed out after ${timeoutMs}ms`);
    await new Promise(resolve => setImmediate(resolve));
  }
}

function makeRow(block) {
  return {
    type: 'user',
    uuid: `result-${block.id}`,
    message: {
      role: 'user',
      content: [{type: 'tool_result', tool_use_id: block.id, content: `done:${block.id}`, is_error: false}],
    },
  };
}

function toolName(id) {
  return `Tool${id.slice(id.lastIndexOf('-') + 1).toUpperCase()}`;
}

function makeHarness(ids, safeIds = []) {
  const plans = new Map(ids.map(id => [id, {started: deferred(), release: deferred()}]));
  const safe = new Set(safeIds);
  const w1Calls = [];
  const syntheticRows = [];
  const toolDefinitions = new Map(ids.map(id => [toolName(id), {
    inputSchema: {safeParse: input => ({success: true, data: input})},
    isConcurrencySafe: () => safe.has(id),
  }]));
  const W1 = (block, _assistantMessage, _canUseTool, context) => {
    const plan = plans.get(block.id);
    assert(plan, `unexpected Native W1 call for ${block.id}`);
    w1Calls.push(block.id);
    return (async function* controlledW1() {
      plan.started.resolve();
      const signal = context.abortController.signal;
      const outcome = await Promise.race([
        plan.release.promise.then(() => 'complete'),
        new Promise(resolve => {
          if (signal.aborted) resolve('abort');
          else signal.addEventListener('abort', () => resolve('abort'), {once: true});
        }),
      ]);
      if (outcome === 'abort') return;
      yield {message: makeRow(block)};
    })();
  };
  const sandbox = vm.createContext({
    AbortController,
    yn: (definitions, name) => definitions.get(name),
    wVn: () => false,
    Tcn: () => undefined,
    KMt: () => undefined,
    rh: () => new AbortController(),
    W1,
    jP: () => false,
    DLr: () => false,
    Re: input => {
      syntheticRows.push(input);
      const result = input.content?.[0];
      return {
        type: 'user',
        uuid: `synthetic-${result?.tool_use_id}`,
        message: {role: 'user', content: input.content ?? []},
        toolUseResult: input.toolUseResult,
      };
    },
    owe: (_context, _layers) => _context,
    i: () => {},
    wu: () => ({}),
  });
  const NativeExecutor = vm.runInContext(`(${executorSource})`, sandbox);
  const toolUseContext = {
    options: {toolAliases: []},
    abortController: new AbortController(),
    agentId: 'scheduler-oracle',
  };
  const executor = new NativeExecutor(toolDefinitions, () => true, toolUseContext, () => 0);
  function addTool(id) {
    const block = {id, name: toolName(id), input: {}};
    executor.addTool(block, {
      uuid: `assistant-${id}`,
      requestId: `request-${id}`,
      message: {content: [block]},
    });
  }
  return {executor, plans, w1Calls, syntheticRows, sandbox, addTool};
}

function entry(executor, id) {
  return executor.tools.find(tool => tool.id === id);
}

const checks = [];
async function check(name, run) {
  await run();
  checks.push(name);
}

await check('addTool autonomously starts its first W1 call without polling Tn', async () => {
  const harness = makeHarness(['tool-a']);
  let completedPolls = 0;
  const getCompleted = harness.executor.getCompletedResults.bind(harness.executor);
  harness.executor.getCompletedResults = function* countedGetCompletedResults() {
    completedPolls++;
    yield* getCompleted();
  };
  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'first W1 start');
  assert.deepEqual(harness.w1Calls, ['tool-a']);
  assert.equal(entry(harness.executor, 'tool-a').status, 'executing');
  assert.equal(completedPolls, 0, 'addTool must not consume the completed-result poll');
  harness.plans.get('tool-a').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-a').status === 'completed', 'first W1 completion');
  assert.equal(completedPolls, 0);
});

await check('A completion finally advances queued B without another provider event or Tn', async () => {
  const harness = makeHarness(['tool-a', 'tool-b']);
  const nativeProcessQueue = harness.executor.processQueue.bind(harness.executor);
  let processQueueCalls = 0;
  harness.executor.processQueue = (...args) => {
    processQueueCalls++;
    return nativeProcessQueue(...args);
  };
  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'A W1 start');
  harness.addTool('tool-b');
  assert.equal(entry(harness.executor, 'tool-b').status, 'queued');
  assert.deepEqual(harness.w1Calls, ['tool-a']);
  const callsBeforeACompletion = processQueueCalls;
  harness.plans.get('tool-a').release.resolve();
  await within(harness.plans.get('tool-b').started.promise, 'B W1 start after A finally');
  assert.equal(processQueueCalls, callsBeforeACompletion + 1,
    'the Native executeTool promise finally must advance the queue');
  assert.deepEqual(harness.w1Calls, ['tool-a', 'tool-b']);
  harness.plans.get('tool-b').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-b').status === 'completed', 'B completion');
});

await check('fully completed concurrency-safe tools drain in registration order after B finishes first', async () => {
  const harness = makeHarness(['tool-a', 'tool-b'], ['tool-a', 'tool-b']);
  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'A W1 start');
  harness.addTool('tool-b');
  await within(harness.plans.get('tool-b').started.promise, 'B W1 start');
  harness.plans.get('tool-b').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-b').status === 'completed', 'B completion first');
  assert.equal(entry(harness.executor, 'tool-a').status, 'executing');
  harness.plans.get('tool-a').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-a').status === 'completed', 'A completion second');

  const drained = [...harness.executor.getCompletedResults()]
    .filter(result => result.message?.uuid)
    .map(result => result.message.uuid);
  assert.deepEqual(drained, ['result-tool-a', 'result-tool-b']);
});

await check('a completed safe B is consumable while earlier safe A remains executing', async () => {
  const harness = makeHarness(['tool-a', 'tool-b'], ['tool-a', 'tool-b']);
  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'A W1 start');
  harness.addTool('tool-b');
  await within(harness.plans.get('tool-b').started.promise, 'B W1 start');
  harness.plans.get('tool-b').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-b').status === 'completed', 'B completion');

  const completedB = [...harness.executor.getCompletedResults()]
    .filter(result => result.message?.uuid)
    .map(result => result.message.uuid);
  assert.deepEqual(completedB, ['result-tool-b']);
  assert.equal(entry(harness.executor, 'tool-a').status, 'executing');

  harness.plans.get('tool-a').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-a').status === 'completed', 'A completion');
  const completedA = [...harness.executor.getCompletedResults()]
    .filter(result => result.message?.uuid)
    .map(result => result.message.uuid);
  assert.deepEqual(completedA, ['result-tool-a']);
});

await check('getCompletedResults yields only completed entries and never starts queued work', async () => {
  const harness = makeHarness(['tool-a', 'tool-b']);
  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'A W1 start');
  harness.addTool('tool-b');
  const polled = [...harness.executor.getCompletedResults()];
  assert.deepEqual(polled, []);
  assert.equal(entry(harness.executor, 'tool-b').status, 'queued');
  assert.deepEqual(harness.w1Calls, ['tool-a']);
  harness.plans.get('tool-a').release.resolve();
  await within(harness.plans.get('tool-b').started.promise, 'B W1 start after A completion');
  harness.plans.get('tool-b').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-b').status === 'completed', 'B completion');
});

await check('an executing unsafe tool is a result-drain barrier for later queued safe work', async () => {
  const harness = makeHarness(['tool-a', 'tool-b', 'tool-c'], ['tool-a', 'tool-c']);
  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'safe A W1 start');
  harness.plans.get('tool-a').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-a').status === 'completed', 'safe A completion');

  harness.addTool('tool-b');
  await within(harness.plans.get('tool-b').started.promise, 'unsafe B W1 start');
  harness.addTool('tool-c');
  assert.equal(entry(harness.executor, 'tool-c').status, 'queued');
  const drained = [...harness.executor.getCompletedResults()]
    .filter(result => result.message?.uuid)
    .map(result => result.message.uuid);
  assert.deepEqual(drained, ['result-tool-a']);
  assert.equal(entry(harness.executor, 'tool-b').status, 'executing');
  assert.equal(entry(harness.executor, 'tool-c').status, 'queued');

  harness.plans.get('tool-b').release.resolve();
  await within(harness.plans.get('tool-c').started.promise, 'safe C W1 start after B');
  harness.plans.get('tool-c').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-c').status === 'completed', 'safe C completion');
});

await check('discard prevents a queued tool from entering W1', async () => {
  const harness = makeHarness(['tool-a', 'tool-b']);
  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'A W1 start');
  harness.addTool('tool-b');
  assert.equal(entry(harness.executor, 'tool-b').status, 'queued');
  const discarded = harness.executor.discardAndAbortInFlight('server fallback');
  assert.equal(discarded.queuedNeverStarted, 1);
  await waitFor(() => entry(harness.executor, 'tool-b').status === 'completed', 'discarded B synthetic result');
  assert.deepEqual(harness.w1Calls, ['tool-a']);
  assert(harness.syntheticRows.some(input => input.content?.[0]?.tool_use_id === 'tool-b'));
});

await check('a completed result stays queued until the post-provider-event Tn poll', async () => {
  const harness = makeHarness(['tool-a']);
  const delivered = [];
  const recorded = [];
  const toolRows = [];
  harness.sandbox.f = {abortController: new AbortController()};
  harness.sandbox.ue = {streamingToolExecutor: harness.executor, toolDurations: []};
  harness.sandbox.bn = message => {
    const output = {type: 'native-row', message};
    delivered.push(output);
    return output;
  };
  harness.sandbox.Ar = message => recorded.push(message);
  harness.sandbox.xr = message => toolRows.push(message);
  const NativeTn = vm.runInContext(`(${nativeTnSource})`, harness.sandbox);

  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'A W1 start');
  const providerGate = deferred();
  const provider = (async function* controlledProviderEvent() {
    await providerGate.promise;
    yield {type: 'provider-event'};
  })();
  const nextProviderEvent = provider.next();
  harness.plans.get('tool-a').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-a').status === 'completed', 'A completion before provider event');
  assert.equal(delivered.length, 0, 'executor completion alone must not emit a result row');
  assert.equal(recorded.length, 0);
  assert.equal(toolRows.length, 0);

  providerGate.resolve();
  const event = await within(nextProviderEvent, 'provider event boundary');
  assert.deepEqual(event, {value: {type: 'provider-event'}, done: false});
  const polled = [...NativeTn()];
  assert.equal(polled.length, 1);
  assert.equal(polled[0].type, 'native-row');
  assert.equal(polled[0].message.uuid, 'result-tool-a');
  assert.equal(delivered.length, 1);
  assert.equal(recorded.length, 1);
  assert.equal(toolRows.length, 1);
});

await check('getRemainingResults performs the Native final drain', async () => {
  const harness = makeHarness(['tool-a']);
  harness.addTool('tool-a');
  await within(harness.plans.get('tool-a').started.promise, 'A W1 start');
  harness.plans.get('tool-a').release.resolve();
  await waitFor(() => entry(harness.executor, 'tool-a').status === 'completed', 'A completion');
  const drained = [];
  for await (const result of harness.executor.getRemainingResults()) drained.push(result);
  assert(drained.some(result => result.message?.uuid === 'result-tool-a'));
  assert(drained.some(result => result.type === 'set_in_progress_tool_use_ids'));
  assert.equal(entry(harness.executor, 'tool-a').status, 'yielded');
});

console.log(JSON.stringify({
  oracle: 'mods-agent-executor-schedule-2.1.289',
  binary: {path: binaryPath, sha256: sha256(binary)},
  sourceWindows,
  checks,
  executionBoundary: {
    executed: 'The exact Native ds class and exact Tn method extracted from the pinned binary.',
    controlledSeams: ['tool definition lookup/schema', 'W1 async-generator', 'abort controller', 'row projection helpers'],
    notExecuted: ['Native CLI/session startup', 'real provider/network', 'real tool implementations/permissions', 'complete query generator'],
  },
}, null, 2));
