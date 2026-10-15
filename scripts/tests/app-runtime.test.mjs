import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import test from 'node:test';

const source = readFileSync(new URL('../../crates/visualization/assets/app-runtime.js', import.meta.url), 'utf8');
const plain = value => JSON.parse(JSON.stringify(value));

function launch(storage = new Map(), seed = null) {
  const events = new Map();
  const media = { matches: false, addEventListener: (_, fn) => events.set('theme', fn) };
  const document = {
    documentElement: { dataset: {}, lang: '' }, visibilityState: 'visible',
    addEventListener: (name, fn) => events.set(name, fn),
  };
  const localStorage = {
    getItem: key => storage.get(key) ?? null,
    setItem: (key, value) => storage.set(key, value),
  };
  const window = {
    __lingxiAppBoot: seed, matchMedia: () => media,
    addEventListener: (name, fn) => events.set(name, fn),
  };
  runInNewContext(source, { window, document, localStorage, navigator: { language: 'zh-CN' }, TextEncoder, queueMicrotask, console });
  return { api: window.lingxi, events, document, media, storage, localStorage };
}

test('seed is synchronous and persists independently across launches and code updates', async () => {
  const seed = { version: 4, modelContent: { count: 2 }, privateContent: 'private' };
  const first = launch(new Map(), seed);
  assert.deepEqual(plain(first.api.state), seed);
  const snapshot = first.api.state;
  snapshot.modelContent.count = 99;
  assert.equal(first.api.state.modelContent.count, 2);
  let notified;
  first.api.onState(state => { notified = plain(state); });
  await first.api.saveState({ modelContent: { count: 3 } });
  assert.equal(notified.modelContent.count, 3);
  assert.equal(notified.privateContent, 'private');
  assert.deepEqual(plain(launch(first.storage, { version: 0 }).api.state), notified);
  assert.equal(launch().api.state.modelContent, null);
});

test('failed writes and invalid or oversized state never advance confirmed state', async () => {
  const page = launch();
  await assert.rejects(page.api.saveState({ privateContent: '字'.repeat(6000) }), /16 KiB/);
  const cyclic = {}; cyclic.self = cyclic;
  await assert.rejects(page.api.saveState({ modelContent: cyclic }), /JSON-serializable/);
  page.localStorage.setItem = () => { throw new Error('quota'); };
  await assert.rejects(page.api.saveState({ modelContent: 1 }), /quota/);
  assert.equal(page.api.state.version, 0);
});

test('theme, locale and suspend follow the page lifecycle without duplicate suspension', () => {
  const page = launch();
  assert.equal(page.api.locale, 'zh-CN');
  assert.equal(page.api.expanded, true);
  assert.throws(() => page.api.draftFollowup('x'), /conversation/);
  page.media.matches = true;
  page.events.get('theme')();
  assert.equal(page.api.theme.mode, 'dark');
  assert.equal(page.document.documentElement.dataset.theme, 'dark');
  let suspends = 0;
  page.api.onSuspend(() => { suspends++; void page.api.saveState({ modelContent: suspends }); });
  page.document.visibilityState = 'hidden';
  page.events.get('visibilitychange')();
  page.events.get('pagehide')();
  assert.equal(suspends, 1);
  assert.equal(page.api.state.modelContent, 1);
  page.events.get('pageshow')();
  page.events.get('pagehide')();
  assert.equal(suspends, 2);
});
