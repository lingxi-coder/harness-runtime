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

function browser({child = false, existing = false, hostPlatform = 'android', pendingRecords = [], initialWakeRunId, clock} = {}) {
  const requests = [];
  const inputs = [];
  const listeners = [];
  const config = {appId: 'app-one', name: 'Meal', revision: 3, runId: 'epoch-one',
    hostPlatform, services: ['api']};
  const window = eventTarget();
  window.top = child ? {} : window;
  window.isSecureContext = true;
  const indexedDb = fakeIndexedDb({pendingRecords});
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
        value = {chatId: 'chat-one', title: 'Chat', createdAt: 1, updatedAt: 1, archived: false, state: 'idle'};
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

test('all six read-only facade calls time out waiting for headers after five seconds', async () => {
  const cases = [
    ['agent.tasks.get', b => b.api.agent.tasks.get('run-one')],
    ['agent.tasks.list', b => b.api.agent.tasks.list()],
    ['agent.tasks.events', b => b.api.agent.tasks.events(b.input({runId: 'run-one'}))],
    ['agent.chats.get', b => b.api.agent.chats.open('chat-one')],
    ['agent.chats.list', b => b.api.agent.chats.list()],
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
    ['agent.chats.create', b => b.api.agent.chats.create()],
    ['agent.chats.archive', b => b.api.agent.chats.archive('chat-one')],
    ['agent.chats.delete', b => b.api.agent.chats.delete('chat-one')],
    ['agent.chats.cancel', (_b, handle) => handle.cancel()]
  ];
  for (const [method, call] of cases) {
    const clock = fakeClock();
    const b = browser({clock});
    await b.flush();
    const handle = method === 'agent.chats.cancel' ? await b.api.agent.chats.open('chat-one') : null;
    b.setFetchHandler(() => new Promise(() => {}));
    const promise = call(b, handle);
    let settled = false;
    void promise.then(() => { settled = true; }, () => { settled = true; });
    assert.equal(b.requests.at(-1).envelope.method, method);
    assert.deepEqual(clock.pending(), [120000]);
    clock.advance(5000);
    await b.flush();
    assert.equal(settled, false, method);
    assert.deepEqual(clock.pending(), [115000]);
    clock.advance(115000);
    await assert.rejects(promise, error => error.code === 'host_unavailable' && error.definitive === undefined);
    assert.equal(b.requests.at(-1).options.signal.aborted, true);
    assert.deepEqual(clock.pending(), []);
  }
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

test('iOS wake preserves existing terminal, expired, and scope-mismatched record behavior',
  {timeout: 5000}, async () => {
  const now = Date.now();
  const cases = [
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
      `The ${item.label} record did not preserve its existing marker semantics.`);
    assert.equal(errors.length, 0, `${item.label} must not be misreported as missing.`);
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
  assert.doesNotMatch(source, /requestId|FormData|multipart/);
  assert.doesNotMatch(recoverySource, /requestId|lingxi-local-app-agent-/);
});
