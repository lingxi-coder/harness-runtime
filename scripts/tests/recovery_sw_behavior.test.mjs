import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const recoverySource = readFileSync(new URL('../../crates/runtime/src/mobile/unified_local_apps/recovery_sw.js', import.meta.url), 'utf8');
const recoveryConfig = {appId: 'diagnostic-app', revision: 17, hostPlatform: 'ios'};
const origin = 'https://local-app.example';
const recoveryUrl = `${origin}/__lingxi-offline-recovery`;

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

function trackedSignal() {
  const controller = new AbortController();
  const signal = controller.signal;
  const active = new Set();
  const add = signal.addEventListener.bind(signal);
  const remove = signal.removeEventListener.bind(signal);
  signal.addEventListener = (type, listener, options) => {
    if (type === 'abort') active.add(listener);
    add(type, listener, options);
  };
  signal.removeEventListener = (type, listener, options) => {
    if (type === 'abort') active.delete(listener);
    remove(type, listener, options);
  };
  return {controller, signal, listenerCount: () => active.size};
}

function createWorker(fetchImpl = async () => new Response('upstream ok', {status: 200}), {clock} = {}) {
  const listeners = new Map();
  const cached = new Map();
  const cache = {
    async put(url, response) { cached.set(url, response.clone()); },
    async match(url) { return cached.get(url)?.clone(); }
  };
  const self = {
    location: {origin},
    clients: {claim: async () => {}},
    addEventListener(type, listener) { listeners.set(type, listener); },
    skipWaiting: async () => {}
  };
  vm.runInNewContext(recoverySource.replaceAll('__LINGXI_RECOVERY_CONFIGURATION__', JSON.stringify(recoveryConfig)), {
    self,
    caches: {open: async () => cache},
    fetch: (request, options) => fetchImpl(request, options),
    URL,
    Response,
    AbortController,
    AbortSignal,
    setTimeout: clock?.setTimeout || setTimeout,
    clearTimeout: clock?.clearTimeout || clearTimeout
  });

  async function install() {
    const waits = [];
    listeners.get('install')({waitUntil: promise => waits.push(promise)});
    await Promise.all(waits);
  }

  function dispatchFetch(request) {
    let responsePromise;
    listeners.get('fetch')({
      request,
      respondWith(promise) { responsePromise = Promise.resolve(promise); }
    });
    return {
      intercepted: responsePromise !== undefined,
      response: responsePromise
    };
  }

  return {cached, dispatchFetch, install};
}

function navigation(url, {method = 'GET', mode = 'navigate', body, signal = new AbortController().signal} = {}) {
  return {method, mode, url, body, signal};
}

async function cachedRecoveryDocument(worker) {
  return (await worker.cached.get(recoveryUrl).clone().text());
}

async function responseDocument(responsePromise) {
  const response = await responsePromise;
  assert.equal(response.status, 200);
  return response.text();
}

function assertDiagnostic(html, reason, readyMarker) {
  const diagnostic = html.match(/<div id="recovery-diagnostic" data-reason="([^"]*)" data-ready-marker="([^"]*)" hidden><\/div>/u);
  assert.ok(diagnostic, 'response contains a visible-page diagnostic payload');
  assert.equal(diagnostic[1], reason);
  assert.equal(diagnostic[2], String(readyMarker));
  assert.match(html, /<dt>App ID<\/dt><dd id="recovery-app-id"><\/dd>/u);
  assert.match(html, /<dt>Revision<\/dt><dd id="recovery-revision"><\/dd>/u);
  assert.match(html, /<dt>Ready marker present<\/dt><dd id="recovery-ready-marker">Unavailable<\/dd>/u);
  assert.match(html, /recovery-app-id'\)\.textContent=String\(config\.appId\)/u);
  assert.match(html, /recovery-revision'\)\.textContent=String\(config\.revision\)/u);
  const pageScript = html.match(/<script>([\s\S]*)<\/script>/u)?.[1];
  assert.ok(pageScript, 'response contains the recovery page script');
  assert.doesNotThrow(() => new vm.Script(pageScript), 'recovery page script parses');

  const pageNodes = new Map([
    ['#recovery-diagnostic', {dataset: {reason: diagnostic[1], readyMarker: diagnostic[2]}}],
    ['#return', {}],
    ['#recovery-app-id', {textContent: ''}],
    ['#recovery-revision', {textContent: ''}],
    ['#recovery-reason', {textContent: 'Not available for this cached page.'}],
    ['#recovery-ready-marker', {textContent: 'Unavailable'}],
    ['#runs', {}],
    ['#status', {textContent: ''}]
  ]);
  const pageContext = vm.createContext({
    document: {querySelector: selector => pageNodes.get(selector)}
  });
  assert.equal(vm.runInContext("'indexedDB' in globalThis", pageContext), false);
  vm.runInContext(pageScript, pageContext);

  assert.equal(pageNodes.get('#recovery-app-id').textContent, recoveryConfig.appId);
  assert.equal(pageNodes.get('#recovery-revision').textContent, String(recoveryConfig.revision));
  assert.equal(pageNodes.get('#recovery-reason').textContent,
    reason === 'network_error' ? 'Network error' : reason ? `HTTP ${reason}` : 'Not available for this cached page.');
  assert.equal(pageNodes.get('#recovery-ready-marker').textContent, reason ? String(readyMarker) : 'Unavailable');
}

test('HTTP failures show only their numeric status and leave the cached recovery document generic', async () => {
  for (const status of [403, 404, 410, 503]) {
    const current = createWorker(async () => new Response('private upstream error body', {status}));
    await current.install();
    const result = current.dispatchFetch(navigation(`${origin}/__run/private-path?access_token=private-token`));
    assert.equal(result.intercepted, true);
    const html = await responseDocument(result.response);
    assertDiagnostic(html, String(status), false);
    assert.doesNotMatch(html, /private upstream error body|private-path|access_token|private-token/u);

    const cachedHtml = await cachedRecoveryDocument(current);
    assert.match(cachedHtml, /data-reason="" data-ready-marker="unavailable"/u);
    assert.doesNotMatch(cachedHtml, /data-reason="(?:403|404|410|503)"|private upstream error body/u);
  }
});

test('fetch exceptions show network_error without exposing error details or request data', async () => {
  const worker = createWorker(async () => { throw new Error('private error message and credential'); });
  await worker.install();
  const url = `${origin}/__run/private-path?access_token=private-token&__lingxi_ready=secret-run-id`;
  const result = worker.dispatchFetch(navigation(url));
  const html = await responseDocument(result.response);

  assertDiagnostic(html, 'network_error', true);
  assert.doesNotMatch(html, /private error message|credential|private-path|access_token|private-token|secret-run-id/u);
  assert.match(await cachedRecoveryDocument(worker), /data-reason="" data-ready-marker="unavailable"/u);
});

test('failed navigation builds diagnostics from the current SDK document when the cache is stale', async () => {
  const worker = createWorker(async () => new Response('private upstream error body', {status: 403}));
  await worker.install();
  worker.cached.set(recoveryUrl, new Response('<html>stale cached document without the diagnostic placeholder</html>'));

  const result = worker.dispatchFetch(navigation(`${origin}/__run/private?token=secret`));
  const html = await responseDocument(result.response);
  assertDiagnostic(html, '403', false);
  assert.doesNotMatch(html, /stale cached document|private upstream error body|private\?token/u);
  assert.equal(await cachedRecoveryDocument(worker), '<html>stale cached document without the diagnostic placeholder</html>');
});

test('dedicated recovery URL serves only the generic cached page', async () => {
  const worker = createWorker(async () => assert.fail('the recovery page must not fetch the application URL'));
  await worker.install();
  const result = worker.dispatchFetch(navigation(recoveryUrl));
  const html = await responseDocument(result.response);

  assert.match(html, /data-reason="" data-ready-marker="unavailable"/u);
  assert.match(html, /config\.appId\)/u);
  assert.match(html, /config\.revision\)/u);
  assert.doesNotMatch(html, /data-reason="(?:403|404|410|503|network_error)"|data-ready-marker="(?:true|false)"/u);
});

test('successful HTTP responses and redirects below 400 pass through unchanged', async () => {
  for (const upstream of [new Response('success body', {status: 200}),
    new Response('redirect body', {status: 302, headers: {location: `${origin}/destination`}})]) {
    const worker = createWorker(async () => upstream);
    await worker.install();
    const result = worker.dispatchFetch(navigation(`${origin}/__run/epoch/index.html`));
    assert.equal(result.intercepted, true);
    const response = await result.response;
    assert.equal(response, upstream);
    assert.equal(await response.text(), upstream.status === 200 ? 'success body' : 'redirect body');
    assert.match(await cachedRecoveryDocument(worker), /data-reason="" data-ready-marker="unavailable"/u);
  }
});

test('only GET navigations are intercepted and page input is untouched', () => {
  let fetchCount = 0;
  const worker = createWorker(async () => { fetchCount++; return new Response('ok'); });
  const body = JSON.stringify({prompt: 'must remain private'});
  const postNavigation = navigation(`${origin}/submit`, {method: 'POST', body});
  const subresource = navigation(`${origin}/asset.js`, {mode: 'cors'});

  assert.equal(worker.dispatchFetch(postNavigation).intercepted, false);
  assert.equal(postNavigation.body, body);
  assert.equal(worker.dispatchFetch(subresource).intercepted, false);
  assert.equal(fetchCount, 0);
});

test('simultaneous failed navigations keep their own reason and marker boolean', async () => {
  let rejectFirst;
  let resolveSecond;
  const firstResponse = new Promise((_, reject) => { rejectFirst = reject; });
  const secondResponse = new Promise(resolve => { resolveSecond = resolve; });
  const worker = createWorker(request => request.url.includes('/first/') ? firstResponse : secondResponse);
  await worker.install();

  const first = worker.dispatchFetch(navigation(`${origin}/first/private?__lingxi_ready=secret-run&token=secret-one`));
  const second = worker.dispatchFetch(navigation(`${origin}/second/private?token=secret-two`));
  resolveSecond(new Response('secret response body', {status: 410}));
  rejectFirst(new Error('secret error message'));
  const firstHtml = await responseDocument(first.response);
  const secondHtml = await responseDocument(second.response);

  assertDiagnostic(firstHtml, 'network_error', true);
  assertDiagnostic(secondHtml, '410', false);
  for (const secret of ['secret-run', 'secret-one', 'secret-two', 'secret response body', 'secret error message', '/first/private', '/second/private']) {
    assert.doesNotMatch(firstHtml, new RegExp(secret.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&'), 'u'));
    assert.doesNotMatch(secondHtml, new RegExp(secret.replace(/[.*+?^${}()|[\]\\]/gu, '\\$&'), 'u'));
  }
  assert.match(await cachedRecoveryDocument(worker), /data-reason="" data-ready-marker="unavailable"/u);
});

test('a stalled navigation times out at five seconds and ignores late responses or failures', async () => {
  for (const lateOutcome of ['response', 'rejection']) {
    const clock = fakeClock();
    const requestSignal = trackedSignal();
    const upstream = deferred();
    let networkSignal;
    let fetchCount = 0;
    const worker = createWorker((request, options) => {
      fetchCount++;
      networkSignal = options.signal;
      assert.equal(request.signal, requestSignal.signal);
      return upstream.promise; // Intentionally ignores abort.
    }, {clock});
    await worker.install();
    const cachedBefore = await cachedRecoveryDocument(worker);
    const url = `${origin}/__run/private-path?access_token=private-token&__lingxi_ready=secret-run`;
    const result = worker.dispatchFetch(navigation(url, {signal: requestSignal.signal}));
    let settled = false;
    void result.response.then(() => { settled = true; }, () => { settled = true; });
    assert.equal(fetchCount, 1);
    assert.deepEqual(clock.pending(), [5000]);
    assert.equal(requestSignal.listenerCount(), 1);
    clock.advance(4999);
    await new Promise(setImmediate);
    assert.equal(settled, false);
    clock.advance(1);
    const fallback = await result.response;
    const html = await fallback.clone().text();
    assertDiagnostic(html, 'network_error', true);
    assert.doesNotMatch(html, /private-path|access_token|private-token|secret-run/u);
    assert.equal(networkSignal.aborted, true);
    assert.equal(requestSignal.listenerCount(), 0);
    assert.deepEqual(clock.pending(), []);
    assert.equal(fetchCount, 1);

    if (lateOutcome === 'response') upstream.resolve(new Response('late private response'));
    else upstream.reject(new Error('late private failure'));
    await new Promise(setImmediate);
    assert.equal(await (await result.response).text(), html);
    assert.equal(await cachedRecoveryDocument(worker), cachedBefore);
    assert.equal(fetchCount, 1);
  }
});

test('headers arriving before the deadline pass the streaming response through unchanged', async () => {
  const clock = fakeClock();
  const requestSignal = trackedSignal();
  const upstream = deferred();
  let networkSignal;
  let streamController;
  const stream = new ReadableStream({start(controller) { streamController = controller; }});
  const streamed = new Response(stream, {status: 200, headers: {'Content-Type': 'text/html'}});
  const worker = createWorker((_request, options) => { networkSignal = options.signal; return upstream.promise; }, {clock});
  await worker.install();
  const result = worker.dispatchFetch(navigation(`${origin}/__run/epoch/index.html`, {signal: requestSignal.signal}));
  assert.deepEqual(clock.pending(), [5000]);
  clock.advance(4999);
  upstream.resolve(streamed);
  const response = await result.response;
  assert.equal(response, streamed);
  assert.equal(response.bodyUsed, false);
  assert.equal(requestSignal.listenerCount(), 0);
  assert.deepEqual(clock.pending(), []);
  clock.advance(120000);
  assert.equal(networkSignal.aborted, false);
  streamController.enqueue(new TextEncoder().encode('first streamed chunk'));
  streamController.close();
  assert.equal(await response.text(), 'first streamed chunk');
  assert.match(await cachedRecoveryDocument(worker), /data-reason="" data-ready-marker="unavailable"/u);
});

test('aborting the original navigation after headers aborts the real fetch response body', async () => {
  const clock = fakeClock();
  const requestSignal = trackedSignal();
  let networkSignal;
  let upstreamResponse;
  const worker = createWorker(async (_request, options) => {
    networkSignal = options.signal;
    upstreamResponse = await fetch('data:text/html,navigation%20response', {signal: networkSignal});
    return upstreamResponse;
  }, {clock});
  await worker.install();
  const result = worker.dispatchFetch(navigation(`${origin}/__run/epoch/index.html`, {signal: requestSignal.signal}));
  const response = await result.response;
  assert.equal(response, upstreamResponse);
  assert.equal(response.bodyUsed, false);
  assert.deepEqual(clock.pending(), []);
  assert.equal(requestSignal.listenerCount(), 0);
  requestSignal.controller.abort();
  assert.equal(networkSignal.aborted, true);
  await assert.rejects(response.text());
  assert.deepEqual(clock.pending(), []);
  assert.match(await cachedRecoveryDocument(worker), /data-reason="" data-ready-marker="unavailable"/u);
});

test('a post-header navigation abort reaches an in-progress streaming body', async () => {
  const clock = fakeClock();
  const requestSignal = trackedSignal();
  let networkSignal;
  let upstreamResponse;
  let bodySawAbort = false;
  const worker = createWorker((_request, options) => {
    networkSignal = options.signal;
    const stream = new ReadableStream({
      start(controller) {
        controller.enqueue(new TextEncoder().encode('first chunk'));
        networkSignal.addEventListener('abort', () => {
          bodySawAbort = true;
          controller.error(new DOMException('Navigation aborted', 'AbortError'));
        }, {once: true});
      }
    });
    upstreamResponse = new Response(stream, {status: 200});
    return upstreamResponse;
  }, {clock});
  await worker.install();
  const result = worker.dispatchFetch(navigation(`${origin}/__run/epoch/index.html`, {signal: requestSignal.signal}));
  const response = await result.response;
  assert.equal(response, upstreamResponse);
  assert.deepEqual(clock.pending(), []);
  assert.equal(requestSignal.listenerCount(), 0);
  const reader = response.body.getReader();
  assert.equal(new TextDecoder().decode((await reader.read()).value), 'first chunk');
  const pendingRead = assert.rejects(reader.read(), error => error.name === 'AbortError');
  clock.advance(120000);
  assert.equal(networkSignal.aborted, false);
  requestSignal.controller.abort();
  await pendingRead;
  assert.equal(bodySawAbort, true);
  assert.equal(networkSignal.aborted, true);
  assert.deepEqual(clock.pending(), []);
});

test('navigation cancellation propagates abort and immediately releases timers and listeners', async () => {
  for (const initiallyAborted of [false, true]) {
    const clock = fakeClock();
    const requestSignal = trackedSignal();
    const upstream = deferred();
    let networkSignal;
    let fetchCount = 0;
    const worker = createWorker((_request, options) => {
      fetchCount++;
      networkSignal = options.signal;
      return upstream.promise; // Simulate a fetch that does not reject on abort.
    }, {clock});
    await worker.install();
    if (initiallyAborted) requestSignal.controller.abort();
    const result = worker.dispatchFetch(navigation(`${origin}/__run/epoch/?__lingxi_ready=redacted`, {signal: requestSignal.signal}));
    if (!initiallyAborted) {
      assert.equal(requestSignal.listenerCount(), 1);
      assert.deepEqual(clock.pending(), [5000]);
      requestSignal.controller.abort();
    }
    const html = await responseDocument(result.response);
    assertDiagnostic(html, 'network_error', true);
    assert.doesNotMatch(html, /redacted/u);
    assert.equal(requestSignal.listenerCount(), 0);
    assert.deepEqual(clock.pending(), []);
    assert.equal(fetchCount, initiallyAborted ? 0 : 1);
    if (!initiallyAborted) {
      assert.equal(networkSignal.aborted, true);
      upstream.reject(new Error('late abort failure'));
      await new Promise(setImmediate);
    }
    assert.match(await cachedRecoveryDocument(worker), /data-reason="" data-ready-marker="unavailable"/u);
  }
});

test('network failures and HTTP errors release their own timer and request abort listener', async () => {
  for (const item of [
    {reason: 'network_error', fetch: async () => { throw new Error('private error'); }},
    {reason: '503', fetch: async () => new Response('private error body', {status: 503})}
  ]) {
    const clock = fakeClock();
    const requestSignal = trackedSignal();
    let networkSignal;
    const worker = createWorker((request, options) => {
      networkSignal = options.signal;
      return item.fetch(request, options);
    }, {clock});
    await worker.install();
    const result = worker.dispatchFetch(navigation(`${origin}/__run/epoch/`, {signal: requestSignal.signal}));
    assertDiagnostic(await responseDocument(result.response), item.reason, false);
    assert.equal(requestSignal.listenerCount(), 0);
    assert.deepEqual(clock.pending(), []);
    clock.advance(5000);
    assert.equal(networkSignal.aborted, false);
  }
});

test('one navigation timing out does not abort another navigation or change its response', async () => {
  const clock = fakeClock();
  const firstSignal = trackedSignal();
  const secondSignal = trackedSignal();
  const firstFetch = deferred();
  const secondFetch = deferred();
  const signals = new Map();
  const worker = createWorker((request, options) => {
    signals.set(request.url, options.signal);
    return request.url.includes('/first/') ? firstFetch.promise : secondFetch.promise;
  }, {clock});
  await worker.install();
  const firstUrl = `${origin}/first/index.html?__lingxi_ready=secret`;
  const secondUrl = `${origin}/second/index.html`;
  const first = worker.dispatchFetch(navigation(firstUrl, {signal: firstSignal.signal}));
  clock.advance(1000);
  const second = worker.dispatchFetch(navigation(secondUrl, {signal: secondSignal.signal}));
  clock.advance(4000);
  assertDiagnostic(await responseDocument(first.response), 'network_error', true);
  assert.equal(signals.get(firstUrl).aborted, true);
  assert.equal(signals.get(secondUrl).aborted, false);
  assert.equal(firstSignal.listenerCount(), 0);
  assert.equal(secondSignal.listenerCount(), 1);
  assert.deepEqual(clock.pending(), [1000]);
  const successfulResponse = new Response('second navigation');
  secondFetch.resolve(successfulResponse);
  assert.equal(await second.response, successfulResponse);
  assert.equal(secondSignal.listenerCount(), 0);
  assert.deepEqual(clock.pending(), []);
  firstFetch.reject(new Error('late first failure'));
  await new Promise(setImmediate);
  assert.match(await cachedRecoveryDocument(worker), /data-reason="" data-ready-marker="unavailable"/u);
});
