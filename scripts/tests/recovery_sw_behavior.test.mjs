import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const recoverySource = readFileSync(new URL('../../crates/runtime/src/mobile/unified_local_apps/recovery_sw.js', import.meta.url), 'utf8');
const recoveryConfig = {appId: 'diagnostic-app', revision: 17, hostPlatform: 'ios'};
const origin = 'https://local-app.example';
const recoveryUrl = `${origin}/__lingxi-offline-recovery`;

function createWorker(fetchImpl = async () => new Response('upstream ok', {status: 200})) {
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
    fetch: request => fetchImpl(request),
    URL,
    Response
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

function navigation(url, {method = 'GET', mode = 'navigate', body} = {}) {
  return {method, mode, url, body};
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
