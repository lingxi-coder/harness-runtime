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

test('cold connectAgent is read-only and replays an existing run with no initial messages', async () => {
  const threadId = 'existing-thread';
  const runId = 'existing-run';
  const transport = transportFor([{events: [started(threadId, runId), finished(threadId, runId)], sequence: 2}]);
  const client = createLocalAppAgent({transport, threadId, pollIntervalMs: 10});
  const result = await client.connectAgent({runId});

  assert.deepEqual(result, {result: 'done', newMessages: []});
  assert.equal(transport.starts.length, 0);
  assert.deepEqual(transport.eventRequests, [{runId}]);
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
  const transport = transportFor([{events: [started(input.threadId, input.runId)], sequence: 0}], {state: 'succeeded'});
  const client = createLocalAppAgent({transport, threadId: input.threadId, pollIntervalMs: 10});
  await assert.rejects(client.runInput(input), /without a terminal AG-UI event/);
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
