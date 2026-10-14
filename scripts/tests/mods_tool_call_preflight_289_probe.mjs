#!/usr/bin/env node
// Focused Native 2.1.289 $.tool.call preflight/catalog probe.
// This executes exact eU, Mx, yn/Ht, Uoe, the public tool.call wrapper, the
// I9 hook API, abort propagation, and virtual tool-use construction against an
// injected advertised catalog. The active session/catalog builder, real tools,
// permissions, and tool execution remain controlled stubs.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node scripts/tests/mods_tool_call_preflight_289_probe.mjs <2.1.289-binary>');
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const binary = readFileSync(binaryPath);
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256',
);

const chunks = {
  helpers: {
    start: 182637043, end: 182864930, length: 227886,
    sha256: 'b8b83549f7b4273ed3f6b4669d666e535f9abfd831ea3952e9387ac73500e85c',
  },
  toolCatalog: {
    start: 183191976, end: 183329842, length: 137865,
    sha256: 'ce77e5ade76efa7292b949480b9e1bf490ee657ab74f842dcc27783d4ab81281',
  },
  primary: {
    start: 187181803, end: 191691521, length: 4509717,
    sha256: '47b62b9ac561f0cc5cd75ab6e7b9e14c1b027cccebfb900b56f07685c69fa825',
  },
  sessionHost: {
    start: 206056762, end: 206637683, length: 580920,
    sha256: 'ca03716821e6b76b1769143dcb5b95654d56a7d1c41036c31e0ed3c1c421c937',
  },
};

function chunk(name) {
  const pin = chunks[name];
  const raw = binary.subarray(pin.start, pin.end);
  assert.equal(raw.at(-1), 0, `${name}: expected trailing NUL`);
  const source = raw.subarray(0, -1);
  assert.equal(source.length, pin.length, `${name}: source length changed`);
  assert.equal(sha256(source), pin.sha256, `${name}: source chunk hash changed`);
  assert.deepEqual(binary.subarray(pin.start, pin.end - 1), source,
    `${name}: source mapping no longer matches executable bytes`);
  return source;
}

const source = {
  helpers: chunk('helpers'),
  toolCatalog: chunk('toolCatalog'),
  primary: chunk('primary'),
  sessionHost: chunk('sessionHost'),
};
function slice(bytes, start, end, expectedHash, marker) {
  const value = bytes.subarray(start, end);
  assert.equal(value.length, end - start, 'pinned source range length changed');
  assert.equal(sha256(value), expectedHash, 'pinned source range hash changed');
  if (marker) assert(value.toString().startsWith(marker), `expected source marker ${marker}`);
  return value.toString();
}

const sourceText = {
  nw: slice(source.helpers, 207956, 209089,
    '10bc2d7a98eaebd0dcc3193e7dd3c97d524cfac6c5b9415e9c5f8f10e3ca0812', 'var nw='),
  catalogMatch: slice(source.toolCatalog, 132804, 132869,
    'b2650546effe0fc34bcc0d1d39749c2957ac76aa753f6df32f3295928399366b', 'function Ht('),
  resolver: slice(source.toolCatalog, 132979, 133387,
    'a179f2564b0982eecb1287d53c2746dbd4f79c2e400d78d49a072f3644288411', 'var Mr='),
  batchToolFilter: slice(source.toolCatalog, 136453, 136636,
    'b8fb3141477c690ceee58e51e016579ee9975eff4b095376f8b2632df29fdba2', 'function Mx('),
  huSessionGetter: slice(source.primary, 1153251, 1153471,
    '5b08f0d0c21e64b340856d0f6593c78d9f1ec7b366dd02ccb23c7a1fe5daa9fe', 'function Hu('),
  toolCall: slice(source.primary, 1671409, 1673850,
    'a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66', 'async function eU('),
  hookApi: slice(source.primary, 2387333, 2388574,
    '40f00de65c2d558b7413b7d652112c45c3067e86388e997d35df266270275c27', 'var $Vt='),
  abortLink: slice(source.helpers, 132334, 132535,
    '179b1becbb41a9e8fbc818af5a981e52c003a5cf4984ca09fee507525b3614e1', 'function av('),
  virtualToolUse: slice(source.primary, 1326591, 1326696,
    '4e933392955334171fd9efc7dd0a1ab917ecebf507c0037e30e6c010318ee6f7', 'var hfo='),
  projectsConsent: Buffer.from(binary.subarray(183805687, 183805858)).toString(),
  activeCatalog: slice(source.sessionHost, 418802, 420578,
    '02cec48c78535528d898d12e9c0a053fffb40c7360d5b1e8285f6c5141046650', 'di=(b)=>{let x=kC()'),
  activeCatalogGetter: slice(source.sessionHost, 420578, 420593,
    'aae30344138e2b213cebc3062e5c4e6ec168fd5bb28978c0eb526fd2558afd97', 'Wi=()=>di(w())'),
};
assert.equal(sha256(Buffer.from(sourceText.projectsConsent)),
  '7507090d046e83385390644695e89b695b66ec7b919048cc1dda2e69e98fa3cb');

const state = {
  plugin: 'preflight-probe',
  catalog: [],
  catalogCalls: 0,
  toolsCalls: 0,
  projectsSignal: false,
  projectsEnv: false,
  projectsSession: false,
  payloadPermit: false,
  usePayloadPermit: false,
  consentChecks: 0,
  shadowInputCalls: 0,
  dispatchCalls: 0,
  abortControllers: 0,
  virtualIds: 0,
  toolExecutions: 0,
  w1Observations: [],
  contextSignals: [],
  logs: [],
};

class NativeApiError extends Error {}
class ProbeAbortController {
  constructor() {
    state.abortControllers += 1;
    this.controller = new AbortController();
    this.signal = this.controller.signal;
  }

  abort(reason) {
    this.controller.abort(reason);
  }
}

const context = vm.createContext({
  AbortController: ProbeAbortController,
  NativeApiError,
  Ie: NativeApiError,
  Ny: value => value,
  N: value => value !== null && typeof value === 'object',
  dQe: /^[a-zA-Z0-9_-]+$/,
  _se: input => { state.shadowInputCalls += 1; return input; },
  t: message => { state.logs.push(String(message)); },
  Hu: (_plugin, _api) => {
    state.catalogCalls += 1;
    return {tools: () => { state.toolsCalls += 1; return state.catalog; }};
  },
  Uoe: undefined,
  _r: 'WebFetch',
  Pm: 'WebSearch',
  el: {},
  Is: () => ({value: state.projectsSignal}),
  a: {get CLAUDE_CODE_PROJECTS_SESSION() { return state.projectsEnv; }},
  Sbt: () => state.projectsSession,
  SS: () => ({}),
  Tf: {},
  io: () => ({value: state.payloadPermit, source: 'payload'}),
  Use: () => state.usePayloadPermit,
  gfo: () => `00000000-0000-4000-8000-${String(++state.virtualIds).padStart(12, '0')}`,
  NAt: 'probe-caller',
  $At: (_session, controller, hook) => {
    state.contextSignals.push({signal: controller.signal, hook});
    return {messages: [], signal: controller.signal, hookOrigin: hook.origin, hookCaller: hook.plugin};
  },
  wc: value => value,
  W1: async function* (toolUse, _assistant, _session, toolContext) {
    state.toolExecutions += 1;
    state.w1Observations.push({
      name: toolUse.name,
      id: toolUse.id,
      aborted: toolContext.signal.aborted,
    });
  },
  ya: (event, hookOptions) => async input => {
    if (event !== 'tool.call') throw new Error(`unexpected hook event ${event}`);
    state.hookSignal = hookOptions.signal;
    return context.eU(input, {
      plugin: state.plugin,
      origin: hookOptions.origin,
      signal: hookOptions.signal,
    });
  },
  FVt: (event, hookOptions) => (...args) => context.ya(event, hookOptions)(...args),
});

for (const code of [
  sourceText.catalogMatch,
  sourceText.resolver,
  sourceText.batchToolFilter,
  sourceText.projectsConsent,
  sourceText.toolCall,
  sourceText.nw,
  sourceText.hookApi,
  sourceText.abortLink,
  sourceText.virtualToolUse,
]) {
  vm.runInContext(code, context);
}
context.nativeUoe = context.Uoe;
context.Uoe = toolName => {
  state.consentChecks += 1;
  return context.nativeUoe(toolName);
};
function apiWithHookSignal(signal) {
  const hookApi = context.I9({signal, origin: ['preflight-probe']});
  const dispatch = (...args) => {
    state.dispatchCalls += 1;
    assert.equal(args.length, 2, 'nw.call dispatches only the event and input');
    const [event, input] = args;
    assert.equal(event, 'tool.call');
    return hookApi.tool.call(input);
  };
  return context.nw(state.plugin, dispatch);
}

context.dispatch = (event, input) => {
  state.dispatchCalls += 1;
  assert.equal(event, 'tool.call');
  return context.eU(input, {plugin: state.plugin, origin: ['preflight-probe']});
};
const api = context.nw(state.plugin, context.dispatch);

const wrappedV1Tool = {
  name: 'WrappedLegacyBatch',
  aliases: ['hidden-batch-alias'],
  underlyingV1ToolName: 'Read',
  entryFieldName: 'files',
  perEntryHookInputs() {},
  reassemble() {},
};
const webFetchTool = {name: 'WebFetch', aliases: ['fetch']};
const webSearchTool = {name: 'WebSearch', aliases: ['search']};
const readTool = {name: 'Read', aliases: ['read_file']};
state.catalog = [webFetchTool, wrappedV1Tool, webSearchTool, readTool];

assert.equal(context.Mx(wrappedV1Tool), true,
  'Native Mx identifies a wrapped V1 batch tool from its structural fields');
assert.equal(context.Mx({
  name: 'AlmostWrapped', underlyingV1ToolName: 'Read', entryFieldName: 'files', reassemble() {},
}), false, 'all four structural fields are required for Mx');
const filteredCatalog = state.catalog.filter(tool => !context.Mx(tool));
assert.equal(context.yn(filteredCatalog, 'fetch'), webFetchTool,
  'Native resolver matches a visible tool alias');
assert.equal(context.yn(filteredCatalog, 'Fetch'), undefined,
  'Native resolver preserves case-sensitive alias matching');
assert.equal(context.yn(filteredCatalog, 'hidden-batch-alias'), undefined,
  'Native Mx filtered tools cannot be resolved through their aliases');

const callsBeforeInvalidTool = state.dispatchCalls;
await assert.rejects(api.call({tool: 42, consent: {}}),
  /preflight-probe: \$\.tool\.call takes the event's input/);
assert.equal(state.dispatchCalls, callsBeforeInvalidTool,
  'public API rejects a non-string tool before eU');
assert.equal(state.catalogCalls, 0);

await assert.rejects(api.call({tool: 'missing', consent: {}}),
  /preflight-probe: \$\.tool\.call: consent, when given, is a string/);
assert.equal(state.catalogCalls, 0,
  'eU validates consent type before reading the current session/catalog');
assert.equal(state.consentChecks, 0,
  'eU validates consent type before the Projects restriction predicate');

state.projectsSignal = true;
const beforeUnknownLogs = state.logs.length;
await assert.rejects(api.call({tool: 'missing', consent: 'human consent'}),
  /preflight-probe: \$\.tool\.call: no tool named "missing" in this session/);
assert.equal(state.consentChecks, 0,
  'unknown name is rejected before checking Projects consent');
assert(state.logs.slice(beforeUnknownLogs).some(log =>
  log.includes('the session has WebFetch, WebSearch')),
'unknown-tool diagnostic lists only Mx-filtered advertised tool names');
assert.equal(state.logs.slice(beforeUnknownLogs).some(log => log.includes('WrappedLegacyBatch')), false);

const executionsBeforeProjects = state.toolExecutions;
await assert.rejects(api.call({tool: 'fetch', consent: 'human consent', url: 'https://example.invalid'}),
  /WebFetch does not accept `consent` in a Projects session/);
assert.equal(state.consentChecks, 1,
  'resolved canonical WebFetch name reaches the actual Native Uoe predicate');
assert.equal(state.catalogCalls, 2,
  'Projects rejection occurs only after current tool catalog lookup');
assert.equal(state.abortControllers, 0,
  'Projects rejection happens before constructing the tool-call abort controller');
assert.equal(state.toolExecutions, executionsBeforeProjects,
  'Projects rejection happens before W1/tool execution');
assert.equal(state.shadowInputCalls, 3,
  'eU reads/merges each otherwise valid API input before its consent type check');

// The actual I9 hook API takes its signal from the hook context. Invalid
// preflight inputs win over a pre-aborted caller signal and do not mint the
// per-call controller or virtual tool-use id.
function counters() {
  return {
    abortControllers: state.abortControllers,
    virtualIds: state.virtualIds,
    toolExecutions: state.toolExecutions,
  };
}
function assertUnchanged(before, message) {
  assert.deepEqual(counters(), before, message);
}
const aborted = new AbortController();
aborted.abort(new Error('caller already cancelled'));
state.projectsSignal = true;
let before = counters();
await assert.rejects(apiWithHookSignal(aborted.signal).call({tool: 'fetch', consent: {}}),
  /preflight-probe: \$\.tool\.call: consent, when given, is a string/);
assertUnchanged(before, 'invalid consent precedes abort-controller and virtual-id creation');

before = counters();
await assert.rejects(apiWithHookSignal(aborted.signal).call({tool: 'missing', consent: 'human'}),
  /preflight-probe: \$\.tool\.call: no tool named "missing" in this session/);
assertUnchanged(before, 'unknown tool precedes abort-controller and virtual-id creation');

before = counters();
await assert.rejects(apiWithHookSignal(aborted.signal).call({tool: 'fetch', consent: 'human'}),
  /WebFetch does not accept `consent` in a Projects session/);
assertUnchanged(before, 'Projects consent rejection precedes abort-controller and virtual-id creation');

// After preflight, eU links the hook signal to its own tool context, mints a
// virtual tool-use id, and enters W1 even when the original signal was already
// aborted. W1 is stubbed and yields no result, so the exact Native terminal
// error includes the aborted suffix.
state.projectsSignal = false;
before = counters();
await assert.rejects(apiWithHookSignal(aborted.signal).call({tool: 'read_file'}),
  /preflight-probe: \$\.tool\.call\(Read\) produced no result \(aborted\)/);
assert.equal(state.abortControllers, before.abortControllers + 1,
  'eU creates its inner controller only after successful preflight');
assert.equal(state.virtualIds, before.virtualIds + 1,
  'eU mints one virtual tool-use id before entering W1');
assert.equal(state.toolExecutions, before.toolExecutions + 1,
  'pre-aborted signal is passed into W1 rather than short-circuiting before it');
assert.equal(state.w1Observations.at(-1).name, 'Read',
  'W1 sees the canonical name after alias resolution');
assert.equal(state.w1Observations.at(-1).id,
  `toolu_plugin_${`00000000-0000-4000-8000-${String(state.virtualIds).padStart(12, '0')}`.replaceAll('-', '')}`,
  'W1 sees the freshly minted Native-shaped virtual tool-use ID');
assert.equal(state.w1Observations.at(-1).aborted, true,
  'W1 sees the aborted tool-context signal');

// nw.call has one formal input argument. A second {signal} is ignored; I9's
// default signal remains live and is the one linked into eU/W1.
before = counters();
const secondArgApi = apiWithHookSignal(undefined);
const afterI9 = counters();
await assert.rejects(secondArgApi.call({tool: 'Read'}, {signal: aborted.signal}),
  /preflight-probe: \$\.tool\.call\(Read\) produced no result$/);
assert.equal(state.abortControllers, afterI9.abortControllers + 1,
  'the call adds only eU’s inner controller; I9 already created its default signal');
assert.equal(state.toolExecutions, before.toolExecutions + 1,
  'the ignored second-argument signal does not prevent W1 from receiving a live signal');
assert.equal(state.w1Observations.at(-1).aborted, false,
  'I9 default signal, not nw.call second options, reaches the tool context');
assert.equal(state.w1Observations.at(-1).name, 'Read',
  'an alias is resolved before the internal virtual tool-use row reaches W1');

console.log('passed 2.1.289 tool.call preflight, I9 caller signal, abort propagation, virtual-id ordering, and ignored second-argument options (controlled catalog; W1 stubbed)');
