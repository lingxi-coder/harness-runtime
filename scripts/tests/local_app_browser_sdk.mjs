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

function fakeIndexedDb() {
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
    open(name) {
      const request = {};
      setImmediate(() => {
        let db = databases.get(name);
        const upgrading = !db;
        if (!db) {
          const stores = new Map();
          db = {
            get objectStoreNames() { return {contains: storeName => stores.has(storeName)}; },
            createObjectStore(storeName) { stores.set(storeName, new Map()); },
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

function browser({child = false, existing = false, hostPlatform = 'android'} = {}) {
  const requests = [];
  const inputs = [];
  const listeners = [];
  const config = {appId: 'app-one', name: 'Meal', revision: 3, runId: 'epoch-one',
    hostPlatform, services: ['api']};
  const window = eventTarget();
  window.top = child ? {} : window;
  window.isSecureContext = true;
  const indexedDb = fakeIndexedDb();
  window.indexedDB = indexedDb;
  if (existing) window.localApp = {desktop: true};

  const location = {
    origin: 'http://127.0.0.1:24131',
    href: 'http://127.0.0.1:24131/__run/epoch-one/index.html',
    assign(url) { this.href = url; }
  };
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
  const context = vm.createContext({
    window, location, document, navigator, history, console, URL, URLSearchParams,
    CustomEvent: BrowserCustomEvent,
    indexedDB: window.indexedDB,
    File,
    crypto: webcrypto,
    TextEncoder,
    btoa: globalThis.btoa,
    AbortController,
    setTimeout,
    clearTimeout,
    setInterval: () => 1,
    fetch: async (url, options) => {
      const envelope = JSON.parse(options.body);
      const request = {url, options, envelope};
      requests.push(request);
      const {method, args} = envelope;
      let value = {};
      if (method === 'agent.tasks.start') {
        value = {runId: args.input.runId};
      } else if (method === 'agent.tasks.events') {
        const submitted = requests.find(candidate => candidate.envelope.method === 'agent.tasks.start');
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
  b.simulateWakeReturn('not-the-pending-run');
  await waitFor(() => !new URL(b.location.href).searchParams.has('__lingxi_ready'),
    'A missing pending record did not consume its wake marker.');
  assert.equal(errors.length, 0);
  assert.equal(ready.length, 0);
  assert.equal(b.requests.filter(request => request.envelope.method === 'agent.tasks.start').length, 0);

  const key = 'app-one:3:run-one';
  const getsBeforeRestore = b.indexedDb.getCount(key);
  b.indexedDb.failNextGet();
  b.simulateWakeReturn(runInput.runId);
  await waitFor(() => errors.length === 1, 'IDB claim failure did not dispatch the handoff error.');

  assert.equal(errors[0].code, 'storage_unavailable');
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
