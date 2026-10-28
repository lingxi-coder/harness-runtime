import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {readFileSync} from 'node:fs';
import {File} from 'node:buffer';
import {webcrypto} from 'node:crypto';
import test from 'node:test';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';

const bundle = readFileSync(new URL('../../packages/local-app-agui/dist/browser.global.js', import.meta.url), 'utf8');
const source = readFileSync(new URL('../../crates/runtime/src/mobile/unified_local_apps/browser_sdk.js', import.meta.url), 'utf8');
const recoverySource = readFileSync(new URL('../../crates/runtime/src/mobile/unified_local_apps/recovery_sw.js', import.meta.url), 'utf8');

function fakeIndexedDb({databaseName = 'lingxi-local-app-agui-app-one', pendingRecords = []} = {}) {
  const databases = new Map();
  const getCounts = new Map();
  let nextGetFailure = null;
  let nextGetGate = null;

  function transactionFor(stores, storeName) {
    let pending = 0;
    let completed = false;
    let aborted = false;
    const transaction = {oncomplete: null, onerror: null, onabort: null, error: null};
    const maybeComplete = () => {
      if (pending !== 0 || completed || aborted) return;
      setImmediate(() => {
        if (pending !== 0 || completed || aborted) return;
        completed = true;
        transaction.oncomplete?.();
      });
    };
    const schedule = (request, operation) => {
      pending++;
      setImmediate(async () => {
        try {
          request.result = await operation();
          if (!aborted) request.onsuccess?.();
        } catch (error) {
          if (!aborted) {
            aborted = true;
            request.error = error;
            transaction.error = error;
            request.onerror?.();
            transaction.onerror?.();
            transaction.onabort?.();
          }
        } finally {
          pending--;
          maybeComplete();
        }
      });
      return request;
    };
    const records = stores.get(storeName);
    return {
      transaction,
      store: {
        get(key) {
          return schedule({}, async () => {
            getCounts.set(key, (getCounts.get(key) || 0) + 1);
            if (nextGetFailure) {
              const error = nextGetFailure;
              nextGetFailure = null;
              throw error;
            }
            if (nextGetGate) {
              const gate = nextGetGate;
              nextGetGate = null;
              gate.startedResolve();
              await gate.releasePromise;
            }
            return records.get(key);
          });
        },
        add(value) {
          return schedule({}, () => {
            if (records.has(value.key)) throw new Error('Key already exists');
            records.set(value.key, value);
            return value;
          });
        },
        put(value) { return schedule({}, () => (records.set(value.key, value), value)); },
        delete(key) { return schedule({}, () => records.delete(key)); },
        openCursor() {
          const request = {};
          const keys = [...records.keys()];
          let index = 0;
          const deliver = () => schedule(request, () => {
            if (index >= keys.length) return null;
            const key = keys[index];
            return {
              value: records.get(key),
              delete: () => schedule({}, () => records.delete(key)),
              continue: () => { index++; deliver(); }
            };
          });
          return deliver();
        }
      }
    };
  }

  return {
    failNextGet() { nextGetFailure = new Error('Injected IndexedDB read failure.'); },
    holdNextGet() {
      let startedResolve;
      let releaseResolve;
      const started = new Promise(resolve => { startedResolve = resolve; });
      const releasePromise = new Promise(resolve => { releaseResolve = resolve; });
      nextGetGate = {startedResolve, releasePromise};
      return {started, release: releaseResolve};
    },
    getCount(key) { return getCounts.get(key) || 0; },
    getRecord(name, storeName, key) {
      return structuredClone(databases.get(name)?.stores.get(storeName)?.get(key));
    },
    setRecord(name, storeName, key, value) {
      const records = databases.get(name)?.stores.get(storeName);
      assert.ok(records, 'Database must already be open before a second tab changes a record.');
      records.set(key, structuredClone(value));
    },
    open(name) {
      const request = {};
      setImmediate(() => {
        let db = databases.get(name);
        const upgrading = !db;
        if (!db) {
          const stores = new Map();
          db = {
            get objectStoreNames() { return {contains: storeName => stores.has(storeName)}; },
            createObjectStore(storeName) {
              const records = storeName === 'pending' && name === databaseName
                ? new Map(pendingRecords.map(record => [record.key, record])) : new Map();
              stores.set(storeName, records);
            },
            stores,
            transaction(storeName) {
              const current = transactionFor(stores, storeName);
              current.transaction.objectStore = () => current.store;
              return current.transaction;
            },
            close() {}
          };
          databases.set(name, db);
        }
        request.result = db;
        if (upgrading) request.onupgradeneeded?.();
        request.onsuccess?.();
      });
      return request;
    },
    deleteRecord(name, storeName, key) {
      return databases.get(name)?.stores.get(storeName)?.delete(key) ?? false;
    },
    deleteDatabase(name) {
      const request = {};
      setImmediate(() => { databases.delete(name); request.onsuccess?.(); });
      return request;
    }
  };
}

function eventTarget() {
  const listeners = new Map();
  return {
    addEventListener(type, listener) {
      const handlers = listeners.get(type) || new Set();
      handlers.add(listener);
      listeners.set(type, handlers);
    },
    dispatchEvent(event) {
      for (const listener of listeners.get(event.type) || []) listener.call(this, event);
      return true;
    }
  };
}

class BrowserCustomEvent {
  constructor(type, options = {}) {
    this.type = type;
    this.detail = options.detail;
  }
}

function userMessage(id, content = 'hello') {
  return {id, role: 'user', content: [{type: 'text', text: content}]};
}

function input({threadId = 'thread-one', runId = 'run-one', content = [userMessage('user-one')]} = {}) {
  return {threadId, runId, messages: content};
}

async function waitFor(predicate, message) {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  assert.fail(message);
}

function fakeClock() {
  let now = 0;
  let nextId = 1;
  const timers = new Map();
  return {
    setTimeout(callback, delay) {
      const id = nextId++;
      timers.set(id, {callback, at: now + delay});
      return id;
    },
    clearTimeout(id) { timers.delete(id); },
    advance(ms) {
      const target = now + ms;
      for (;;) {
        const next = [...timers.entries()].filter(([, timer]) => timer.at <= target)
          .sort((a, b) => a[1].at - b[1].at || a[0] - b[0])[0];
        if (!next) break;
        const [id, timer] = next;
        now = timer.at;
        timers.delete(id);
        timer.callback();
      }
      now = target;
    },
    pending() { return [...timers.values()].map(timer => timer.at - now).sort((a, b) => a - b); }
  };
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((onResolve, onReject) => { resolve = onResolve; reject = onReject; });
  return {promise, resolve, reject};
}

function runEvents(runInput, result = 'answer') {
  return [
    {type: 'RUN_STARTED', threadId: runInput.threadId, runId: runInput.runId, protocolVersion: '1.0'},
    {type: 'TEXT_MESSAGE_START', messageId: 'assistant-one', role: 'assistant'},
    {type: 'TEXT_MESSAGE_CONTENT', messageId: 'assistant-one', delta: result},
    {type: 'TEXT_MESSAGE_END', messageId: 'assistant-one'},
    {type: 'RUN_FINISHED', threadId: runInput.threadId, runId: runInput.runId,
      outcome: {type: 'success'}, result},
  ];
}

function browser({child = false, existing = false, hostPlatform = 'android', appId = 'app-one', revision = 3, pendingRecords = [], initialWakeRunId, clock, indexedDb: sharedIndexedDb} = {}) {
  const requests = [];
  const inputs = [];
  const listeners = [];
  const config = {appId, name: 'Meal', revision, runId: 'epoch-one',
    hostPlatform, services: ['api']};
  const window = eventTarget();
  window.top = child ? {} : window;
  window.isSecureContext = true;
  const indexedDb = sharedIndexedDb || fakeIndexedDb({databaseName: 'lingxi-local-app-agui-' + appId, pendingRecords});
  window.indexedDB = indexedDb;
  if (existing) window.localApp = {desktop: true};

  const location = {
    origin: 'http://127.0.0.1:24131',
    href: 'http://127.0.0.1:24131/__run/epoch-one/index.html',
    assign(url) { this.href = url; }
  };
  if (initialWakeRunId) {
    location.href += `?__lingxi_ready=${encodeURIComponent(initialWakeRunId)}`;
  }
  const document = eventTarget();
  document.visibilityState = 'visible';
  document.body = {children: [], append(inputElement) { this.children.push(inputElement); }};
  document.createElement = name => {
    assert.equal(name, 'input');
    const handlers = new Map();
    const inputElement = {
      style: {}, files: [],
      addEventListener(event, handler) { handlers.set(event, handler); },
      click() { inputElement.clicked = true; },
      remove() { inputElement.removed = true; },
      handlers
    };
    inputs.push(inputElement);
    return inputElement;
  };
  const navigator = {serviceWorker: {
    register: async (url, options) => ({url, options}),
    ready: Promise.resolve({scope: `${location.origin}/`})
  }};
  const history = {state: null, replaceState(_state, _title, url) { location.href = new URL(url, location.href).href; }};
  let fetchHandler;
  const context = vm.createContext({
    window, location, document, navigator, history, console, URL, URLSearchParams,
    CustomEvent: BrowserCustomEvent,
    indexedDB: window.indexedDB,
    File,
    crypto: webcrypto,
    TextEncoder,
    btoa: globalThis.btoa,
    AbortController,
    setTimeout: clock?.setTimeout || setTimeout,
    clearTimeout: clock?.clearTimeout || clearTimeout,
    setInterval: () => 1,
    fetch: async (url, options) => {
      const envelope = JSON.parse(options.body);
      const request = {url, options, envelope};
      requests.push(request);
      if (fetchHandler) return fetchHandler(request);
      const {method, args} = envelope;
      let value = {};
      if (method === 'agent.tasks.start' || method === 'agent.chats.send') {
        value = {runId: args.input.runId};
      } else if (method === 'agent.tasks.events') {
        const submitted = requests.find(candidate => ['agent.tasks.start', 'agent.chats.send'].includes(candidate.envelope.method));
        const runInput = submitted?.envelope.args.input || {threadId: 'thread-one', runId: args.runId};
        value = {events: runEvents(runInput, 'answer'), sequence: 5};
      } else if (method === 'agent.chats.create') {
        value = {chatId: args.requestId, title: 'Chat', createdAt: 1, updatedAt: 1, archived: false, state: 'idle'};
      } else if (method === 'agent.chats.creationStatus') {
        value = null;
      } else if (method === 'agent.chats.get') {
        value = {chatId: args.chatId, title: 'Chat', createdAt: 1, updatedAt: 1, archived: false, state: 'idle'};
      } else if (method === 'agent.chats.history') {
        value = {messages: [], nextCursor: undefined};
      }
      return {ok: true, json: async () => parseInPage(JSON.stringify({ok: true, value}))};
    }
  });
  context.structuredClone = vm.runInContext('value => JSON.parse(JSON.stringify(value))', context);
  const parseInPage = vm.runInContext('json => JSON.parse(json)', context);
  vm.runInContext(bundle, context);
  window.LingxiLocalAppAGUI = context.LingxiLocalAppAGUI;
  vm.runInContext(source.replace('__LINGXI_APP_CONFIGURATION__', JSON.stringify(config)), context);

  return {
    api: window.localApp, requests, inputs, listeners, location, window, document, history, indexedDb,
    input: value => parseInPage(JSON.stringify(value)),
    setFetchHandler(handler) { fetchHandler = handler; },
    async flush() { await new Promise(setImmediate); await new Promise(setImmediate); },
    simulateWakeReturn(runId) {
      location.href = `${location.origin}/__run/epoch-one/index.html?__lingxi_ready=${encodeURIComponent(runId)}`;
      window.dispatchEvent({type: 'pageshow'});
    }
  };
}

test('browser sends a complete official AG-UI JSON input and returns RunAgentResult', async () => {
  const b = browser();
  const photo = await b.api.agent.file(new File([new Uint8Array([0x89, 0x50, 0x4e, 0x47])], 'photo.png', {type: 'image/png'}));
  const runInput = b.input(input({content: [{id: 'user-one', role: 'user', content: [photo]}]}));
  const result = await b.api.agent.run(runInput);

  const start = b.requests.find(request => request.envelope.method === 'agent.tasks.start');
  assert.equal(start.url, 'http://127.0.0.1:24131/__run/epoch-one/__host/agent');
  assert.equal(start.options.method, 'POST');
  assert.equal(start.options.redirect, 'error');
  assert.equal(start.options.credentials, 'omit');
  assert.equal(start.options.headers['Content-Type'], 'application/json');
  assert.equal(start.options.headers['X-LingXi-App-Request'], '1');
  assert.deepEqual(JSON.parse(JSON.stringify(start.envelope)), JSON.parse(JSON.stringify({
    method: 'agent.tasks.start',
    args: {input: {
      ...runInput,
      protocolVersion: '1.0',
      state: {},
      tools: [],
      context: [],
      forwardedProps: {'com.lingxi.localApp': {version: 1}},
    }}
  })));
  assert.equal(result.result, 'answer');
  assert.equal(result.newMessages.length, 1);
  assert.equal(result.newMessages[0].role, 'assistant');
  const eventPage = await b.api.agent.tasks.events(b.input({runId: runInput.runId}));
  assert.deepEqual(JSON.parse(JSON.stringify(eventPage.events.at(-1).outcome)), {type: 'success'});
});

test('cold official client reconnect reads events without starting a model run', async () => {
  const b = browser();
  const client = b.api.agent.createClient({threadId: 'thread-one'});
  const result = await client.connectAgent({runId: 'existing-run'});
  assert.equal(result.result, 'answer');
  assert.equal(result.newMessages.length, 1);
  assert.equal(b.requests.some(request => request.envelope.method === 'agent.tasks.start'), false);
  assert.deepEqual(b.requests.map(request => request.envelope.method), ['agent.tasks.events']);
});

test('a terminal task with unreadable events reports once and stops polling', async () => {
  const clock = fakeClock();
  const b = browser({clock});
  await b.flush();
  const errors = [];
  const events = [];
  b.window.addEventListener('localApp:agent-events-error', event => errors.push(event.detail));
  b.setFetchHandler(request => {
    const {method} = request.envelope;
    if (method === 'agent.tasks.events') return {ok: true, json: async () => b.input({
      ok: false, error: {code: 'agent_events_unavailable', message: 'Events unavailable'}
    })};
    if (method === 'agent.tasks.get') return {ok: true, json: async () => b.input({
      ok: true, value: {state: 'interrupted'}
    })};
    assert.fail(`Unexpected method ${method}`);
  });
  const unsubscribe = b.api.agent.tasks.subscribe('damaged-run', event => events.push(event));
  await b.flush();
  assert.deepEqual(b.requests.map(request => request.envelope.method), ['agent.tasks.events', 'agent.tasks.get']);
  assert.equal(errors.length, 1);
  assert.equal(errors[0].code, 'agent_events_unavailable');
  assert.equal(errors[0].runId, 'damaged-run');
  assert.deepEqual(events, [], 'The SDK must not manufacture terminal AG-UI events.');
  assert.deepEqual(clock.pending(), []);
  clock.advance(10000);
  await b.flush();
  assert.equal(b.requests.length, 2);
  unsubscribe();
});

test('a running task retries event reads and later delivers real events', async () => {
  const clock = fakeClock();
  const b = browser({clock});
  await b.flush();
  const errors = [];
  const events = [];
  let eventReads = 0;
  b.window.addEventListener('localApp:agent-events-error', event => errors.push(event.detail));
  b.setFetchHandler(request => {
    if (request.envelope.method === 'agent.tasks.events') {
      eventReads++;
      return {ok: true, json: async () => b.input(eventReads === 1
        ? {ok: false, error: {code: 'agent_events_unavailable', message: 'Events unavailable'}}
        : {ok: true, value: {events: runEvents({threadId: 'thread-one', runId: 'running-run'}), sequence: 5}})};
    }
    assert.equal(request.envelope.method, 'agent.tasks.get');
    return {ok: true, json: async () => b.input({ok: true, value: {state: 'running'}})};
  });
  const unsubscribe = b.api.agent.tasks.subscribe('running-run', event => events.push(event));
  await b.flush();
  assert.equal(errors.length, 1);
  assert.deepEqual(clock.pending(), [1000]);
  clock.advance(1000);
  await b.flush();
  assert.equal(eventReads, 2);
  assert.equal(events.at(-1).type, 'RUN_FINISHED');
  assert.deepEqual(clock.pending(), []);
  unsubscribe();
});

test('temporary transport failure retries even when the task already finished', async () => {
  const clock = fakeClock();
  const b = browser({clock});
  await b.flush();
  const errors = [];
  const events = [];
  let eventReads = 0;
  b.window.addEventListener('localApp:agent-events-error', event => errors.push(event.detail));
  b.setFetchHandler(request => {
    if (request.envelope.method === 'agent.tasks.events') {
      eventReads++;
      if (eventReads === 1) throw new TypeError('temporary network failure');
      return {ok: true, json: async () => b.input({ok: true, value: {
        events: runEvents({threadId: 'thread-one', runId: 'finished-run'}), sequence: 5
      }})};
    }
    assert.equal(request.envelope.method, 'agent.tasks.get');
    return {ok: true, json: async () => b.input({ok: true, value: {state: 'succeeded'}})};
  });
  const unsubscribe = b.api.agent.tasks.subscribe('finished-run', event => events.push(event));
  await b.flush();
  assert.equal(errors.length, 1);
  assert.equal(errors[0].code, 'host_unavailable');
  assert.deepEqual(clock.pending(), [1000]);
  clock.advance(1000);
  await b.flush();
  assert.equal(events.at(-1).type, 'RUN_FINISHED');
  assert.equal(eventReads, 2);
  assert.deepEqual(clock.pending(), []);
  unsubscribe();
});

test('an unavailable task-status check does not infer a terminal state', async () => {
  const clock = fakeClock();
  const b = browser({clock});
  await b.flush();
  const errors = [];
  let statusReads = 0;
  b.window.addEventListener('localApp:agent-events-error', event => errors.push(event.detail));
  b.setFetchHandler(request => {
    if (request.envelope.method === 'agent.tasks.events') return {ok: true, json: async () => b.input({
      ok: false, error: {code: 'agent_events_unavailable', message: 'Events unavailable'}
    })};
    assert.equal(request.envelope.method, 'agent.tasks.get');
    statusReads++;
    if (statusReads === 1) throw new TypeError('status temporarily unreachable');
    return {ok: true, json: async () => b.input({ok: true, value: {state: 'failed'}})};
  });
  const events = [];
  const unsubscribe = b.api.agent.tasks.subscribe('unknown-status', event => events.push(event));
  await b.flush();
  assert.equal(errors.length, 1);
  assert.deepEqual(clock.pending(), [1000]);
  clock.advance(1000);
  await b.flush();
  assert.equal(statusReads, 2);
  assert.equal(errors.length, 2);
  assert.deepEqual(events, []);
  assert.deepEqual(clock.pending(), []);
  unsubscribe();
});

test('unsubscribe while checking task status prevents later polling and events', async () => {
  const clock = fakeClock();
  const b = browser({clock});
  await b.flush();
  const response = deferred();
  const events = [];
  b.setFetchHandler(request => {
    if (request.envelope.method === 'agent.tasks.events') return {ok: true, json: async () => b.input({
      ok: false, error: {code: 'agent_events_unavailable', message: 'Events unavailable'}
    })};
    assert.equal(request.envelope.method, 'agent.tasks.get');
    return response.promise;
  });
  const unsubscribe = b.api.agent.tasks.subscribe('cancelled-subscription', event => events.push(event));
  await waitFor(() => b.requests.some(request => request.envelope.method === 'agent.tasks.get'), 'Task status check did not start.');
  unsubscribe();
  response.resolve({ok: true, json: async () => b.input({ok: true, value: {state: 'running'}})});
  await b.flush();
  assert.deepEqual(clock.pending(), []);
  clock.advance(10000);
  await b.flush();
  assert.equal(b.requests.length, 2);
  assert.deepEqual(events, []);
});

test('read-only facade calls time out waiting for headers after five seconds', async () => {
  const cases = [
    ['agent.tasks.get', b => b.api.agent.tasks.get('run-one')],
    ['agent.tasks.list', b => b.api.agent.tasks.list()],
    ['agent.tasks.events', b => b.api.agent.tasks.events(b.input({runId: 'run-one'}))],
    ['agent.chats.get', b => b.api.agent.chats.open('chat-one')],
    ['agent.chats.list', b => b.api.agent.chats.list()],
    ['agent.chats.creationStatus', b => b.api.agent.chats.creationStatus(b.input({requestId: '11111111-1111-4111-8111-111111111111'}))],
    ['agent.chats.history', (_b, handle) => handle.history()]
  ];
  for (const [method, call] of cases) {
    const clock = fakeClock();
    const b = browser({clock});
    await b.flush();
    const handle = method === 'agent.chats.history' ? await b.api.agent.chats.open('chat-one') : null;
    const fetchResult = deferred();
    b.setFetchHandler(() => fetchResult.promise); // Ignores abort, like a stalled transport.
    const promise = call(b, handle);
    let settled = false;
    void promise.then(() => { settled = true; }, () => { settled = true; });
    const request = b.requests.at(-1);
    assert.equal(request.envelope.method, method);
    assert.deepEqual(clock.pending(), [5000]);
    clock.advance(4999);
    await b.flush();
    assert.equal(settled, false, method);
    clock.advance(1);
    await assert.rejects(promise, error => {
      assert.equal(error.code, 'host_unavailable');
      assert.equal(error.definitive, undefined);
      assert.equal(error.message, 'LingXi is unavailable or paused; open LingXi to reconnect.');
      return true;
    });
    assert.equal(request.options.signal.aborted, true, method);
    assert.deepEqual(clock.pending(), []);
    fetchResult.reject(new TypeError('late fetch failure'));
    await b.flush(); // The losing fetch rejection must remain handled.
  }
});

test('read-only deadline covers a stalled JSON body and cleans up after a late rejection', async () => {
  const clock = fakeClock();
  const b = browser({clock, hostPlatform: 'ios'});
  await b.flush();
  const initialUrl = b.location.href;
  const body = deferred();
  let bodyStarted = false;
  b.setFetchHandler(() => ({ok: true, json: () => { bodyStarted = true; return body.promise; }}));
  const promise = b.api.agent.tasks.get('run-one');
  let settled = false;
  void promise.then(() => { settled = true; }, () => { settled = true; });
  await waitFor(() => bodyStarted, 'The response body was not requested.');
  const request = b.requests.at(-1);
  assert.deepEqual(clock.pending(), [5000]);
  clock.advance(4999);
  await b.flush();
  assert.equal(settled, false);
  clock.advance(1);
  await assert.rejects(promise, error => error.code === 'host_unavailable' && error.definitive === undefined);
  assert.equal(request.options.signal.aborted, true);
  assert.deepEqual(clock.pending(), []);
  b.window.dispatchEvent({type: 'pageshow'});
  b.document.dispatchEvent({type: 'visibilitychange'});
  await b.flush();
  assert.equal(b.requests.length, 1);
  assert.equal(b.location.href, initialUrl, 'A read-only timeout must not wake LingXi.');
  body.reject(new TypeError('late body failure'));
  await b.flush();
});

test('an immediate abort rejection cannot replace the timeout error', async () => {
  for (const phase of ['headers', 'body']) {
    const clock = fakeClock();
    const b = browser({clock});
    await b.flush();
    let bodyStarted = false;
    b.setFetchHandler(request => {
      const aborted = new Promise((_, reject) => {
        request.options.signal.addEventListener('abort', () => reject(new SyntaxError('abort')), {once: true});
      });
      if (phase === 'headers') return aborted;
      return {ok: true, json: () => { bodyStarted = true; return aborted; }};
    });
    const promise = b.api.agent.tasks.get('run-one');
    if (phase === 'body') await waitFor(() => bodyStarted, 'The response body was not requested.');
    clock.advance(5000);
    await assert.rejects(promise, error => error.code === 'host_unavailable' && error.definitive === undefined);
    assert.equal(b.requests.at(-1).options.signal.aborted, true);
    assert.deepEqual(clock.pending(), []);
    await b.flush();
  }
});

test('a body completed before the deadline releases its timer without aborting', async () => {
  const clock = fakeClock();
  const b = browser({clock});
  await b.flush();
  const body = deferred();
  let bodyStarted = false;
  b.setFetchHandler(() => ({ok: true, json: () => { bodyStarted = true; return body.promise; }}));
  const promise = b.api.agent.tasks.get('run-one');
  await waitFor(() => bodyStarted, 'The response body was not requested.');
  clock.advance(4999);
  assert.deepEqual(clock.pending(), [1]);
  body.resolve(b.input({ok: true, value: {state: 'idle'}}));
  assert.equal((await promise).state, 'idle');
  assert.deepEqual(clock.pending(), []);
  clock.advance(120000);
  assert.equal(b.requests.at(-1).options.signal.aborted, false);
});

test('completed bodies and every response error release the request timer', async () => {
  const scenarios = [
    {name: 'success', response: b => ({ok: true, json: async () => b.input({ok: true, value: {state: 'idle'}})})},
    {name: 'invalid JSON', response: () => ({ok: true, json: async () => { throw new SyntaxError('bad JSON'); }}), code: 'invalid_host_response'},
    {name: 'body transport failure', response: () => ({ok: true, json: async () => { throw new TypeError('stream failed'); }}), code: 'host_unavailable'},
    {name: 'fetch transport failure', response: () => { throw new TypeError('network failed'); }, code: 'host_unavailable'},
    {name: 'HTTP error', response: () => ({ok: false}), code: 'host_unavailable', definitive: true, aborted: true},
    {name: 'host error', response: b => ({ok: true, json: async () => b.input({ok: false, error: {code: 'denied', message: 'Denied'}})}), code: 'denied', definitive: true}
  ];
  for (const scenario of scenarios) {
    const clock = fakeClock();
    const b = browser({clock});
    await b.flush();
    b.setFetchHandler(() => scenario.response(b));
    const promise = b.api.agent.tasks.get('run-one');
    if (scenario.code) {
      await assert.rejects(promise, error => {
        assert.equal(error.code, scenario.code, scenario.name);
        assert.equal(error.definitive, scenario.definitive, scenario.name);
        return true;
      });
    } else {
      assert.equal((await promise).state, 'idle');
    }
    assert.deepEqual(clock.pending(), [], scenario.name);
    assert.equal(b.requests.at(-1).options.signal.aborted, scenario.aborted || false, scenario.name);
    clock.advance(120000);
    assert.equal(b.requests.at(-1).options.signal.aborted, scenario.aborted || false, scenario.name);
  }
});

test('mutating control calls keep their 120-second budget', async () => {
  const cases = [
    ['agent.tasks.cancel', b => b.api.agent.tasks.cancel('run-one')],
    ['agent.chats.create', b => b.api.agent.chats.create(b.input({requestId: '11111111-1111-4111-8111-111111111111'}))],
    ['agent.chats.archive', b => b.api.agent.chats.archive('chat-one')],
    ['agent.chats.delete', b => b.api.agent.chats.delete('chat-one')],
    ['agent.chats.cancel', (_b, handle) => handle.cancel()]
  ];
  for (const [method, call] of cases) {
    const clock = fakeClock();
    const b = browser({clock});
    const pendingSignals = [];
    b.window.addEventListener('localApp:chat-creation-pending', event => pendingSignals.push(event.detail.requestId));
    await b.flush();
    const handle = method === 'agent.chats.cancel' ? await b.api.agent.chats.open('chat-one') : null;
    b.setFetchHandler(() => new Promise(() => {}));
    const promise = call(b, handle);
    let settled = false;
    void promise.then(() => { settled = true; }, () => { settled = true; });
    await waitFor(() => b.requests.at(-1)?.envelope.method === method, `missing ${method} request`);
    assert.deepEqual(clock.pending(), method === 'agent.chats.create' ? [5000, 120000] : [120000]);
    clock.advance(5000);
    await b.flush();
    assert.equal(settled, false, method);
    assert.deepEqual(pendingSignals, method === 'agent.chats.create' ? ['11111111-1111-4111-8111-111111111111'] : []);
    assert.deepEqual(clock.pending(), [115000]);
    clock.advance(115000);
    await assert.rejects(promise, error => error.code === 'host_unavailable'
      && error.definitive === (method === 'agent.chats.create' ? false : undefined));
    assert.equal(b.requests.at(-1).options.signal.aborted, true);
    assert.deepEqual(clock.pending(), []);
  }
});

test('Chat creation saves identity before POST and shares concurrent same-ID calls', async () => {
  const b = browser();
  const requestId = 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa';
  const options = b.input({requestId, title: '  Exact title  '});
  const reply = deferred();
  b.setFetchHandler(request => {
    assert.equal(request.envelope.method, 'agent.chats.create');
    const saved = b.indexedDb.getRecord('lingxi-local-app-chat-creates-app-one-revision-3', 'creations', requestId);
    assert.equal(saved.state, 'pending');
    assert.equal(saved.options.title, '  Exact title  ');
    return reply.promise;
  });
  const first = b.api.agent.chats.create(options);
  await waitFor(() => b.requests.length === 1, 'Chat creation never reached the host');
  const second = b.api.agent.chats.create(options);
  await assert.rejects(b.api.agent.chats.create(b.input({requestId, title: 'Exact title'})),
    error => error.code === 'chat_create_conflict');
  assert.equal(b.requests.length, 1);
  reply.resolve({ok: true, json: async () => b.input({ok: true, value: {chatId: requestId}})});
  assert.equal((await first).id, requestId);
  assert.equal((await second).id, requestId);
  assert.equal(b.requests.length, 1);
  assert.equal(b.indexedDb.getRecord('lingxi-local-app-chat-creates-app-one-revision-3', 'creations', requestId).state, 'accepted');
  assert.deepEqual(JSON.parse(JSON.stringify(await b.api.agent.chats.pendingCreations())),
    [{requestId, title: '  Exact title  ', state: 'accepted'}]);
  await b.api.agent.chats.forgetCreation(requestId);
  assert.deepEqual(JSON.parse(JSON.stringify(await b.api.agent.chats.pendingCreations())), []);
});

test('Chat creation recovery is read-only after response loss and page reopen', async () => {
  const b = browser();
  const requestId = 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb';
  const options = b.input({requestId});
  b.setFetchHandler(() => { throw new Error('response lost'); });
  await assert.rejects(b.api.agent.chats.create(options), error =>
    error.code === 'host_unavailable' && error.definitive === false && error.requestId === requestId);
  await assert.rejects(b.api.agent.chats.forgetCreation(requestId), error => error.code === 'chat_creation_unresolved');
  const reopened = browser({indexedDb: b.indexedDb});
  const saved = JSON.parse(JSON.stringify(await reopened.api.agent.chats.pendingCreations()));
  assert.deepEqual(saved, [{requestId, state: 'pending'}]);
  assert.equal(reopened.requests.length, 0, 'opening the page must not replay create');
  const recoveredOptions = reopened.input({requestId});
  assert.equal(await reopened.api.agent.chats.creationStatus(recoveredOptions), null);
  assert.deepEqual(reopened.requests.map(request => request.envelope.method), ['agent.chats.creationStatus']);
  // The caller explicitly decides to retry using the exact stored identity.
  const handle = await reopened.api.agent.chats.create(recoveredOptions);
  assert.equal(handle.id, requestId);
  assert.deepEqual(reopened.requests.map(request => request.envelope.method),
    ['agent.chats.creationStatus', 'agent.chats.create']);
});

test('Chat creation never sends when identity cannot be saved or is invalid', async () => {
  const b = browser();
  const requestId = 'cccccccc-cccc-4ccc-8ccc-cccccccccccc';
  b.indexedDb.failNextGet();
  await assert.rejects(b.api.agent.chats.create(b.input({requestId})), error => error.code === 'storage_unavailable');
  assert.equal(b.requests.length, 0);
  for (const options of [{}, {requestId: requestId.toUpperCase()}, {requestId, title: null}, {requestId, extra: 1}]) {
    await assert.rejects(b.api.agent.chats.create(b.input(options)), error => error.code === 'invalid_chat');
  }
  assert.equal(b.requests.length, 0);
});

test('Chat creation receipts are isolated by App and revision at one browser origin', async () => {
  const first = browser();
  const second = browser({appId: 'app-two', indexedDb: first.indexedDb});
  const newer = browser({appId: 'app-one', revision: 4, indexedDb: first.indexedDb});
  const requestId = 'ffffffff-ffff-4fff-8fff-ffffffffffff';
  assert.equal(first.location.origin, second.location.origin);
  await first.api.agent.chats.create(first.input({requestId, title: 'First'}));
  await second.api.agent.chats.create(second.input({requestId, title: 'Second'}));
  await newer.api.agent.chats.create(newer.input({requestId, title: 'First'}));
  assert.deepEqual(JSON.parse(JSON.stringify(await first.api.agent.chats.pendingCreations())),
    [{requestId, title: 'First', state: 'accepted'}]);
  assert.deepEqual(JSON.parse(JSON.stringify(await second.api.agent.chats.pendingCreations())),
    [{requestId, title: 'Second', state: 'accepted'}]);
  assert.deepEqual(JSON.parse(JSON.stringify(await newer.api.agent.chats.pendingCreations())),
    [{requestId, title: 'First', state: 'accepted'}]);
  await newer.api.agent.chats.forgetCreation(requestId);
  assert.equal((await first.api.agent.chats.pendingCreations()).length, 1);
  assert.equal((await second.api.agent.chats.pendingCreations()).length, 1);
});

test('only definitive Chat rejection permits forgetting a receipt', async () => {
  for (const [code, state] of [['chat_create_deleted', 'deleted'], ['chat_create_conflict', 'rejected'], ['invalid_chat', 'rejected']]) {
    const b = browser();
    const requestId = 'dddddddd-dddd-4ddd-8ddd-dddddddddddd';
    b.setFetchHandler(() => ({ok: true, json: async () => b.input({ok: false, error: {code, message: code}})}));
    await assert.rejects(b.api.agent.chats.create(b.input({requestId})), error => error.code === code);
    const receipts = JSON.parse(JSON.stringify(await b.api.agent.chats.pendingCreations()));
    assert.deepEqual(receipts, [{requestId, state}]);
    await b.api.agent.chats.forgetCreation(requestId);
    assert.equal((await b.api.agent.chats.pendingCreations()).length, 0);
  }
  const b = browser();
  const requestId = 'eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee';
  b.setFetchHandler(() => ({ok: true, json: async () => b.input({ok: false,
    error: {code: 'chat_storage_uncertain', message: 'unknown'}})}));
  await assert.rejects(b.api.agent.chats.create(b.input({requestId})), error =>
    error.code === 'chat_storage_uncertain' && error.definitive === false);
  assert.deepEqual(JSON.parse(JSON.stringify(await b.api.agent.chats.pendingCreations())), [{requestId, state: 'pending'}]);
  await assert.rejects(b.api.agent.chats.forgetCreation(requestId), error => error.code === 'chat_creation_unresolved');
});

test('large task and chat submissions preserve pending input for an explicit same-ID retry', async () => {
  for (const method of ['agent.tasks.start', 'agent.chats.send']) {
    const clock = fakeClock();
    const b = browser({clock});
    await b.flush();
    const chat = method === 'agent.chats.send' ? await b.api.agent.chats.open('chat-one') : null;
    const attachment = await b.api.agent.file(new File([new Uint8Array(1024 * 1024).fill(0x61)], 'document.txt', {type: 'text/plain'}));
    const runInput = b.input(input({
      threadId: chat ? chat.id : 'thread-one', runId: 'same-run',
      content: [{id: 'user-one', role: 'user', content: [attachment]}]
    }));
    const body = deferred();
    const initialUrl = b.location.href;
    const initialRequests = b.requests.length;
    b.setFetchHandler(() => ({ok: true, json: () => body.promise}));
    const submit = () => chat ? chat.send(runInput) : b.api.agent.tasks.start(runInput);
    const promise = submit();
    let settled = false;
    void promise.then(() => { settled = true; }, () => { settled = true; });
    await waitFor(() => b.requests.length > initialRequests, `${method} was not submitted.`);
    const request = b.requests.at(-1);
    assert.equal(request.envelope.method, method);
    assert.ok(request.options.body.length > 1024 * 1024);
    assert.deepEqual(clock.pending(), [120000]);
    clock.advance(5000);
    await b.flush();
    assert.equal(settled, false, method);
    assert.deepEqual(clock.pending(), [115000]);
    clock.advance(115000);
    await assert.rejects(promise, error => {
      assert.equal(error.code, 'host_unavailable');
      assert.equal(error.definitive, undefined);
      assert.match(error.message, /saved for an explicit retry/);
      return true;
    });
    assert.equal(request.options.signal.aborted, true);
    assert.deepEqual(clock.pending(), []);
    const key = 'app-one:3:same-run';
    const saved = b.indexedDb.getRecord('lingxi-local-app-agui-app-one', 'pending', key);
    assert.equal(saved.state, 'uncertain');
    assert.equal(saved.runId, runInput.runId);
    assert.equal(saved.threadId, runInput.threadId);
    assert.equal(saved.method, method);
    assert.ok(saved.fingerprint);
    assert.deepEqual(saved.input, request.envelope.args.input);
    b.window.dispatchEvent({type: 'pageshow'});
    b.document.dispatchEvent({type: 'visibilitychange'});
    await b.flush();
    assert.equal(b.requests.length, initialRequests + 1, 'Timeout must not automatically retry or create a task.');
    assert.equal(b.location.href, initialUrl, 'Timeout must not automatically wake LingXi.');

    b.setFetchHandler(null);
    const result = await submit();
    if (chat) assert.equal(result.result, 'answer');
    else assert.equal(result.runId, runInput.runId);
    const attempts = b.requests.filter(item => item.envelope.method === method);
    assert.equal(attempts.length, 2);
    assert.deepEqual(attempts[1].envelope, attempts[0].envelope);
    assert.equal(b.requests.filter(item => item.envelope.method === 'agent.tasks.start').length,
      method === 'agent.tasks.start' ? 2 : 0);
    const accepted = b.indexedDb.getRecord('lingxi-local-app-agui-app-one', 'pending', key);
    assert.equal(accepted.state, 'accepted');
    assert.equal(accepted.fingerprint, saved.fingerprint);
    assert.equal(accepted.runId, saved.runId);
    assert.equal(accepted.threadId, saved.threadId);
    assert.equal(b.location.href, initialUrl);
    body.reject(new TypeError('late submission body failure'));
    await b.flush();
  }
});

test('submission failures keep definitive rejections distinct from uncertain input', async () => {
  const cases = [
    {name: 'HTTP failure', response: () => ({ok: false}), code: 'host_unavailable', state: 'rejected', definitive: true},
    {name: 'host rejection', response: b => ({ok: true, json: async () => b.input({ok: false, error: {code: 'denied'}})}), code: 'denied', state: 'rejected', definitive: true},
    {name: 'invalid JSON', response: () => ({ok: true, json: async () => { throw new SyntaxError('bad JSON'); }}), code: 'invalid_host_response', state: 'uncertain'},
    {name: 'body transport failure', response: () => ({ok: true, json: async () => { throw new TypeError('stream failed'); }}), code: 'host_unavailable', state: 'uncertain'}
  ];
  for (const item of cases) {
    const clock = fakeClock();
    const b = browser({clock});
    await b.flush();
    b.setFetchHandler(() => item.response(b));
    const runInput = b.input(input({runId: 'failed-run'}));
    await assert.rejects(b.api.agent.tasks.start(runInput), error => {
      assert.equal(error.code, item.code, item.name);
      assert.equal(error.definitive, item.definitive, item.name);
      return true;
    });
    const saved = b.indexedDb.getRecord('lingxi-local-app-agui-app-one', 'pending', 'app-one:3:failed-run');
    assert.equal(saved.state, item.state, item.name);
    assert.equal(saved.runId, runInput.runId);
    assert.equal(saved.threadId, runInput.threadId);
    assert.equal(b.requests.length, 1);
    assert.deepEqual(clock.pending(), []);
    if (item.state === 'uncertain') assert.deepEqual(saved.input, b.requests[0].envelope.args.input);
  }
});

test('iOS wake stores the full RunAgentInput and retries only after runId callback', async () => {
  const b = browser({hostPlatform: 'ios'});
  const runInput = b.input(input());
  const resultPromise = b.api.agent.run(runInput);
  let failure;
  void resultPromise.catch(error => { failure = error; });
  for (let attempt = 0; attempt < 100 && !b.location.href.startsWith('lingxi://') && !failure; attempt++) {
    await new Promise(resolve => setTimeout(resolve, 5));
  }
  if (failure) throw failure;
  assert.match(b.location.href, /lingxi:\/\/apps\/app-one\?.*runId=run-one/);
  assert.equal(b.requests.length, 0);

  b.simulateWakeReturn(runInput.runId);
  const result = await resultPromise;
  const start = b.requests.find(request => request.envelope.method === 'agent.tasks.start');
  assert.equal(start.envelope.args.input.runId, 'run-one');
  assert.deepEqual(JSON.parse(JSON.stringify(start.envelope.args.input.messages)), JSON.parse(JSON.stringify(runInput.messages)));
  assert.equal(result.result, 'answer');
  assert(!b.location.href.includes('__lingxi_ready='));
});

test('iOS wake reports a missing request without posting and leaves other run waiters intact',
  {timeout: 5000}, async () => {
  const b = browser({hostPlatform: 'ios'});
  const otherInput = b.input(input({runId: 'other-run'}));
  const otherPromise = b.api.agent.run(otherInput);
  let otherSettled = false;
  void otherPromise.then(() => { otherSettled = true; }, () => { otherSettled = true; });
  await waitFor(() => new URL(b.location.href).searchParams.get('runId') === otherInput.runId,
    'Other run wake link was not requested.');

  const errors = [];
  const ready = [];
  let otherReturnRequested = false;
  b.window.addEventListener('localApp:agent-handoff-error', event => errors.push(event.detail));
  b.window.addEventListener('localApp:agent-handoff-ready', event => ready.push(event.detail));
  try {
    b.simulateWakeReturn('missing-run');
    await waitFor(() => errors.length === 1, 'A missing pending record did not dispatch the handoff error.');
    assert.equal(errors[0].code, 'pending_request_missing');
    assert.equal(errors[0].runId, 'missing-run');
    assert.match(errors[0].message, /original tab|shares its storage/);
    assert.equal(new URL(b.location.href).searchParams.get('__lingxi_ready'), 'missing-run');
    assert.equal(ready.length, 0);
    assert.equal(otherSettled, false);
    assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.start').length, 0);

    b.window.dispatchEvent({type: 'pageshow'});
    b.document.dispatchEvent({type: 'visibilitychange'});
    b.window.dispatchEvent({type: 'pageshow'});
    await b.flush();
    assert.equal(errors.length, 1);
    assert.equal(new URL(b.location.href).searchParams.get('__lingxi_ready'), 'missing-run');
    assert.equal(otherSettled, false);
    otherReturnRequested = true;
    b.simulateWakeReturn(otherInput.runId);
  } finally {
    if (!otherReturnRequested) {
      b.simulateWakeReturn(otherInput.runId);
      await otherPromise.catch(() => {});
    }
  }

  const result = await otherPromise;
  const otherStart = b.requests.filter(request => request.envelope.method === 'agent.tasks.start');
  assert.equal(otherStart.length, 1);
  assert.equal(otherStart[0].envelope.args.input.runId, otherInput.runId);
  assert.equal(result.result, 'answer');
  assert.equal(ready.length, 1);
  assert.equal(errors.length, 1);
  assert.equal(otherSettled, true);
});

test('iOS wake reports ineligible records without submitting them',
  {timeout: 5000}, async () => {
  const now = Date.now();
  const cases = [
    {label: 'pending', state: 'pending'},
    {label: 'accepted', state: 'accepted'},
    {label: 'sending', state: 'sending'},
    {label: 'uncertain', state: 'uncertain'},
    {label: 'rejected', state: 'rejected'},
    {label: 'expired', state: 'waking', createdAt: now - 25 * 60 * 60 * 1000},
    {label: 'scope-mismatched', state: 'waking', revision: 2}
  ];
  for (const item of cases) {
    const runId = `existing-${item.label}`;
    const timestamp = Date.now();
    const record = {
      key: `app-one:3:${runId}`, appId: 'app-one', revision: item.revision ?? 3, runId, state: item.state,
      createdAt: item.createdAt ?? timestamp, ...(item.state === 'accepted' ? {acceptedAt: timestamp} : {})
    };
    const b = browser({hostPlatform: 'ios', pendingRecords: [record], initialWakeRunId: runId});
    const errors = [];
    const ready = [];
    b.window.addEventListener('localApp:agent-handoff-error', event => errors.push(event.detail));
    b.window.addEventListener('localApp:agent-handoff-ready', event => ready.push(event.detail));

    await waitFor(() => !new URL(b.location.href).searchParams.has('__lingxi_ready'),
      `The ${item.label} record did not consume its ineligible marker.`);
    assert.equal(errors.length, 1, `${item.label} must report its ineligible handoff.`);
    assert.equal(errors[0].runId, runId);
    assert.equal(errors[0].code, 'pending_request_not_waking');
    assert.match(errors[0].message, /check.*status/i);
    assert.equal(ready.length, 0);
    assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.start').length, 0);
    if (item.label === 'expired') {
      assert.equal(b.indexedDb.deleteRecord('lingxi-local-app-agui-app-one', 'pending', record.key), false,
        'An expired row must still be deleted after its matching wake claim completes.');
    }
    if (item.label === 'scope-mismatched') {
      assert.equal(b.indexedDb.deleteRecord('lingxi-local-app-agui-app-one', 'pending', record.key), true,
        'A scope-mismatched row must keep its existing non-claim behavior.');
    }
  }
});

test('iOS wake rejects the original waiter if another tab changes its waking record',
  {timeout: 5000}, async () => {
  const b = browser({hostPlatform: 'ios', clock: fakeClock()});
  const runInput = b.input(input({runId: 'cross-tab-run'}));
  const promise = b.api.agent.run(runInput);
  void promise.catch(() => {});
  await waitFor(() => b.location.href.startsWith('lingxi://'), 'iOS wake link was not requested.');
  const key = 'app-one:3:cross-tab-run';
  const record = b.indexedDb.getRecord('lingxi-local-app-agui-app-one', 'pending', key);
  assert.equal(record.state, 'waking');
  b.indexedDb.setRecord('lingxi-local-app-agui-app-one', 'pending', key,
    {...record, state: 'uncertain'});
  const errors = [];
  b.window.addEventListener('localApp:agent-handoff-error', event => errors.push(event.detail));

  b.simulateWakeReturn(runInput.runId);
  await assert.rejects(promise, error => error.code === 'pending_request_not_waking');
  assert.equal(errors.length, 1);
  assert.equal(errors[0].runId, runInput.runId);
  assert.equal(errors[0].code, 'pending_request_not_waking');
  assert.equal(b.indexedDb.getRecord('lingxi-local-app-agui-app-one', 'pending', key).state, 'uncertain');
  assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.start').length, 0);
  assert.equal(new URL(b.location.href).searchParams.has('__lingxi_ready'), false);
  b.window.dispatchEvent({type: 'pageshow'});
  b.document.dispatchEvent({type: 'visibilitychange'});
  await b.flush();
  assert.equal(errors.length, 1);
  assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.start').length, 0);
});

test('iOS wake reconnects an accepted cross-tab run only when its full identity matches',
  {timeout: 5000}, async () => {
  const cases = [
    {label: 'matching'},
    {label: 'fingerprint', changes: {fingerprint: '0'.repeat(64)}},
    {label: 'missing fingerprint', changes: {fingerprint: undefined}},
    {label: 'method', changes: {method: 'agent.chats.send'}},
    {label: 'thread', changes: {threadId: 'another-thread'}},
    {label: 'app', changes: {appId: 'another-app'}},
    {label: 'revision', changes: {revision: 4}},
    {label: 'run', changes: {runId: 'another-run'}}
  ];
  for (const item of cases) {
    const b = browser({hostPlatform: 'ios', clock: fakeClock()});
    const runInput = b.input(input({runId: 'cross-tab-accepted'}));
    const resultPromise = b.api.agent.run(runInput);
    void resultPromise.catch(() => {});
    await waitFor(() => b.location.href.startsWith('lingxi://'), 'iOS wake link was not requested.');
    const key = 'app-one:3:cross-tab-accepted';
    const original = b.indexedDb.getRecord('lingxi-local-app-agui-app-one', 'pending', key);
    assert.equal(original.state, 'waking');
    const receipt = {
      key: original.key, appId: original.appId, revision: original.revision,
      runId: original.runId, threadId: original.threadId, method: original.method,
      fingerprint: original.fingerprint, state: 'accepted', acceptedAt: Date.now(),
      ...item.changes
    };
    b.indexedDb.setRecord('lingxi-local-app-agui-app-one', 'pending', key, receipt);
    const errors = [];
    const ready = [];
    b.window.addEventListener('localApp:agent-handoff-error', event => errors.push(event.detail));
    b.window.addEventListener('localApp:agent-handoff-ready', event => ready.push(event.detail));

    b.simulateWakeReturn(runInput.runId);
    if (!item.changes) {
      const result = await resultPromise;
      assert.equal(result.result, 'answer');
      assert.equal(result.newMessages[0].content, 'answer');
      assert.equal(errors.length, 0);
      assert.equal(ready.length, 1);
      assert.equal(ready[0].runId, runInput.runId);
      assert.ok(b.requests.some(request => request.envelope.method === 'agent.tasks.events'),
        'The real SDK must read the accepted run events.');
    } else {
      await assert.rejects(resultPromise, error => error.code === 'pending_request_not_waking', item.label);
      assert.equal(errors.length, 1, item.label);
      assert.equal(ready.length, 0, item.label);
      assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.events').length, 0, item.label);
    }
    assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.start').length, 0, item.label);
    assert.equal(new URL(b.location.href).searchParams.has('__lingxi_ready'), false, item.label);
    assert.equal(b.indexedDb.getRecord('lingxi-local-app-agui-app-one', 'pending', key).state, 'accepted', item.label);
  }
});

test('iOS offline recovery offers native review only for waking records',
  {timeout: 5000}, async () => {
  const config = {appId: 'app-one', revision: 3, hostPlatform: 'ios'};
  const workerListeners = new Map();
  let recoveryHtml;
  const self = {
    location: {origin: 'http://127.0.0.1:24131'},
    addEventListener(type, listener) { workerListeners.set(type, listener); },
    skipWaiting: async () => {}, clients: {claim: async () => {}}
  };
  const caches = {open: async () => ({put: async (_url, response) => { recoveryHtml = await response.text(); }})};
  vm.runInNewContext(recoverySource.replaceAll('__LINGXI_RECOVERY_CONFIGURATION__', JSON.stringify(config)),
    {self, caches, URL, Response});
  let installation;
  workerListeners.get('install')({waitUntil(promise) { installation = promise; }});
  await installation;
  const pageScript = recoveryHtml.match(/<script>([\s\S]*?)<\/script>/u)?.[1];
  assert.ok(pageScript, 'The actual recovery response must contain its page script.');

  const now = Date.now();
  const record = (state, options = {}) => ({
    key: `app-one:3:${state}`, appId: 'app-one', revision: 3, runId: state,
    state, createdAt: now, input: {secret: 'private input must never be rendered'}, ...options
  });
  const indexedDb = fakeIndexedDb({pendingRecords: [
    ...['pending', 'sending', 'uncertain', 'waking', 'rejected'].map(state => record(state)),
    record('accepted', {acceptedAt: now}),
    record('foreign', {appId: 'other-app'}),
    record('expired', {createdAt: now - 25 * 60 * 60 * 1000}),
    record('waking', {key: 'app-one:3:different-key', runId: 'wrong-key'}),
    record('waking', {key: `app-one:3:${'☕'.repeat(43)}`, runId: '☕'.repeat(43)}),
    record('waking', {key: 'app-one:3:\ud800', runId: '\ud800'})
  ]});
  function element(tag) {
    return {tag, children: [], textContent: '', append(...children) { this.children.push(...children); }};
  }
  const nodes = new Map([
    ['#recovery-diagnostic', {dataset: {reason: '', readyMarker: 'unavailable'}}],
    ...['return', 'recovery-app-id', 'recovery-revision', 'recovery-reason',
      'recovery-ready-marker', 'runs', 'status'].map(name => [`#${name}`, element(name)])
  ]);
  let fetches = 0;
  const document = {
    querySelector: selector => nodes.get(selector),
    createElement: tag => element(tag)
  };
  vm.runInNewContext(pageScript, {
    document, indexedDB: indexedDb, URLSearchParams, TextEncoder,
    fetch() { fetches++; throw new Error('Recovery must not send a request.'); }
  });
  await waitFor(() => nodes.get('#status').textContent.includes('run ID(s)'), 'Recovery did not render its saved runs.');
  const rows = nodes.get('#runs').children;
  assert.deepEqual(rows.map(row => row.children[0].textContent).sort(),
    ['pending', 'sending', 'uncertain', 'waking', 'wrong-key', '☕'.repeat(43), '\ud800'].sort());
  for (const row of rows) {
    const state = row.children[0].textContent;
    const links = row.children.filter(child => child && typeof child === 'object' && child.tag === 'a');
    if (state === 'waking') {
      assert.equal(links.length, 1);
      const link = new URL(links[0].href);
      assert.equal(link.protocol, 'lingxi:');
      assert.equal(link.searchParams.get('review'), 'agent');
      assert.equal(link.searchParams.get('runId'), state);
      assert.equal(link.searchParams.get('revision'), '3');
    } else {
      assert.equal(links.length, 0, `${state} must not start an unusable native handoff.`);
      assert.match(row.children.filter(child => typeof child === 'string').join(' '), /check.*status/i);
    }
  }
  assert.equal(fetches, 0, 'Rendering recovery must never send or retry the model request.');
  assert.doesNotMatch(JSON.stringify(rows), /private input must never be rendered/u);
});

test('iOS wake preserves its marker across an IDB failure, coalesces retries, and keeps current URL state',
  {timeout: 5000}, async () => {
  const b = browser({hostPlatform: 'ios'});
  const runInput = b.input(input());
  const resultPromise = b.api.agent.run(runInput);
  void resultPromise.catch(() => {});
  await waitFor(() => b.location.href.startsWith('lingxi://'), 'iOS wake link was not requested.');

  const errors = [];
  const ready = [];
  b.window.addEventListener('localApp:agent-handoff-error', event => errors.push(event.detail));
  b.window.addEventListener('localApp:agent-handoff-ready', event => ready.push(event.detail));

  const key = 'app-one:3:run-one';
  const getsBeforeRestore = b.indexedDb.getCount(key);
  b.indexedDb.failNextGet();
  b.simulateWakeReturn(runInput.runId);
  await waitFor(() => errors.length === 1, 'IDB claim failure did not dispatch the handoff error.');

  assert.equal(errors[0].code, 'storage_unavailable');
  assert.equal(errors[0].runId, runInput.runId);
  assert.equal(new URL(b.location.href).searchParams.get('__lingxi_ready'), runInput.runId);
  assert.equal(b.indexedDb.getCount(key), getsBeforeRestore + 1);
  assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.start').length, 0);

  const gate = b.indexedDb.holdNextGet();
  b.window.dispatchEvent({type: 'pageshow'});
  b.document.dispatchEvent({type: 'visibilitychange'});
  b.window.dispatchEvent({type: 'pageshow'});
  await gate.started;
  assert.equal(b.indexedDb.getCount(key), getsBeforeRestore + 2);

  b.history.replaceState(null, '', '/__run/epoch-one/other.html?keep=preserved&__lingxi_ready=run-one#return');
  b.window.dispatchEvent({type: 'pageshow'});
  b.document.dispatchEvent({type: 'visibilitychange'});
  await b.flush();
  assert.equal(b.indexedDb.getCount(key), getsBeforeRestore + 2);
  gate.release();

  const result = await resultPromise;
  const starts = b.requests.filter(request => request.envelope.method === 'agent.tasks.start');
  assert.equal(starts.length, 1);
  assert.equal(starts[0].envelope.args.input.runId, 'run-one');
  assert.deepEqual(JSON.parse(JSON.stringify(starts[0].envelope.args.input.messages)),
    JSON.parse(JSON.stringify(runInput.messages)));
  assert.equal(result.result, 'answer');
  assert.equal(ready.length, 1);
  assert.equal(errors.length, 1);

  const currentUrl = new URL(b.location.href);
  assert.equal(currentUrl.searchParams.has('__lingxi_ready'), false);
  assert.equal(currentUrl.searchParams.get('keep'), 'preserved');
  assert.equal(currentUrl.pathname, '/__run/epoch-one/other.html');
  assert.equal(currentUrl.hash, '#return');
  b.window.dispatchEvent({type: 'pageshow'});
  b.document.dispatchEvent({type: 'visibilitychange'});
  await b.flush();
  assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.start').length, 1);
});

test('file picker and run-scoped service helpers remain available', async () => {
  const b = browser();
  assert.equal(await b.api.serviceSocketUrl('api', '/events?q=1'),
    'ws://127.0.0.1:24131/__run/epoch-one/__service/api/events?q=1');
  for (const path of ['/../secret', '/%2e%2e/secret', '/%2fsecret', '//remote', '/x#fragment', '/%00']) {
    assert.throws(() => b.api.serviceUrl('api', path));
  }
  assert.throws(() => b.api.serviceUrl('other'));

  const cancelled = b.api.files.pick(b.input({accept: 'image/*'}));
  assert.equal(b.inputs.length, 1);
  assert.equal(b.inputs[0].type, 'file');
  assert.equal(b.inputs[0].accept, 'image/*');
  assert.equal(b.inputs[0].clicked, true);
  b.inputs[0].handlers.get('cancel')();
  assert.deepEqual(Array.from(await cancelled), []);
  assert.equal(b.inputs[0].removed, true);
  assert.equal(b.requests.length, 0);
});

test('host preexisting SDK and child frame remain untouched', () => {
  assert.equal(browser({existing: true}).api.desktop, true);
  assert.equal(browser({child: true}).api, undefined);
});

test('dynamic ES module import exposes the official SDK before the browser host adapter', () => {
  const fixture = fileURLToPath(new URL('./fixtures/local_app_browser_sdk_module.mjs', import.meta.url));
  const repositoryRoot = fileURLToPath(new URL('../../', import.meta.url));
  const result = spawnSync(process.execPath, [fixture], {
    cwd: repositoryRoot,
    encoding: 'utf8',
    timeout: 15000,
  });

  assert.equal(result.error, undefined, result.error?.message);
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /module-mode browser SDK bootstrap initialized localApp, File, and fresh client APIs/);
});

test('offline recovery uses only the new AG-UI runId store and callback', () => {
  assert.match(source, /lingxi-local-app-agui-/);
  assert.match(source, /url\.searchParams\.getAll\('__lingxi_ready'\)/);
  assert.match(recoverySource, /lingxi-local-app-agui-/);
  assert.match(recoverySource, /runId:record\.runId/);
  // Chat creation has its own requestId; Agent run recovery still uses runId.
  assert.doesNotMatch(source, /FormData|multipart/);
  assert.doesNotMatch(recoverySource, /requestId|lingxi-local-app-agent-/);
});
