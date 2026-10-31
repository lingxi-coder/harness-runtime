import assert from 'node:assert/strict';
import {File} from 'node:buffer';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import {AbstractAgent} from '@ag-ui/client';
import {ContentPartSchema, EventSchema, RunAgentInputSchema} from '@ag-ui/core/schemas';
import {browserAgentSource} from '../dist/browser.source.js';
import {
  createLocalAppAgent,
  fileToContentPart,
  validateEvent,
  validateLocalAppRunInput,
  validateRunAgentInput,
} from '../src/index.js';
import {preflightJson} from '../src/preflight.js';

if (!globalThis.File) globalThis.File = File;

const generatedRunInput = JSON.parse(readFileSync(new URL('./fixtures/run-agent-input.ag-ui-1.0.3.json', import.meta.url), 'utf8'));

function userMessage(id, text = 'hello') {
  return {id, role: 'user', content: [{type: 'text', text}]};
}

function runInput({threadId = 'thread-1', runId = 'run-1', messages = [userMessage('user-1')] } = {}) {
  return {threadId, runId, messages};
}

function started(threadId, runId) {
  return {type: 'RUN_STARTED', threadId, runId, protocolVersion: '1.0'};
}

function finished(threadId, runId, outcome = {type: 'success'}, result = 'done') {
  return {type: 'RUN_FINISHED', threadId, runId, outcome, result};
}

function transportFor(pages, {state = 'running'} = {}) {
  const starts = [];
  const eventRequests = [];
  let pageIndex = 0;
  return {
    starts,
    eventRequests,
    start: async (input, options) => {
      starts.push({input, options});
      return {runId: input.runId};
    },
    events: async request => {
      eventRequests.push(request);
      return pages[Math.min(pageIndex++, pages.length - 1)];
    },
    status: async () => ({state}),
  };
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((done, fail) => { resolve = done; reject = fail; });
  return {promise, resolve, reject};
}

function gatedRunTransport(threadId, runId) {
  const entered = deferred();
  const page = deferred();
  const cancellations = [];
  const transport = transportFor([]);
  transport.events = async (request, {signal}) => {
    transport.eventRequests.push(request);
    if (request.runId !== runId) {
      return {events: [started(threadId, request.runId), finished(threadId, request.runId)], sequence: 2};
    }
    entered.resolve(signal);
    return await page.promise;
  };
  transport.cancel = async request => { cancellations.push(request); };
  const finish = (outcome = {type: 'success'}) => page.resolve({
    events: [started(threadId, runId), finished(threadId, runId, outcome)], sequence: 2,
  });
  return {transport, entered, page, cancellations, finish};
}

function journalTransport(log, {pageSize = 128, state = 'succeeded'} = {}) {
  const starts = [];
  const eventRequests = [];
  const statusRequests = [];
  return {
    starts, eventRequests, statusRequests,
    async start(input) { starts.push(input); return {runId: input.runId}; },
    async events(request) {
      eventRequests.push({...request});
      const cursor = request.afterSequence ?? 0;
      const events = log.slice(cursor, cursor + pageSize);
      return {events, sequence: cursor + events.length};
    },
    async status(request) { statusRequests.push({...request}); return {state}; },
  };
}

test('official schemas validate RunAgentInput, BaseEvent and a real image File part', async () => {
  const bytes = new Uint8Array(1024);
  bytes[0] = 0x89;
  bytes[1] = 0x50;
  bytes[bytes.length - 1] = 0x7f;
  const file = new File([bytes], 'photo.png', {type: 'image/png'});
  const part = await fileToContentPart(file);

  assert.equal(part.type, 'image');
  assert.equal(part.source.type, 'data');
  assert.equal(part.source.mimeType, 'image/png');
  assert.equal(part.metadata['com.lingxi.localApp'].filename, 'photo.png');
  assert.deepEqual(ContentPartSchema.parse(part), part);
  const decoded = Buffer.from(part.source.value, 'base64');
  assert.deepEqual(decoded, Buffer.from(bytes));

  const input = runInput({messages: [{id: 'user-1', role: 'user', content: [part]}]});
  assert.deepEqual(validateRunAgentInput(input), RunAgentInputSchema.parse(input));
  assert.equal(validateEvent(started(input.threadId, input.runId)).type, 'RUN_STARTED');
  assert.throws(() => validateEvent({type: 'NOT_AN_AG_UI_EVENT'}));
});

test('the ESM browser injection asset contains the exact built IIFE source', () => {
  const iife = readFileSync(new URL('../dist/browser.global.js', import.meta.url), 'utf8');
  assert.equal(browserAgentSource, iife);
});

test('Local App options are limited to the versioned forwardedProps namespace', () => {
  const input = runInput();
  input.forwardedProps = {'com.lingxi.localApp': {
    version: 1,
    outputSchema: {type: 'object'},
    tools: ['builtin:Read'],
    skills: ['extract-text'],
    maxTurns: 3,
    timeoutMs: 10000,
  }};
  assert.equal(validateLocalAppRunInput(input).forwardedProps['com.lingxi.localApp'].maxTurns, 3);
  assert.throws(() => validateLocalAppRunInput({...runInput(), forwardedProps: {other: {version: 1}}}), /only forwardedProps/);
  assert.throws(() => validateLocalAppRunInput({...runInput(), forwardedProps: {'com.lingxi.localApp': {version: 1, privateTools: []}}}), /unsupported field/);
  assert.throws(() => validateLocalAppRunInput({...runInput(), forwardedProps: {'com.lingxi.localApp': {version: 1, maxTurns: 101}}}), /maxTurns/);
});

test('pinned SDK prepareRunAgentInput reproduces the native AG-UI fixture', () => {
  const client = createLocalAppAgent({
    transport: {start: async input => ({runId: input.runId}), events: async () => ({events: [], sequence: 0})},
    threadId: generatedRunInput.threadId,
    initialMessages: generatedRunInput.messages,
  });
  const prepared = client.prepareRunAgentInput({
    runId: generatedRunInput.runId,
    forwardedProps: generatedRunInput.forwardedProps,
    resume: generatedRunInput.resume,
  });
  assert.deepEqual(JSON.parse(JSON.stringify(prepared)), generatedRunInput);
  assert.deepEqual(RunAgentInputSchema.parse(generatedRunInput), generatedRunInput);
  assert.deepEqual(generatedRunInput.tools, []);
  assert.deepEqual(generatedRunInput.context, []);
  assert.deepEqual(generatedRunInput.state, {});
  assert.deepEqual(generatedRunInput.resume, []);
});

test('JSON preflight rejects pathological arrays and nesting before official schema parsing', () => {
  const runInput = {threadId: 'thread-1', runId: 'run-1', messages: new Array(10_001).fill(null)};
  assert.throws(() => validateRunAgentInput(runInput), error => error.code === 'agui_input_array_too_large');
  let nested = {};
  for (let index = 0; index < 65; index++) nested = {next: nested};
  assert.throws(() => preflightJson(nested), error => error.code === 'agui_input_too_deep');
});

test('real 8 MiB and 25 MiB Files remain valid inline DataSource parts', async () => {
  for (const size of [8, 25].map(mib => mib * 1024 * 1024)) {
    const bytes = Buffer.alloc(size, 0x5a);
    const file = new File([bytes], size === 25 * 1024 * 1024 ? 'large.png' : 'medium.png', {type: 'image/png'});
    const part = await fileToContentPart(file);
    assert.deepEqual(ContentPartSchema.parse(part), part);
    assert.equal(part.source.value.length, Math.ceil(size / 3) * 4);
    assert.equal(Buffer.from(part.source.value, 'base64').length, size);
    const input = runInput({runId: `run-${size}`, messages: [{id: `user-${size}`, role: 'user', content: [part]}]});
    assert.equal(validateLocalAppRunInput(input).messages[0].content[0].source.value.length, part.source.value.length);
  }
});

test('a plain-text File stays an official DocumentPart with UTF-8 MIME and filename metadata', async () => {
  const file = new File(['hello'], 'notes.txt', {type: ''});
  const part = await fileToContentPart(file);
  assert.equal(part.type, 'document');
  assert.equal(part.source.mimeType, 'text/plain;charset=utf-8');
  assert.equal(part.metadata['com.lingxi.localApp'].filename, 'notes.txt');
  assert.deepEqual(Buffer.from(part.source.value, 'base64'), Buffer.from('hello'));
});

test('the shared client is the official AbstractAgent and resolves only after standard success events', async () => {
  const input = runInput();
  const transport = transportFor([
    {events: [started(input.threadId, input.runId), {type: 'TEXT_MESSAGE_START', messageId: 'assistant-1', role: 'assistant'}], sequence: 1},
    {events: [
      {type: 'TEXT_MESSAGE_CONTENT', messageId: 'assistant-1', delta: 'hello'},
      {type: 'TEXT_MESSAGE_END', messageId: 'assistant-1'},
      finished(input.threadId, input.runId, {type: 'success'}, 'answer'),
    ], sequence: 4},
  ]);
  const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
  assert(client instanceof AbstractAgent);
  client.addMessage(input.messages[0]);
  const types = [];
  const result = await client.runAgent({runId: input.runId}, {onEvent: ({event}) => types.push(event.type)});

  assert.deepEqual(types, [
    'RUN_STARTED', 'TEXT_MESSAGE_START', 'TEXT_MESSAGE_CONTENT', 'TEXT_MESSAGE_END', 'RUN_FINISHED',
  ]);
  assert.equal(result.result, 'answer');
  assert.equal(result.newMessages.length, 1);
  assert.equal(result.newMessages[0].role, 'assistant');
  assert.equal(transport.starts.length, 1);
  assert.equal(transport.starts[0].input.runId, input.runId);
  assert.equal(transport.starts[0].input.protocolVersion, '1.0');
  assert.equal(transport.starts[0].input.forwardedProps['com.lingxi.localApp'].version, 1);
});

test('event pages cap event count separately from bounded nested structured results', async () => {
  const input = runInput();
  const structuredResult = Array.from({length: 200}, (_, index) => ({index}));
  const validEvents = [
    started(input.threadId, input.runId),
    {type: 'TEXT_MESSAGE_START', messageId: 'assistant-large-result', role: 'assistant'},
    {type: 'TEXT_MESSAGE_CONTENT', messageId: 'assistant-large-result', delta: 'validated'},
    {type: 'TEXT_MESSAGE_END', messageId: 'assistant-large-result'},
    finished(input.threadId, input.runId, {type: 'success'}, structuredResult),
  ];
  const validTransport = transportFor([{events: validEvents, sequence: validEvents.length}], {state: 'succeeded'});
  const validClient = createLocalAppAgent({transport: validTransport, threadId: input.threadId, pollIntervalMs: 10});
  const result = await validClient.runInput(input);
  assert.deepEqual(result.result, structuredResult);
  assert.equal(result.newMessages.length, 1);

  const oversizedInput = runInput({threadId: 'oversized-thread', runId: 'oversized-run'});
  const oversizedEvents = [
    started(oversizedInput.threadId, oversizedInput.runId),
    ...Array.from({length: 127}, () => ({
      type: 'TEXT_MESSAGE_CONTENT', messageId: 'assistant-long-stream', delta: 'chunk',
    })),
    finished(oversizedInput.threadId, oversizedInput.runId, {type: 'success'}, structuredResult),
  ];
  assert.equal(oversizedEvents.length, 129);
  const oversizedTransport = transportFor([{events: oversizedEvents, sequence: oversizedEvents.length}], {state: 'succeeded'});
  const oversizedClient = createLocalAppAgent({
    transport: oversizedTransport, threadId: oversizedInput.threadId, pollIntervalMs: 10,
  });
  await assert.rejects(oversizedClient.runInput(oversizedInput), /more than 128 events/);
  assert.equal(oversizedTransport.starts.length, 1);
  assert.deepEqual(oversizedClient.messages.map(message => message.role), ['user'],
    'an oversized page containing RUN_FINISHED must not produce a false successful result');
});

test('cold connectAgent is read-only and replays an existing run with no initial messages', async () => {
  const threadId = 'existing-thread';
  const runId = 'existing-run';
  const transport = transportFor([{events: [started(threadId, runId), finished(threadId, runId)], sequence: 2}]);
  const cancellations = [];
  transport.cancel = async request => { cancellations.push(request); };
  const client = createLocalAppAgent({transport, threadId, pollIntervalMs: 10});
  const result = await client.connectAgent({runId});

  assert.deepEqual(result, {result: 'done', newMessages: []});
  assert.equal(transport.starts.length, 0);
  assert.deepEqual(transport.eventRequests, [{runId}]);
  client.abortRun();
  assert.deepEqual(cancellations, [], 'successful reconnect completion must clear its cancel target');
});

test('abortRun cancels the connected run and clears its target after completion', async t => {
  for (const previousRun of [false, true]) {
    await t.test(previousRun ? 'run A then connect B' : 'fresh connect B', async () => {
      const threadId = 'existing-thread';
      const runId = 'connected-run-b';
      const {transport, entered, cancellations, finish} = gatedRunTransport(threadId, runId);
      const client = createLocalAppAgent({transport, threadId, pollIntervalMs: 10});
      if (previousRun) {
        await client.runInput(runInput({threadId, runId: 'started-run-a'}));
        client.abortRun();
        assert.deepEqual(cancellations, [], 'a completed run is no longer cancellable');
      }
      const pending = client.connectAgent({runId});
      await entered.promise;
      try {
        assert.equal(transport.starts.length, previousRun ? 1 : 0);
        assert.deepEqual(cancellations, [], 'connecting only reads the existing run');
        client.abortRun();
        assert.deepEqual(cancellations, [{runId}]);
      } finally {
        finish({type: 'cancelled'});
        await pending;
      }
      client.abortRun();
      assert.deepEqual(cancellations, [{runId}], 'completion must clear the connected cancel target');
    });
  }
});

test('abortRun targets connectAgent while onRunInitialized is still awaiting', async () => {
  const threadId = 'existing-thread';
  const runId = 'initializing-run';
  const {transport, cancellations, finish} = gatedRunTransport(threadId, runId);
  const initializing = deferred();
  const releaseInitialization = deferred();
  const client = createLocalAppAgent({transport, threadId});
  const pending = client.connectAgent({runId}, {
    async onRunInitialized() {
      initializing.resolve();
      await releaseInitialization.promise;
    },
  });
  await initializing.promise;
  try {
    assert.deepEqual(transport.eventRequests, []);
    client.abortRun();
    assert.deepEqual(cancellations, [{runId}]);
  } finally {
    finish({type: 'cancelled'});
    releaseInitialization.resolve();
    await pending;
  }
  client.abortRun();
  assert.deepEqual(cancellations, [{runId}]);
  assert.equal(transport.starts.length, 0);
});

test('failed connects clear their cancel target without falling back to an earlier run', async t => {
  for (const phase of ['input validation', 'polling']) {
    await t.test(phase, async () => {
      const threadId = 'existing-thread';
      const runId = 'failed-run-b';
      const {transport, entered, page, cancellations} = gatedRunTransport(threadId, runId);
      const client = createLocalAppAgent({transport, threadId});
      await client.runInput(runInput({threadId, runId: 'started-run-a'}));
      const failure = new Error('reconnect failed');
      if (phase === 'input validation') {
        await assert.rejects(client.connectAgent({runId, tools: [null]}));
      } else {
        let observedFailure;
        const pending = client.connectAgent({runId}, {
          onRunFailed({error}) {
            observedFailure = error;
            return {stopPropagation: true};
          },
        });
        await entered.promise;
        page.reject(failure);
        await pending;
        assert.equal(observedFailure, failure);
      }
      client.abortRun();
      assert.deepEqual(cancellations, []);
      assert.equal(transport.starts.length, 1, 'a failed reconnect must not start another run');
    });
  }
});

test('detachActiveRun only stops reading and clears the connected cancel target', async () => {
  const threadId = 'existing-thread';
  const runId = 'detached-run';
  const {transport, entered, cancellations, finish} = gatedRunTransport(threadId, runId);
  const client = createLocalAppAgent({transport, threadId});
  const pending = client.connectAgent({runId});
  const signal = await entered.promise;
  try {
    await client.detachActiveRun();
    await pending;
    assert.equal(signal.aborted, true);
    client.abortRun();
    assert.deepEqual(cancellations, [], 'detaching must never cancel host execution');
    assert.equal(transport.starts.length, 0);
  } finally {
    finish();
  }
});

test('unsubscribing an older stream cannot clear a newer stream cancel target', () => {
  const threadId = 'existing-thread';
  const cancellations = [];
  const signals = [];
  const pages = [];
  const transport = {
    async start() { throw new Error('connect must remain read-only'); },
    async events(request, {signal}) {
      const page = deferred();
      pages.push(page);
      signals.push(signal);
      return await page.promise;
    },
    async cancel(request) { cancellations.push(request); },
  };
  const client = createLocalAppAgent({transport, threadId});
  const first = client.connect(runInput({threadId, runId: 'connected-a', messages: []})).subscribe();
  const second = client.connect(runInput({threadId, runId: 'connected-b', messages: []})).subscribe();
  try {
    first.unsubscribe();
    client.abortRun();
    assert.deepEqual(cancellations, [{runId: 'connected-b'}]);
    second.unsubscribe();
    client.abortRun();
    assert.deepEqual(cancellations, [{runId: 'connected-b'}]);
    assert(signals.every(signal => signal.aborted));
  } finally {
    first.unsubscribe();
    second.unsubscribe();
    for (const page of pages) page.resolve({events: [], sequence: 0});
  }
});

test('the convenience result path rejects cancelled, interrupted and failed runs', async t => {
  const cases = [
    {
      name: 'cancelled',
      events: [started('thread-1', 'run-1'), finished('thread-1', 'run-1', {type: 'cancelled'}, undefined)],
      code: 'run_cancelled',
    },
    {
      name: 'interrupted',
      events: [started('thread-1', 'run-1'), finished('thread-1', 'run-1', {
        type: 'interrupt', interrupts: [{id: 'approval-1', reason: 'approval'}],
      }, undefined)],
      code: 'run_interrupted',
    },
    {
      name: 'failed',
      events: [started('thread-1', 'run-1'), {type: 'RUN_ERROR', message: 'provider failed', code: 'provider_error'}],
      code: 'provider_error',
    },
  ];
  for (const scenario of cases) {
    await t.test(scenario.name, async () => {
      const transport = transportFor([{events: scenario.events, sequence: scenario.events.length}], {state: 'failed'});
      const client = createLocalAppAgent({transport, threadId: 'thread-1', pollIntervalMs: 10});
      await assert.rejects(client.runInput(runInput()), error => error.code === scenario.code);
    });
  }
});

test('a terminal task without a terminal AG-UI event rejects instead of resolving as success', async () => {
  const input = runInput();
  const transport = transportFor([
    {events: [started(input.threadId, input.runId)], sequence: 1},
    {events: [], sequence: 1},
  ], {state: 'succeeded'});
  const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
  await assert.rejects(client.runInput(input), /without a terminal AG-UI event/);
  assert.deepEqual(transport.eventRequests, [{runId: input.runId}, {runId: input.runId, afterSequence: 1}]);
  assert.equal(transport.starts.length, 1);
});

test('terminal status after a stale event page drains the same run from its cursor', async () => {
  const input = runInput();
  const log = [started(input.threadId, input.runId)];
  const transport = journalTransport(log);
  const statusEntered = deferred();
  const releaseStatus = deferred();
  transport.status = async request => {
    transport.statusRequests.push({...request});
    statusEntered.resolve();
    await releaseStatus.promise;
    return {state: 'succeeded'};
  };
  const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
  const pending = client.runInput(input);
  await statusEntered.promise;
  // The previous event response was captured already. Commit the completed
  // journal before allowing the separate status request to observe success.
  log.push(
    {type: 'TEXT_MESSAGE_START', messageId: 'assistant-1', role: 'assistant'},
    {type: 'TEXT_MESSAGE_CONTENT', messageId: 'assistant-1', delta: 'answer'},
    {type: 'TEXT_MESSAGE_END', messageId: 'assistant-1'},
    finished(input.threadId, input.runId, {type: 'success'}, 'done'),
  );
  releaseStatus.resolve();
  const result = await pending;
  assert.equal(result.result, 'done');
  assert.deepEqual(result.newMessages, [{id: 'assistant-1', role: 'assistant', content: 'answer'}]);
  assert.deepEqual(transport.eventRequests, [{runId: input.runId}, {runId: input.runId, afterSequence: 1}]);
  assert.equal(transport.starts.length, 1);
  assert.equal(transport.statusRequests.length, 1);
});

test('terminal status after an old empty page rereads the unchanged cursor', async () => {
  const input = runInput();
  const log = [started(input.threadId, input.runId)];
  const transport = journalTransport(log);
  const secondStatusEntered = deferred();
  const releaseStatus = deferred();
  transport.status = async request => {
    transport.statusRequests.push({...request});
    if (transport.statusRequests.length === 1) return {state: 'running'};
    secondStatusEntered.resolve();
    await releaseStatus.promise;
    return {state: 'succeeded'};
  };
  const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
  const pending = client.runInput(input);
  await secondStatusEntered.promise;
  // The second events request returned [] at sequence 1. Commit the terminal
  // only after that old page, before the status request returns succeeded.
  log.push(finished(input.threadId, input.runId, {type: 'success'}, 'after-empty'));
  releaseStatus.resolve();
  const result = await pending;
  assert.equal(result.result, 'after-empty');
  assert.deepEqual(transport.eventRequests, [
    {runId: input.runId},
    {runId: input.runId, afterSequence: 1},
    {runId: input.runId, afterSequence: 1},
  ]);
  assert.equal(transport.starts.length, 1);
  assert.equal(transport.statusRequests.length, 2);
});

test('terminal drain handles more than 128 pages without restarting the model', async () => {
  const input = runInput();
  const log = [
    started(input.threadId, input.runId),
    {type: 'TEXT_MESSAGE_START', messageId: 'assistant-many', role: 'assistant'},
    ...Array.from({length: 128}, () => ({type: 'TEXT_MESSAGE_CONTENT', messageId: 'assistant-many', delta: 'x'})),
    {type: 'TEXT_MESSAGE_END', messageId: 'assistant-many'},
    finished(input.threadId, input.runId),
  ];
  const transport = journalTransport(log, {pageSize: 1});
  const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
  const result = await client.runInput(input);
  assert.equal(result.newMessages[0].content, 'x'.repeat(128));
  assert.equal(transport.eventRequests.length, log.length);
  assert.deepEqual(transport.eventRequests.map(request => request.afterSequence),
    [undefined, ...Array.from({length: log.length - 1}, (_, index) => index + 1)]);
  assert.equal(transport.starts.length, 1);
  assert.equal(transport.statusRequests.length, 1);
});

test('terminal drain is bounded even if a broken host produces endless pages', async () => {
  const input = runInput();
  const log = [started(input.threadId, input.runId), ...Array.from({length: 10_000}, () => ({
    type: 'CUSTOM', name: 'fixture', value: {},
  }))];
  const transport = journalTransport(log);
  const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
  await assert.rejects(client.runInput(input), /more than 10000 AG-UI events/);
  assert.equal(transport.starts.length, 1);
  assert.equal(transport.statusRequests.length, 1);
});

test('terminal drain preserves cancellation and read-only reconnect semantics', async t => {
  for (const reconnect of [false, true]) {
    await t.test(reconnect ? 'read-only reconnect' : 'new run', async () => {
      const input = runInput();
      const log = [
        started(input.threadId, input.runId),
        finished(input.threadId, input.runId, {type: 'cancelled'}, undefined),
      ];
      const transport = journalTransport(log, {pageSize: 1, state: 'cancelled'});
      const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
      if (reconnect) {
        let outcome;
        await client.connectAgent({runId: input.runId}, {onRunFinishedEvent: params => { outcome = params.outcome; }});
        assert.equal(outcome, 'cancelled');
        assert.equal(transport.starts.length, 0);
      } else {
        await assert.rejects(client.runInput(input), error => error.code === 'run_cancelled');
        assert.equal(transport.starts.length, 1);
      }
      assert.deepEqual(transport.eventRequests, [{runId: input.runId}, {runId: input.runId, afterSequence: 1}]);
      assert.equal(transport.statusRequests.length, 1);
    });
  }
});

test('terminal drain rejects invalid pages before delivering their terminal event', async () => {
  const input = runInput();
  const transport = transportFor([
    {events: [started(input.threadId, input.runId)], sequence: 1},
    {events: [finished(input.threadId, input.runId), {type: 'CUSTOM', name: 'late', value: {}}], sequence: 3},
  ], {state: 'succeeded'});
  const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
  const delivered = [];
  await assert.rejects(client.runInput(input, {onEvent: ({event}) => delivered.push(event.type)}),
    /events after an AG-UI terminal event/);
  assert.deepEqual(delivered, ['RUN_STARTED']);
  assert.equal(transport.starts.length, 1);
});

test('unsupported stateless history fails before the host transport starts', async () => {
  const transport = transportFor([]);
  const client = createLocalAppAgent({transport, threadId: 'fresh-thread'});
  const input = runInput({messages: [userMessage('one'), userMessage('two')]});
  await assert.rejects(client.runInput(input), /exactly one user message/);
  assert.equal(transport.starts.length, 0);
});

test('persistent chat history is retained while only a new user message starts the run', async () => {
  const oldMessage = userMessage('old-user', 'old');
  const newMessage = userMessage('new-user', 'new');
  const input = runInput({threadId: 'chat-1', runId: 'chat-run', messages: [oldMessage, newMessage]});
  const transport = transportFor([{events: [
    started(input.threadId, input.runId), finished(input.threadId, input.runId),
  ], sequence: 2}]);
  const client = createLocalAppAgent({
    transport,
    threadId: 'chat-1',
    initialMessages: [oldMessage],
    persistentChat: true,
    pollIntervalMs: 10,
  });
  const result = await client.runInput(input);
  assert.equal(result.result, 'done');
  assert.deepEqual(transport.starts[0].options.knownMessageIds, new Set(['old-user']));
  assert.equal(transport.starts[0].options.persistentChat, true);
  assert.equal(transport.starts[0].input.messages.length, 2);
});
