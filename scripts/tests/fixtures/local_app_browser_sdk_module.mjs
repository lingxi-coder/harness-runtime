import assert from 'node:assert/strict';
import {File} from 'node:buffer';
import {readFileSync} from 'node:fs';

const bundle = readFileSync('packages/local-app-agui/dist/browser.global.js', 'utf8');
const adapter = readFileSync('crates/runtime/src/mobile/unified_local_apps/browser_sdk.js', 'utf8');
const config = {
  appId: 'module-mode-app',
  name: 'Module mode fixture',
  revision: 1,
  runId: 'module-mode-run',
  hostPlatform: 'android',
  services: [],
};

function eventTarget() {
  const listeners = new Map();
  return {
    addEventListener(type, listener) {
      const rows = listeners.get(type) || new Set();
      rows.add(listener);
      listeners.set(type, rows);
    },
    dispatchEvent(event) {
      for (const listener of listeners.get(event.type) || []) listener.call(this, event);
      return true;
    },
  };
}

class BrowserCustomEvent {
  constructor(type, options = {}) {
    this.type = type;
    this.detail = options.detail;
  }
}

const windowEvents = eventTarget();
const window = globalThis;
window.top = window;
window.window = window;
window.isSecureContext = false;
window.addEventListener = windowEvents.addEventListener.bind(windowEvents);
window.dispatchEvent = windowEvents.dispatchEvent.bind(windowEvents);
window.location = {
  origin: 'http://127.0.0.1:24321',
  href: 'http://127.0.0.1:24321/__run/module-mode-run/index.html',
  assign(url) { this.href = url; },
};
window.document = eventTarget();
window.document.visibilityState = 'visible';
window.document.body = {append() {}};
window.document.createElement = () => { throw new Error('The module bootstrap test does not open a file picker.'); };
window.history = {state: null, replaceState() {}};
window.File = File;
window.CustomEvent = BrowserCustomEvent;
window.TextEncoder = TextEncoder;
window.AbortController = AbortController;
window.setInterval = () => 0;
window.fetch = async () => { throw new Error('The module bootstrap test must not contact a host.'); };
if (typeof window.btoa !== 'function') {
  window.btoa = value => Buffer.from(value, 'binary').toString('base64');
}

const browserSdk = adapter.replace('__LINGXI_APP_CONFIGURATION__', JSON.stringify(config));
const script = bundle
  + '\n;\nglobalThis.LingxiLocalAppAGUI = LingxiLocalAppAGUI;\n;\n'
  + browserSdk;
const dataUrl = 'data:text/javascript;base64,' + Buffer.from(script).toString('base64');
await import(dataUrl);

assert.equal(window.LingxiLocalAppAGUI?.installBrowserAgent instanceof Function, true);
assert.equal(window.localApp?.info instanceof Function, true);
assert.equal(window.localApp?.agent?.run instanceof Function, true);
assert.equal(window.localApp?.agent?.createClient instanceof Function, true);
assert.equal(window.localApp?.agent?.file instanceof Function, true);

const part = await window.localApp.agent.file(new File(['module File content'], 'module-note.txt', {type: 'text/plain'}));
assert.equal(part.type, 'document');
assert.equal(part.source.type, 'data');
assert.equal(part.metadata['com.lingxi.localApp'].filename, 'module-note.txt');

const client = window.localApp.agent.createClient({threadId: 'module-fresh-client'});
assert.equal(client.threadId, 'module-fresh-client');
assert.equal(client.addMessage instanceof Function, true);
assert.equal(client.runAgent instanceof Function, true);
assert.equal(client.connectAgent instanceof Function, true);
client.addMessage({id: 'module-user-message', role: 'user', content: [{type: 'text', text: 'bootstrap is ready'}]});
assert.equal(client.messages.length, 1);
assert.equal(client.messages[0].id, 'module-user-message');

process.stdout.write('module-mode browser SDK bootstrap initialized localApp, File, and fresh client APIs\n');
