#!/usr/bin/env node
// Source-pinned 2.1.289 UI fault contract oracle.
// Usage: node scripts/tests/mods_ui_fault_289_oracle.mjs <2.1.289-binary> [--check]
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const args = process.argv.slice(2);
const check = args.includes('--check');
const binaries = args.filter((arg) => arg !== '--check');
assert.equal(
  binaries.length,
  1,
  'usage: node scripts/tests/mods_ui_fault_289_oracle.mjs <2.1.289-binary> [--check]',
);

const binaryPath = binaries[0];
const binary = readFileSync(binaryPath);
const sha256 = (value) => createHash('sha256').update(value).digest('hex');
const expectedBinarySha256 = '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69';
assert.equal(sha256(binary), expectedBinarySha256, 'unexpected Claude Code 2.1.289 binary SHA-256');
const sourceByteRanges = [];

function take(label, start, end, expectedHash, startText, endText) {
  assert(start >= 0 && end > start, label + ': invalid pinned byte range');
  if (startText) {
    assert.equal(
      binary.subarray(start, start + Buffer.byteLength(startText)).toString(),
      startText,
      label + ': start marker moved',
    );
  }
  if (endText) {
    assert.equal(
      binary.subarray(end, end + Buffer.byteLength(endText)).toString(),
      endText,
      label + ': end marker moved',
    );
  }
  const bytes = binary.subarray(start, end);
  const digest = sha256(bytes);
  assert.equal(digest, expectedHash, label + ': source range hash changed');
  sourceByteRanges.push({label, start, end, sha256: digest});
  return bytes.toString('utf8');
}

const source = {
  componentEnum: take(
    'ui_client_fault component enum',
    181077075,
    181077472,
    '9fb3958e6ae4fe5b1acb3c594e8858a119e7aec0c098c9fa4ef20946334ab85e',
    'lr=f(()=>j(["AskUserQuestion"',
    ',Vw=f(()=>',
  ),
  requestAddress: take(
    'ui_client_fault shared request address',
    181095164,
    181095854,
    '076fe7797a979dda92a631dc2b486d4990ce165edaaf8a7809b7e0cc9c5ffd71',
    'Y=256,ir=16384,Oa=f(()=>',
    ',VAn=f(()=>',
  ),
  requestSchema: take(
    'ui_client_fault request schema',
    181100761,
    181102273,
    '693fae4140b465ae7bd1fdf6e5e4aa3a6b67ea13b9cb9313148967c4b249e6f1',
    'HW=200,YAn=f(()=>Oa().extend({subtype:R("ui_client_fault")',
    ',KW=f(()=>u({handled:H().describe("@internal False when the engine has no such Client drawn;',
  ),
  hookPin: take(
    'ui.fault pinned hook envelope',
    182668805,
    182668977,
    'fea23ce0f7ce1aceec80f0b3342f89eb97505eca0a841c2fc3afd6fd842e72eb',
    'var fo=["surface","component","requestId","element","module","phase","reason"]',
    '',
  ),
  scannerAllowlist: take(
    'function-hook scanner allowlist',
    182596971,
    182597657,
    '4116461f149f4f7e474ad14185870e921cd99ff69c69be0ad344e3d62bdbdec3',
    'var uQe=[...Xe,"tool.call","tool.check","ui.render"',
    '',
  ),
  siteIdentity: take(
    'fault site identity helper',
    186063234,
    186063296,
    'b1f076bb240612abc0f3e3a33865179a9f6af86958fe2e1022632aa3ef6ae7fc',
    'var MD=(e)=>`$\u007be.surface}',
    '',
  ),
  currentVersion: take(
    'fault current-version lookup',
    186073286,
    186073321,
    '5e6ffceaee4f8f4fd663afda7d814a17a31c982f92e70641e7832db0b019f073',
    'var Lue=(e)=>EUe(HD().getState(),e)',
    '',
  ),
  currentClientRegistry: take(
    'fault current Client registry and generation guard',
    186075934,
    186077175,
    '7b9385ffb1f7026da547ae4ef1f10e863e486009390d0dc12a691e25c7f7bddf',
    'var id=({component:e,requestId:n})=>',
    'function Oh(e,n,r){',
  ),
  drawGateAndDuplicatePredicate: take(
    'fault draw gate and duplicate predicate',
    186077175,
    186077601,
    'a3f09f2325ca4b87ff367024b4ccfd899dc975306da6a0fba46397b0fd50ce1c',
    'function Oh(e,n,r){',
    'var iO=(e,n)=>',
  ),
  dispatchAndPostDispatchInvalidation: take(
    'fault dispatch, matcher, and post-dispatch invalidation',
    186077601,
    186078649,
    '8ddc67706bf42056bb3c4d9915365e3185328e1983a69af629b2266b92cefa19',
    'var iO=(e,n)=>',
    'function fO(e){',
  ),
  reasonNormalizer: take(
    'fault final reason normalizer',
    186078649,
    186079128,
    'c1cc13647de907da30923899317af7d21826d633bee2be69dd71007b494862f8',
    'function fO(e){let n=e.replace(tL," ")',
    'var WBr=',
  ),
  payloadAndProducer: take(
    'fault hook payload and WX producer',
    186079128,
    186079534,
    'fc8b8c3dfcade48ccf4d0fd5d8b3b0366df6acafe7311abe6cd46d112cced267',
    'var WBr=(e,n,r)=>',
    '',
  ),
  faultCallbackAttachment: take(
    'Client fault callback attachment',
    186099249,
    186099267,
    '9b61783615300917f9d0cb4f282b1f557a4029bf3aa994d3db94fa8088443e01',
    'clientFault:WX()',
    '',
  ),
  controlScrubber: take(
    'fault control and lone-surrogate scrub regex',
    182639750,
    182639987,
    'baec57e91c2bc5d5447f2635a7fb24553b4204f964f2f39afe5db4bfe65efd1c',
    'var Lz={escape:String.raw',
    'function cs(',
  ),
  utf16Truncation: take(
    'fault UTF-16 safe truncation helper',
    179171341,
    179171483,
    '0cd9c06e168964b38d4d40e576fb3ecc8b21f8d0f864efa607f0f0fec619afe7',
    'function re(t,n){',
    'function ic(',
  ),
  utf16BufferHelpers: take(
    'fault UTF-16 buffer helpers',
    179171754,
    179172058,
    '99eb1d0aae1519737ae76411f7449aa99cc69ef759131289afcedcb7a7b4e1f8',
    'function f(t){if(typeof Buffer',
    'var m=/',
  ),
  reasonText: take(
    'Client frame rejection-to-text conversion',
    78987945,
    78988248,
    'b63a7ae94eed893416674e2cb0f6c4317ba12e2d63472158447ff24522084702',
    'const reasonText = error => {',
    '\n\n  // Tells the host a fault.',
  ),
  promiseRejectionWrapper: take(
    'Client promise rejection error wrapper',
    197824868,
    197824944,
    '7a8a9fcb748ef4e7f93b79c21cafb45a93f0256a857ee3227b232db5428744d1',
    'new Ie(`${f}: Client ${s.module}: a promise it returned was rejected: ${n}`)',
    '',
  ),
  hostErrorText: take(
    'Client lifecycle Error-to-text conversion',
    179164635,
    179164695,
    '2f921ed78ffeed39032d7e9c4783e05c294cba2ce61cc5ac20466ece27a81075',
    'function l(n){return n instanceof Error?n.message:String(n)}',
    'function E(',
  ),
  clientFaultReport: take(
    'Client failure report constructor',
    197829242,
    197829538,
    '55d8c5a1c3d6c4040c4fc4f91fc5ca5140055cf91dac93230437b1a15e0bf048',
    'function S(e,o,n){let s=l(o)',
    'let P=(e,o)=>',
  ),
  clientRunRender: take(
    'Client run and render failure path',
    197829587,
    197830936,
    '94ba28d05606c05a39785f5eee88d6de2d0016bf65df341c8904804944ee5cb5',
    'function y(e,o){if(!e.surface||e.isDestroyed)return;',
    'function fe(e){',
  ),
  clientLoad: take(
    'Client mount/load failure path',
    197831959,
    197832658,
    '479c25e08df41bfe106655b7548dc9fe175f4c0b0929f868d5c027e0c8250cd8',
    'function ue(e,o){if(e.isDestroyed||e.surface)return;',
    'function V(e,o){',
  ),
  ordinaryUiRenderInvalidTree: take(
    'ordinary ui.render invalid-tree fallback',
    186072521,
    186072886,
    'f335b3328b9f021349a3ca9b242f6b0b75d721102f93a660e80c09ad36865f08',
    'if(B!==void 0)return t(`ui.render ($\u007be.component}): a hook returned a tree that does not validate',
    'return hUe',
  ),
  internalUiInvalidateInstances: take(
    'internal system ui_invalidate instances schema',
    180875737,
    180877101,
    '01a251bab24d3f9bee03259064fadf0abb3b2f7719e5edde016164a7b1d60751',
    'FF=f(()=>u({type:R("system"),subtype:R("ui_invalidate")',
    '',
  ),
};

function evaluateReasonFunctions() {
  const sourceText = [
    source.utf16Truncation,
    source.utf16BufferHelpers,
    source.controlScrubber,
    'var DS=500;',
    source.reasonNormalizer,
    source.reasonText,
    source.hostErrorText,
    'this.reasonText=reasonText;this.mO=mO;this.re=re;this.hostErrorText=l;',
  ].join('\n');
  const context = vm.createContext({Buffer, _String: String, Error, Ie: class NativeError extends Error {}});
  vm.runInContext(sourceText, context);
  vm.runInContext(
    'this.promiseError=function(f,s,n){return ' + source.promiseRejectionWrapper + ';};',
    context,
  );
  return context;
}

const reasonVm = evaluateReasonFunctions();
const plugin = 'fixture-mod';
const modulePath = 'hooks/ui.ts';
const prefix = plugin + ': Client ' + modulePath + ': ';
const rejectionPrefix = 'a promise it returned was rejected: ';
const reasonInputs = [
  ['Error with message', new Error('broken render')],
  ['empty Error', new Error('')],
  ['null', null],
  ['undefined', undefined],
  ['number', 42],
  ['object with non-string message', {message: 42}],
  ['object without message', {}],
  ['throwing message getter', Object.defineProperty({}, 'message', {get() { throw new Error('getter'); }})],
];
const reasonCases = reasonInputs.map(([id, value]) => {
  const raw = reasonVm.reasonText(value);
  const wrappedError = reasonVm.promiseError(plugin, {module: modulePath}, raw);
  const reportReason = reasonVm.hostErrorText(wrappedError);
  const actual = reasonVm.mO({plugin, module: modulePath}, reportReason);
  return {id, frameReason: raw, hookReason: actual};
});
const directEmptyReason = reasonVm.mO({plugin, module: modulePath}, '');
assert.equal(directEmptyReason, 'the module failed without a message');
for (const entry of reasonCases) {
  assert.equal(entry.hookReason, rejectionPrefix + entry.frameReason, entry.id);
}
assert.equal(reasonCases.find(({id}) => id === 'empty Error').hookReason, rejectionPrefix);

const controlInput = 'a\t\n\u0000b\u007fc\ud800d\u2028e\u2029f';
const controlOutput = reasonVm.mO({plugin, module: modulePath}, controlInput);
assert.equal(controlOutput, 'a   b c d e f');
const capped = reasonVm.mO({plugin, module: modulePath}, 'x'.repeat(201));
assert.equal(capped.length, 200);
assert.equal(capped, 'x'.repeat(199) + '…');
const astralInput = 'a'.repeat(198) + '😀Z';
const astralOutput = reasonVm.mO({plugin, module: modulePath}, astralInput);
assert.equal(astralInput.length, 201);
assert.equal(astralOutput.length, 199);
assert.equal(/[\uD800-\uDBFF]$/.test(astralOutput), false);
const doublePrefix = reasonVm.mO(
  {plugin, module: modulePath},
  prefix + prefix + 'remaining',
);
assert.equal(doublePrefix, prefix + 'remaining');

function stringify(value) {
  return JSON.parse(JSON.stringify(value));
}

function makeFaultVm({hasListener = true} = {}) {
  let context;
  const trace = [];
  const dispatched = [];
  const versions = new Map();
  const pluginState = {faultRuns: new Map()};
  const versionKey = (site) => site.component + '\u0000' + site.requestId;
  const contextValues = {
    Buffer,
    performance: {now: () => 0},
    Od: {'ui.fault': 'ui.fault'},
    versions,
    pluginState,
    trace,
    dispatched,
    duringDispatch: () => undefined,
    HD: () => ({getState: () => versions}),
    EUe: (state, site) => state.get(versionKey(site)) ?? 0,
    Q_: () => pluginState,
    nt: () => false,
    c: () => undefined,
    t: () => undefined,
    l: (value) => value instanceof Error ? value.message : String(value),
    tvn: () => true,
    u8e: (_event, _site, options) => {
      assert.equal(options.only, plugin);
      return hasListener ? [{matcherFor: () => undefined}] : [];
    },
    qP: (_event, _unused, options) => options.only === plugin && hasListener ? ['plugin-hook'] : [],
    qT: async ({e, handlers, site}) => {
      trace.push('dispatch-start');
      dispatched.push({event: stringify(e), handlerCount: handlers.length, site});
      await context.duringDispatch(e);
      trace.push('dispatch-end');
      return {};
    },
    xS: (site) => {
      trace.push('invalidate');
      const key = versionKey(site);
      versions.set(key, (versions.get(key) ?? 0) + 1);
    },
    BC: () => undefined,
  };
  context = vm.createContext(contextValues);
  const faultCode = [
    source.siteIdentity,
    source.currentVersion,
    source.currentClientRegistry,
    source.drawGateAndDuplicatePredicate,
    source.dispatchAndPostDispatchInvalidation,
    source.controlScrubber,
    source.utf16Truncation,
    source.utf16BufferHelpers,
    source.reasonNormalizer,
    source.payloadAndProducer,
    'this.registry=is;this.makeFault=WX;this.dispatchFault=dO;',
  ].join('\n');
  vm.runInContext(faultCode, context);
  return context;
}

const clientAddress = {
  plugin,
  module: modulePath,
  component: 'AbovePrompt',
  requestId: 'instance-1',
  key: 'client-1',
};
const report = {phase: 'render', reason: 'broken render', sites: () => []};
const versionKey = (site) => site.component + '\u0000' + site.requestId;

function drawClient(context, address = clientAddress) {
  context.versions.set(versionKey(address), 1);
  const generation = context.registry.begin(address.component, address.requestId);
  context.registry.record(generation, [{
    plugin: address.plugin,
    module: address.module,
    key: address.key,
  }]);
  return generation;
}

async function invokeFault(context, address = clientAddress) {
  const handler = context.makeFault(context.registry, (wrapped) => {
    context.wrapped = stringify({plugin: wrapped.plugin, fault: wrapped.fault});
    return context.dispatchFault(wrapped, context.xS);
  });
  await handler({address, report});
}

const currentnessVm = makeFaultVm();
const staleGeneration = currentnessVm.registry.begin(clientAddress.component, clientAddress.requestId);
const latestGeneration = currentnessVm.registry.begin(clientAddress.component, clientAddress.requestId);
currentnessVm.registry.record(staleGeneration, [{
  plugin: clientAddress.plugin,
  module: clientAddress.module,
  key: clientAddress.key,
}]);
const oldGenerationWasDrawn = currentnessVm.registry.isDrawn(clientAddress);
currentnessVm.versions.set(versionKey(clientAddress), 1);
currentnessVm.registry.record(latestGeneration, [{
  plugin: clientAddress.plugin,
  module: clientAddress.module,
  key: clientAddress.key,
}]);
const latestGenerationIsDrawn = currentnessVm.registry.isDrawn(clientAddress);
assert.equal(oldGenerationWasDrawn, false);
assert.equal(latestGenerationIsDrawn, true);

const currentVm = makeFaultVm();
drawClient(currentVm);
await invokeFault(currentVm);
assert.deepEqual(currentVm.trace, ['dispatch-start', 'dispatch-end', 'invalidate']);
assert.deepEqual(currentVm.dispatched[0].event, {
  surface: 'desktop',
  component: 'AbovePrompt',
  requestId: 'instance-1',
  element: 'client-1',
  module: 'hooks/ui.ts',
  phase: 'render',
  reason: 'broken render',
});

const unmountedVm = makeFaultVm();
const unmountedGeneration = drawClient(unmountedVm);
unmountedVm.duringDispatch = () => {
  unmountedVm.registry.record(unmountedGeneration, []);
};
await invokeFault(unmountedVm);
assert.deepEqual(unmountedVm.trace, ['dispatch-start', 'dispatch-end']);
assert.equal(unmountedVm.dispatched.length, 1);

const changedVersionVm = makeFaultVm();
drawClient(changedVersionVm);
changedVersionVm.duringDispatch = () => {
  changedVersionVm.versions.set(versionKey(clientAddress), 2);
};
await invokeFault(changedVersionVm);
assert.deepEqual(changedVersionVm.trace, ['dispatch-start', 'dispatch-end', 'invalidate']);

const duplicateVm = makeFaultVm();
drawClient(duplicateVm);
await invokeFault(duplicateVm);
await invokeFault(duplicateVm);
assert.deepEqual(
  duplicateVm.trace,
  ['dispatch-start', 'dispatch-end', 'invalidate', 'dispatch-start', 'dispatch-end'],
);

const noListenerVm = makeFaultVm({hasListener: false});
drawClient(noListenerVm);
await invokeFault(noListenerVm);
assert.deepEqual(noListenerVm.trace, ['dispatch-start', 'dispatch-end']);

const noCurrentSiteVm = makeFaultVm();
const noCurrentGeneration = drawClient(noCurrentSiteVm);
noCurrentSiteVm.registry.record(noCurrentGeneration, []);
await invokeFault(noCurrentSiteVm);
assert.deepEqual(noCurrentSiteVm.trace, []);
assert.equal(noCurrentSiteVm.dispatched.length, 0);

const dispatchCases = [
  {
    id: 'current site with matching listener',
    trace: currentVm.trace,
    event: currentVm.dispatched[0].event,
    invalidations: currentVm.trace.filter((item) => item === 'invalidate').length,
  },
  {
    id: 'unmounted while plugin dispatch awaits',
    trace: unmountedVm.trace,
    dispatchedEvent: unmountedVm.dispatched[0].event,
    invalidations: unmountedVm.trace.filter((item) => item === 'invalidate').length,
  },
  {
    id: 'version changes while same Client remains drawn',
    trace: changedVersionVm.trace,
    invalidations: changedVersionVm.trace.filter((item) => item === 'invalidate').length,
  },
  {
    id: 'repeat at the already-invalidated version',
    trace: duplicateVm.trace,
    dispatchCount: duplicateVm.dispatched.length,
    invalidations: duplicateVm.trace.filter((item) => item === 'invalidate').length,
  },
  {
    id: 'no matching ui.fault listener',
    trace: noListenerVm.trace,
    dispatchCount: noListenerVm.dispatched.length,
    invalidations: noListenerVm.trace.filter((item) => item === 'invalidate').length,
  },
  {
    id: 'Client absent before producer call',
    trace: noCurrentSiteVm.trace,
    dispatchCount: noCurrentSiteVm.dispatched.length,
    invalidations: noCurrentSiteVm.trace.filter((item) => item === 'invalidate').length,
  },
];

const sourceOnlyClaims = [
  'The pinned request schema and hook pinned envelope are separate contracts.',
  'WX requires an exact currently drawn Client before dispatch.',
  'dO awaits the plugin-scoped hook chain before cO performs the current-site and listener checks.',
  'sO suppresses only an invalidation already recorded for both the bornAt and current Lue versions; it is not a pre-dispatch stale-version gate.',
  'The UI invalidation signal reached through uO/xS is internal; this evidence adds no public stream-json frame.',
  'Ordinary ui.render invalid-tree fallback is separate from Client lifecycle faults.',
];
const vmClaims = [
  'The exact extracted reasonText, promise rejection Error wrapper, Error-to-text helper, mO, re and UTF-16 buffer helper functions were executed in Node VM.',
  'The exact extracted eO registry, WX producer, dO dispatcher and cO/sO/uO invalidation path were executed in Node VM.',
  'Host version lookup, listener enumeration, qP/qT dispatch, unmount timing and xS invalidation effects are injected seams; they are not presented as Native implementations.',
];
const output = {
  version: '2.1.289',
  binary: {path: binaryPath, sha256: expectedBinarySha256},
  sourceByteRanges,
  requestContract: {
    internalControlRequest: {
      subtype: 'ui_client_fault',
      requiredAddressFields: {
        plugin: 'string, max 256',
        component: 'one of AskUserQuestion, UserMessage, AssistantMessage, ToolUse, ToolResult, ToolGroup, ToolProgress, CommandOutput, Spinner, TurnDuration, InfoNotice, SessionMode, PromptHint, AbovePrompt, Pane',
        instance_id: 'string, max 256; ui_render instance requestId',
        client: 'string, max 256; Client key',
        module: 'string, max 256; Client surface-module path',
      },
      requiredFields: {phase: ['load', 'render', 'run'], reason: 'string, max 200; empty allowed'},
      surface: 'absent from control request',
    },
    pluginHookPinnedEnvelope: {
      event: 'ui.fault',
      pinnedFields: ['surface', 'component', 'requestId', 'element', 'module', 'phase', 'reason'],
      validator: 'checkArgument prevents changes to pinned fields; hook pin check is a no-op and contains no field-level enum or length schema',
      plugin: 'dispatcher scope, not a fault field',
    },
    producedHookArgument: {
      surface: 'desktop',
      component: 'parent UI render component',
      requestId: 'parent ui_render instance_id, not the derived nested Client drawing id',
      element: 'Client key',
      module: 'plugin surface-module path',
      phase: 'load, render, or run from Client lifecycle call sites',
      reason: 'mO output',
    },
  },
  reasonVm: {
    cases: reasonCases,
    directEmptyReason,
    controlInput,
    controlOutput,
    cappedOutput: {length: capped.length, tail: capped.slice(-1)},
    astralBoundary: {
      inputUtf16Length: astralInput.length,
      outputUtf16Length: astralOutput.length,
      outputEndsInLoneSurrogate: /[\uD800-\uDBFF]$/.test(astralOutput),
    },
    doublePrefix,
  },
  dispatchVm: {
    currentRegistryGeneration: {staleGenerationDrawn: oldGenerationWasDrawn, latestGenerationDrawn: latestGenerationIsDrawn},
    cases: dispatchCases,
  },
  sourceOnlyClaims,
  vmClaims,
  boundary: 'These tests execute exact source slices from the pinned Native binary in Node VM. Host state/version lookup, hook enumeration, async dispatch completion, and invalidation effect are injected so the Native source order and guards can be observed; this is not a full installed plugin, terminal renderer, live session, or public stream-json end-to-end run.',
};

const json = JSON.stringify(output, null, 2) + '\n';
const fixture = new URL('../../docs/parity/mods-ui-fault-2.1.289.json', import.meta.url);
if (check) {
  assert.equal(readFileSync(fixture, 'utf8'), json, '2.1.289 UI fault evidence fixture differs');
} else {
  writeFileSync(fixture, json);
}
console.log((check ? 'checked' : 'wrote') + ' 2.1.289 UI fault oracle (' + sourceByteRanges.length + ' pinned ranges, ' + reasonCases.length + ' reason cases, ' + dispatchCases.length + ' dispatch cases)');
