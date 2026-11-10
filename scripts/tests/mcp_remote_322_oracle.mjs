#!/usr/bin/env node
// Controlled replay of exact shipped transport methods; no network is opened.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync, mkdirSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.322');
const bytes = readFileSync(binaryPath);
const sha256 = data => createHash('sha256').update(data).digest('hex');
const nativeSha256 = 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
assert.equal(sha256(bytes), nativeSha256);
const ranges = [];
function range(label, startAnchor, endAnchor, from, maxBytes) {
  const start = bytes.indexOf(startAnchor, from);
  const end = bytes.indexOf(endAnchor, start + startAnchor.length);
  assert(start >= from && end > start && end - start <= maxBytes, `missing ${label}`);
  const source = bytes.subarray(start, end);
  ranges.push({ label, start, end, sha256: sha256(source) });
  return source.toString('utf8');
}
const httpSource = range('http-transport', 'var ATn=class{', '\nexport{', 305090000, 20000);
const mimeSource = range('content-type-parser', 'var bu=G((e)=>{', 'var Ni=', 304883000, 5000);
const headerSource = range('header-name-value-encoding', 'var bi="=?base64?"', 'function Al(', 304846000, 2000);
const sseSource = range('legacy-sse-transport', '_dr=class{', ',Ph=1,Th=', 305085000, 10000);
const timeoutSource = range('http-fetch-timeout-wrapper', 'function uo(e,n){let r=br(n);', 'function zc(){', 305713000, 2000);
const sourceOutput = args.find(arg => arg.startsWith('--source-output='))?.slice('--source-output='.length);
if (sourceOutput) {
  mkdirSync(sourceOutput, { recursive: true });
  for (const [label, source] of [['http', httpSource], ['mime', mimeSource], ['headers', headerSource], ['sse', `var ${sseSource};`], ['timeout', timeoutSource]]) {
    writeFileSync(path.join(sourceOutput, `${label}.js`), source);
  }
  writeFileSync(path.join(sourceOutput, 'ranges.json'), JSON.stringify({
    version: '2.1.322', native_sha256: nativeSha256, ranges,
    derived_source_note: 'sse.js adds only var and a trailing semicolon to the exact class-expression range.',
  }, null, 2) + '\n');
}

class NativeError extends Error {
  constructor(code, message, data) { super(message); this.code = code; this.data = data; }
}
const context = vm.createContext({
  Headers, Response, URL, AbortController, TextEncoder, btoa,
  G: factory => { const exports = {}; factory(exports); return () => exports; },
  Co: value => value,
  so: value => value ?? {},
  // These dependencies are supplied at the transport boundary. Cases use
  // valid frames only; Native message-schema normalization is covered elsewhere.
  $t: value => value,
  uoe: value => typeof value?.method === 'string' && value.id !== undefined,
  ydr: value => value?.method === 'initialize',
  EZr: value => value?.method === 'notifications/initialized',
  Je: 'io.modelcontextprotocol/protocolVersion',
  Ji: NativeError, CR: NativeError,
  Di: { ClientHttpUnexpectedContent: 'unexpected-content', ClientHttpFailedToTerminateSession: 'terminate' },
});
vm.runInContext(mimeSource + headerSource + httpSource, context, { timeout: 1000 });
const Native = vm.runInContext('ATn', context);
const mime = vm.runInContext('Oi', context);

// Replay the unchanged timeout wrapper with a controlled 50ms policy and
// deterministic timers. The underlying fetch owns response-header settlement;
// no network, real timer deadline, or native timeout-policy default is inferred.
Object.assign(context, {
  AbortSignal, DOMException,
  br: () => 50, SNn: budget => budget, _i: () => 50,
  jc: 'application/json, text/event-stream', mwt: () => false,
});
vm.runInContext(timeoutSource, context, { timeout: 1000 });
const nativeTimeoutFetch = vm.runInContext('uo', context);

const mimeCases = [
  null, '', 'application/json', 'Application/JSON; charset=utf-8',
  ' text/event-stream ', 'text/event-stream; charset="utf-8"',
  'application/json; malformed', 'application/json; quoted=","',
  'application/json; malformed,other', 'application/json; quoted="unterminated,',
  'application/json; quoted="中文,"', 'application/json; quoted="\u007f,"',
  'text/event-stream-extra', 'application/jsonp', 'text/plain',
  'application/json, text/event-stream', 'application/json; x=y; next=z',
  'application/json; \tx=y,other', 'application/json; x="a\\\"b,"',
  'application/json; x="\u000b,"', 'application/json; x="\t,"',
  'application/json; x="\\\u000b,"', 'application/json; x="\\\t,"',
].map(value => ({ value, essence: mime(value) ?? null }));

function transport(headers = {}, sessionId, protocolVersion) {
  const target = Object.create(Native.prototype);
  target._url = new URL('http://127.0.0.1:1/mcp');
  target._requestInit = { headers };
  target._sessionId = sessionId;
  target._protocolVersion = protocolVersion;
  target._abortController = new AbortController();
  return target;
}
const headerCases = [];
for (const [label, headers] of [
  ['ordinary', {}],
  ['configured-mixed-case', { Accept: 'APPLICATION/custom, Application/JSON', 'Content-Type': 'text/custom', 'Mcp-Session-Id': 'configured-session', 'Mcp-Protocol-Version': 'configured-version' }],
  ['configured-lowercase', { 'mcp-session-id': 'configured-session', 'mcp-protocol-version': 'configured-version' }],
  ['authored-case-duplicates', { 'X-Dupe': 'one', 'x-dupe': 'two', Accept: 'APPLICATION/custom', accept: 'application/other' }],
]) {
  for (const method of ['initialize', 'tools/list']) {
    const target = transport(headers, 'wire-session-322', '2025-11-25');
    let sent;
    target._fetch = async (_url, init) => {
      sent = Object.fromEntries(init.headers.entries());
      return new Response(JSON.stringify({ jsonrpc: '2.0', id: 1, result: {} }), { headers: { 'content-type': 'application/json' } });
    };
    await target.send({ jsonrpc: '2.0', id: 1, method });
    headerCases.push({ label, headers, sessionId: 'wire-session-322', protocolVersion: '2025-11-25', method, expected: sent });
  }
}
const jsonCases = [];
for (const [label, contentType, response] of [
  ['single', 'application/json', { jsonrpc: '2.0', id: 7, result: { text: '\ud800', number: 1e21 } }],
  ['array', 'Application/JSON; charset=utf-8', [{ jsonrpc: '2.0', id: 7, result: { text: '\ud800' } }]],
  ['wrong-type', 'text/plain', { jsonrpc: '2.0', id: 7, result: {} }],
  ['missing-type', null, { jsonrpc: '2.0', id: 7, result: {} }],
]) {
  const target = transport();
  const received = [];
  target.onmessage = value => received.push(value);
  target._fetch = async () => new Response(Buffer.from(JSON.stringify(response)), { headers: contentType ? { 'content-type': contentType } : {} });
  let error = null;
  try { await target.send({ jsonrpc: '2.0', id: 7, method: 'tools/call' }); }
  catch (failure) { error = failure.message; }
  jsonCases.push({ label, contentType, responseRaw: JSON.stringify(response), receivedRaw: JSON.stringify(received), error });
}
const target = transport({}, 'wire-session-322', '2025-11-25');
let deleteHeaders;
target._fetch = async (_url, init) => { deleteHeaders = Object.fromEntries(init.headers.entries()); return new Response(null, { status: 204 }); };
await target.terminateSession();
const bodyHeaderCases = [];
for (const request of [
  { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'echo', _meta: { 'io.modelcontextprotocol/protocolVersion': '2026-07-28' } } },
  { jsonrpc: '2.0', method: 'notifications/progress', params: { name: 'echo', _meta: { 'io.modelcontextprotocol/protocolVersion': '2026-07-28' } } },
]) {
  const headers = new Headers();
  transport()._applyBodyDerivedHeaders(headers, request);
  bodyHeaderCases.push({ request, expected: Object.fromEntries(headers.entries()) });
}
const timeoutCases = [];
{
  let timer, schedule;
  const scheduled = new Promise(resolve => { schedule = resolve; });
  context.setTimeout = (callback, delay, ...args) => {
    timer = { callback, delay, args, cleared: false, unref() {} };
    schedule();
    return timer;
  };
  context.clearTimeout = value => { value.cleared = true; };
  const target = transport();
  let fetchSignal;
  const errors = [], received = [];
  target.onerror = error => errors.push(error);
  target.onmessage = frame => received.push(frame);
  target._fetch = nativeTimeoutFetch(async (_url, init) => {
    fetchSignal = init.signal;
    return new Promise((_resolve, reject) => init.signal.addEventListener('abort', () => reject(init.signal.reason), { once: true }));
  }, {});
  const pending = target.send({ jsonrpc: '2.0', id: 9, method: 'tools/list' });
  await scheduled;
  assert.equal(timer.delay, 50);
  timer.callback(...timer.args);
  let failure;
  try { await pending; } catch (error) { failure = error; }
  assert.equal(failure, fetchSignal.reason);
  assert.deepEqual(errors, [failure]);
  assert.equal(received.length, 0);
  assert(timer.cleared);
  assert.equal(target._abortController.signal.aborted, false);
  target._fetch = async () => new Response('{"jsonrpc":"2.0","id":10,"result":{}}', { headers: { 'content-type': 'application/json' } });
  await target.send({ jsonrpc: '2.0', id: 10, method: 'tools/list' });
  timeoutCases.push({
    label: 'response-headers-timeout', policyMs: timer.delay,
    errorName: failure.name, errorMessage: failure.message,
    sameErrorReportedAndRejected: errors[0] === failure,
    inboundFramesBeforeRetry: 0, retryFrameId: received[0].id,
    fetchSignalAborted: fetchSignal.aborted,
    connectionSignalAborted: target._abortController.signal.aborted,
    timerCleared: timer.cleared,
  });
}
{
  let timer, clear;
  const cleared = new Promise(resolve => { clear = resolve; });
  context.setTimeout = (callback, delay, ...args) => (timer = { callback, delay, args, cleared: false, unref() {} });
  context.clearTimeout = value => { value.cleared = true; clear(); };
  const target = transport();
  let body, fetchSignal;
  const received = [];
  target.onmessage = frame => received.push(frame);
  target._fetch = nativeTimeoutFetch(async (_url, init) => {
    fetchSignal = init.signal;
    return new Response(new ReadableStream({ start(controller) { body = controller; } }), { headers: { 'content-type': 'application/json' } });
  }, {});
  let settled = false;
  const pending = target.send({ jsonrpc: '2.0', id: 11, method: 'tools/list' }).then(() => { settled = true; });
  await cleared;
  assert.equal(settled, false);
  assert.equal(received.length, 0);
  assert.equal(fetchSignal.aborted, false);
  body.enqueue(new TextEncoder().encode('{"jsonrpc":"2.0","id":11,"result":{}}'));
  body.close();
  await pending;
  timeoutCases.push({
    label: 'body-after-response-headers', policyMs: timer.delay,
    timerClearedBeforeBody: timer.cleared, bodyPendingAfterHeaders: true,
    fetchSignalAborted: fetchSignal.aborted, receivedFrameId: received[0].id,
  });
}
const fixture = {
  version: '2.1.322', native_sha256: nativeSha256, ranges, mimeCases, headerCases, jsonCases,
  deleteHeaders, bodyHeaderCases, timeoutCases,
  scope: 'Unchanged native HTTP class, MIME/header helpers and fetch-timeout wrapper, controlled valid-frame boundary dependencies and timeout policy/timers, no network. Legacy EventSource GET MIME and named-event behavior are proved by the separate native live receipts. Native timeout failures reject the send promise; Rust maps that failure to its existing local JSON-RPC error rather than leaving the call pending.',
};
const fixtureUrl = new URL('../../crates/platforms/common/tests/fixtures/mcp_remote_2_1_322.json', import.meta.url);
const rendered = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--write')) writeFileSync(fixtureUrl, rendered);
else assert.equal(readFileSync(fixtureUrl, 'utf8'), rendered, 'native remote transport fixture drifted');
console.log(`PASS Claude Code 2.1.322 transport replay: ${mimeCases.length} MIME, ${headerCases.length} header, ${jsonCases.length} JSON, ${timeoutCases.length} timeout cases and DELETE header shape`);
