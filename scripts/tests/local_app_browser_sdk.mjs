import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../../crates/runtime/src/mobile/unified_local_apps/browser_sdk.js', import.meta.url), 'utf8');

function fakeIndexedDb() {
  const databases = new Map();

  function transactionFor(stores, storeName) {
    let pending = 0;
    let completed = false;
    const transaction = {oncomplete: null, onerror: null, onabort: null};
    const maybeComplete = () => {
      if (pending !== 0 || completed) return;
      setImmediate(() => {
        if (pending !== 0 || completed) return;
        completed = true;
        transaction.oncomplete?.();
      });
    };
    const schedule = (request, operation) => {
      pending++;
      setImmediate(() => {
        try {
          request.result = operation();
          request.onsuccess?.();
        } catch (error) {
          request.error = error;
          request.onerror?.();
          transaction.onerror?.();
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
          return schedule({}, () => records.get(key));
        },
        add(value) {
          return schedule({}, () => {
            if (records.has(value.key)) throw new Error('Key already exists');
            records.set(value.key, value);
            return value;
          });
        },
        put(value) {
          return schedule({}, () => {
            records.set(value.key, value);
            return value;
          });
        },
        delete(key) {
          return schedule({}, () => records.delete(key));
        },
        openCursor() {
          const request = {};
          const keys = [...records.keys()];
          let index = 0;
          const deliver = () => schedule(request, () => {
            if (index >= keys.length) return null;
            const key = keys[index];
            const value = records.get(key);
            return {
              value,
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

class BrowserFormData {
  constructor() { this.parts = []; }
  append(name, value, filename) { this.parts.push({name, value, filename}); }
  get(name) { return this.parts.find(part => part.name === name)?.value ?? null; }
}

function browser({child = false, existing = false} = {}) {
  const requests = [];
  const timers = new Map();
  const intervals = [];
  const inputs = [];
  const events = [];
  let nextTimerId = 1;
  let task = {runId: 'task-one', state: 'succeeded', sequence: 2, updatedAt: 10,
    result: {text: 'final reply', attachments: []}, usage: {}};
  let failure = false;
  const config = {appId: 'app-one', name: 'Meal', revision: 3, runId: 'epoch-one',
    hostPlatform: 'android', services: ['api']};
  const window = eventTarget();
  window.top = child ? {} : window;
  window.isSecureContext = true;
  window.indexedDB = fakeIndexedDb();
  if (existing) window.localApp = {desktop: true};

  const location = {
    origin: 'http://127.0.0.1:24131',
    href: 'http://127.0.0.1:24131/__run/epoch-one/index.html',
    assign(url) { this.href = url; }
  };
  const document = eventTarget();
  document.visibilityState = 'visible';
  document.body = {children: [], append(input) { this.children.push(input); }};
  document.createElement = name => {
    assert.equal(name, 'input');
    const listeners = new Map();
    const input = {
      style: {}, files: [],
      addEventListener(event, handler) { listeners.set(event, handler); },
      click() { input.clicked = true; },
      remove() { input.removed = true; },
      listeners
    };
    inputs.push(input);
    return input;
  };

  const navigator = {serviceWorker: {
    register: async (url, options) => ({url, options}),
    ready: Promise.resolve({scope: `${location.origin}/`})
  }};
  const history = {state: null, replaceState(_state, _title, url) { location.href = new URL(url, location.href).href; }};
  let parseInPage;
  const context = vm.createContext({
    window, location, document, navigator, history, console, URL,
    CustomEvent: BrowserCustomEvent,
    indexedDB: window.indexedDB,
    File: globalThis.File,
    FormData: BrowserFormData,
    crypto: globalThis.crypto,
    TextEncoder,
    AbortController,
    setTimeout(fn) { const id = nextTimerId++; timers.set(id, fn); return id; },
    clearTimeout(id) { timers.delete(id); },
    setInterval(fn) { intervals.push(fn); return intervals.length; },
    fetch: async (url, options) => {
      const request = {url, options};
      requests.push(request);
      if (failure) throw new Error('offline');
      const envelope = JSON.parse(options.body.get('request'));
      const value = envelope.method === 'agent.tasks.start'
        ? {runId: 'task-one'}
        : envelope.method === 'agent.tasks.get'
          ? task
          : {};
      return {ok: true, json: async () => parseInPage(JSON.stringify({ok: true, value}))};
    }
  });
  parseInPage = vm.runInContext('json => JSON.parse(json)', context);
  vm.runInContext(source.replace('__LINGXI_APP_CONFIGURATION__', JSON.stringify(config)), context);

  return {
    api: window.localApp, requests, timers, intervals, inputs, events, location,
    input: value => parseInPage(JSON.stringify(value)),
    result: value => { task = value; },
    offline: value => { failure = value; },
    async runNextTimer() {
      const [id, callback] = timers.entries().next().value || [];
      if (!callback) throw new Error('No browser timer is pending.');
      timers.delete(id);
      callback();
      await new Promise(setImmediate);
      await new Promise(setImmediate);
    }
  };
}

test('browser transport fixes app/run route and exposes no host authorization methods', async () => {
  const b = browser();
  const reply = await b.api.agent.run(b.input({requestId: 'request-one', prompt: 'describe'}));
  assert.equal(b.requests[0].url, 'http://127.0.0.1:24131/__run/epoch-one/__host/agent');
  assert.equal(b.requests[0].options.method, 'POST');
  assert.equal(b.requests[0].options.redirect, 'error');
  assert.equal(b.requests[0].options.credentials, 'omit');
  assert.equal(b.requests[0].options.headers['X-LingXi-App-Request'], '1');
  assert(b.requests[0].options.body instanceof BrowserFormData);
  assert.deepEqual(JSON.parse(b.requests[0].options.body.get('request')),
    {method: 'agent.tasks.start', args: {input: {prompt: 'describe', requestId: 'request-one'}}});
  assert.deepEqual(JSON.parse(JSON.stringify(reply)),
    {runId: 'task-one', text: 'final reply', attachments: [], usage: {}});
  assert.equal((await b.api.info()).revision, 3);
  assert.deepEqual(Object.keys(b.api.host), ['open']);
  assert.equal(b.api.agent.answer, undefined);
  b.api.host.open();
  assert.equal(b.location.href, 'lingxi://apps/app-one?review=agent');
});

test('unknown service, traversal and encoded separators are rejected before network', async () => {
  const b = browser();
  assert.equal(await b.api.serviceSocketUrl('api', '/events?q=1'),
    'ws://127.0.0.1:24131/__run/epoch-one/__service/api/events?q=1');
  for (const path of ['/../secret', '/%2e%2e/secret', '/%2fsecret', '//remote', '/x#fragment', '/%00']) {
    assert.throws(() => b.api.serviceUrl('api', path));
  }
  assert.throws(() => b.api.serviceUrl('other'));
  assert.equal(b.requests.length, 0);
  assert.equal(b.intervals.length, 1);
});

test('offline subscription reports host state and reconnects without resubmitting a task', async () => {
  const b = browser();
  const events = [];
  b.offline(true);
  const stop = b.api.agent.tasks.subscribe('task-one', event => events.push(event));
  await new Promise(setImmediate);
  assert.equal(events[0].type, 'connection');
  assert.equal(events[0].error.code, 'host_unavailable');
  b.offline(false);
  await b.runNextTimer();
  assert.equal(events[1].task.state, 'succeeded');
  stop();
  assert.equal(b.timers.size, 0);
  assert(b.requests.every(request => JSON.parse(request.options.body.get('request')).method === 'agent.tasks.get'));
});

test('browser file picker returns File objects and cancellation removes only its own input', async () => {
  const b = browser();
  const cancelled = b.api.files.pick(b.input({accept: 'image/*'}));
  assert.equal(b.inputs.length, 1);
  assert.equal(b.inputs[0].type, 'file');
  assert.equal(b.inputs[0].accept, 'image/*');
  assert.equal(b.inputs[0].clicked, true);
  b.inputs[0].listeners.get('cancel')();
  assert.deepEqual(Array.from(await cancelled), []);
  assert.equal(b.inputs[0].removed, true);

  const selected = b.api.files.pick(b.input({multiple: true, accept: ['image/*', '.pdf']}));
  const file = new File([new Uint8Array([0, 255, 1])], 'meal.png', {type: 'image/png'});
  b.inputs[1].files = [file];
  b.inputs[1].listeners.get('change')();
  const files = await selected;
  assert.equal(files.length, 1);
  assert.equal(files[0], file);
  assert(files[0] instanceof File);
  assert.equal(b.inputs[1].accept, 'image/*,.pdf');
  assert.equal(b.inputs[1].removed, true);
  assert.equal(b.requests.length, 0);
});

test('native Desktop SDK is preserved and child frames receive no host SDK', () => {
  assert.equal(browser({existing: true}).api.desktop, true);
  assert.equal(browser({child: true}).api, undefined);
});
