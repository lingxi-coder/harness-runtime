#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';

const args = process.argv.slice(2);
const check = args.includes('--check');
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? '/tmp/harness-claude-code-2.1.289-darwin-arm64';
const expectedBinarySha256 = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
const windows = [
  { label: 'main-query row-to-request builder', start: 189313301, end: 189321332, sha256: '69f76bb5846f681a43a352c9628c1d202334f981e6a29282b0fbc56adc15e544' },
  { label: 'main-query dispatch of built request', start: 189351386, end: 189353602, sha256: '74fca5f77474f4f2a94c1fb7bb68f7c408cc42751bb7804ad06fd78371957e5a' },
  { label: 'Native user and assistant content mapping', start: 189269417, end: 189270527, sha256: '14647de5cf5cd1ddfbf92977e5073aeaaa65cc02a1e4c360a0f320b660927d23' },
  { label: 'Native request message mapper', start: 189400591, end: 189402652, sha256: 'cb3193be9dd48d17409b0ca2ac323f217b4cb7a9abad842c164fe8d6645d2982' },
  { label: 'Anthropic SDK beta messages.create', start: 185713672, end: 185714784, sha256: '40e065e115be82b0133aa8ad5d500eac0b1c0e2fd831693a0d80576414c3fa06' },
  { label: 'Anthropic SDK JSON body encoder', start: 185615033, end: 185615136, sha256: '977f3f3807f3afb1fb238be74dd4b5cd29dfac47a94fad12f3fe9aef95cd5379' },
  { label: 'Anthropic SDK body delegation', start: 185772838, end: 185773601, sha256: '4d7868e17f7fa1a92e7d4811e593f274afbf369fc6932f2a4567ffb4d9f2c673' },
  { label: 'Anthropic SDK binds its body helper', start: 185760468, end: 185760512, sha256: '4aa283df0b7ad6d9ef5d4866c6827982995a718d21fbc084fce6bec24407d8b1' },
];

const hash = value => createHash('sha256').update(value).digest('hex');
const binary = await readFile(binaryPath);
assert.equal(hash(binary), expectedBinarySha256, 'pinned Native binary changed');
function source({ label, start, end, sha256 }) {
  const bytes = binary.subarray(start, end);
  assert.equal(hash(bytes), sha256, `${label} source window changed`);
  return bytes.toString('utf8');
}
const extracted = new Map(windows.map(window => [window.label, source(window)]));
const mainBuilder = extracted.get('main-query row-to-request builder');
const mainDispatch = extracted.get('main-query dispatch of built request');
assert.ok(mainBuilder.includes('ozo(nl,'), 'main request builder no longer maps accepted rows through ozo');
assert.ok(mainBuilder.includes('messages:Wg!==null&&Wg.sliceFrom>0?Df.slice(Wu[Wg.sliceFrom]):Df'), 'main request does not use the mapped messages');
assert.ok(mainDispatch.includes('let zl=lke(Rp)'), 'main dispatch does not use the request builder');
assert.ok(mainDispatch.includes('Xs.beta.messages.create({...zl'), 'main dispatch does not call beta.messages.create with the built request');
assert.ok(extracted.get('Anthropic SDK body delegation').includes('return he(this,Yp,"f").call(this,{body:n,headers:s})'), 'SDK body encoder delegation changed');
assert.equal(extracted.get('Anthropic SDK binds its body helper'), 'this.fetch=b.fetch??eR(),wn(this,Yp,nR,"f"),');

const acceptedRows = [
  {
    type: 'assistant',
    message: { content: [{ type: 'text', text: 'accepted replacement', citations: null }] },
  },
  {
    type: 'user',
    message: { content: [{ type: 'tool_result', tool_use_id: 'toolu_result', content: 'accepted result' }] },
  },
];
const nativeRuntime = {
  a: { CLAUDE_CODE_SIMULATE_PROXY_USAGE: false },
  He: () => false,
  JK: () => false,
  Nk: () => false,
  Wut: () => false,
  $1t: () => ({ markerIndices: new Set(), forkPointPinned: false, parentPromptEndMarked: false, auditTail: () => [] }),
  Pzt: () => undefined,
  i: () => {},
  c: () => {},
};
const converterSource = extracted.get('Native user and assistant content mapping');
const mapperSource = extracted.get('Native request message mapper');
const convert = vm.runInNewContext(`(() => { ${converterSource}\n${mapperSource}\nreturn ozo; })()`, nativeRuntime, { timeout: 5000 });
const projectedMessages = convert(acceptedRows, false, undefined, false, undefined, false, false, false, false, false, undefined, undefined, undefined, false, false, undefined);
const projectedPlain = JSON.parse(JSON.stringify(projectedMessages));
assert.deepEqual(projectedPlain, [
  { role: 'assistant', content: [{ type: 'text', text: 'accepted replacement', citations: null }] },
  { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_result', content: 'accepted result' }] },
], 'Native request message mapping changed citation-null or missing-is_error presence');

let captured;
const sdkContext = { Cx: value => value, Ax: {}, T$: [], Hf: () => undefined, ve: parts => parts };
const sdkCreate = vm.runInNewContext(`({ ${extracted.get('Anthropic SDK beta messages.create')} }).create`, sdkContext, { timeout: 5000 });
const fakeClient = {
  _options: { timeout: 60000 },
  post(path, options) { captured = { path, options }; return captured; },
};
sdkCreate.call({ _client: fakeClient }, {
  model: 'claude-test', max_tokens: 16, messages: projectedPlain, stream: true,
}, {});
assert.equal(captured.path, '/v1/messages?beta=true');
const encoder = vm.runInNewContext(`(() => { ${extracted.get('Anthropic SDK JSON body encoder')}\nreturn nR; })()`, {}, { timeout: 5000 });
const encoded = encoder({ headers: {}, body: captured.options.body });
const requestBody = JSON.parse(encoded.body);
assert.deepEqual(requestBody.messages, projectedPlain, 'SDK JSON body encoding changed citation-null or missing-is_error presence');
assert.equal(requestBody.messages[0].content[0].citations, null);
assert.equal(Object.hasOwn(requestBody.messages[1].content[0], 'is_error'), false);

const toolResultFlags = {};
for (const [name, flag] of [['absent', undefined], ['false', false], ['true', true]]) {
  const rows = structuredClone(acceptedRows);
  if (flag !== undefined) rows[1].message.content[0].is_error = flag;
  const mapped = convert(rows, false, undefined, false, undefined, false, false, false, false, false, undefined, undefined, undefined, false, false, undefined);
  sdkCreate.call({ _client: fakeClient }, {
    model: 'claude-test', max_tokens: 16, messages: mapped, stream: true,
  }, {});
  const body = encoder({ headers: {}, body: captured.options.body }).body;
  const result = JSON.parse(body).messages[1].content[0];
  assert.equal(Object.hasOwn(result, 'is_error'), flag !== undefined);
  if (flag !== undefined) assert.equal(result.is_error, flag);
  toolResultFlags[name] = {
    is_error_present: Object.hasOwn(result, 'is_error'),
    request_body_json: body,
    request_body_sha256: hash(body),
  };
}
assert.equal(toolResultFlags.absent.request_body_json, encoded.body);

const report = {
  result: 'PASS',
  native_binary_sha256: expectedBinarySha256,
  executed: [
    'Native ozo user/assistant request message mapping',
    'Native Anthropic SDK beta.messages.create method with fake transport',
    'Native bound JSON body encoder nR',
  ],
  native_main_path: 'lke -> ozo -> Fl.messages -> beta.messages.create',
  request_messages: requestBody.messages,
  request_body_json: encoded.body,
  request_body_sha256: hash(encoded.body),
  tool_result_flags: toolResultFlags,
  source_windows: windows,
  scope: 'No full CLI launch, network request, or provider call; the exact pinned conversion and SDK body functions were executed in isolation.',
};
if (check) {
  const fixtureUrl = new URL('../../docs/parity/mods-next-request-presence-2.1.289.json', import.meta.url);
  const fixture = JSON.parse(await readFile(fixtureUrl, 'utf8'));
  assert.deepEqual(report, fixture, 'Native request body differs from the pinned fixture');
  process.stdout.write('Native request presence and exact body fixture passed\n');
} else {
  process.stdout.write(JSON.stringify(report, null, 2) + '\n');
}
