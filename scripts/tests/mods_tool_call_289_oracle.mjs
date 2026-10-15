#!/usr/bin/env node
// Source-pinned Claude Code 2.1.289 $.tool.call() Promise-wrapper oracle.
// Usage: node scripts/tests/mods_tool_call_289_oracle.mjs <2.1.289-binary> [--check]
//
// This executes the extracted native eU wrapper and native small helpers. The
// application tool runner W1 is intentionally replaced with a fake host that
// yields Native-shaped result messages; its full permission/tool implementation
// is source-pinned but is not loaded or claimed to execute here.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const args = process.argv.slice(2);
const check = args.includes('--check');
const binaries = args.filter((arg) => arg !== '--check');
assert.equal(
  binaries.length,
  1,
  'usage: node scripts/tests/mods_tool_call_289_oracle.mjs <2.1.289-binary> [--check]',
);

const sha256 = value => createHash('sha256').update(value).digest('hex');
const binary = readFileSync(binaries[0]);
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256',
);

const sourceChunks = {
  primary: {
    start: 187181803,
    end: 191691521,
    sha256: '47b62b9ac561f0cc5cd75ab6e7b9e14c1b027cccebfb900b56f07685c69fa825',
  },
  helpers: {
    start: 182637043,
    end: 182864930,
    sha256: 'b8b83549f7b4273ed3f6b4669d666e535f9abfd831ea3952e9387ac73500e85c',
  },
};

const ranges = [];
function extractedChunk(name, expectedLength) {
  const {start, end, sha256: expectedHash} = sourceChunks[name];
  const raw = binary.subarray(start, end);
  assert.equal(raw.at(-1), 0, `${name}: expected the extraction-trimmed trailing NUL`);
  const source = raw.subarray(0, -1);
  assert.equal(source.length, expectedLength, `${name}: extracted byte length changed`);
  assert.equal(sha256(source), expectedHash, `${name}: extracted source hash changed`);
  ranges.push({
    label: `${name} extracted source chunk (trailing NUL excluded)`,
    binary_start: start,
    binary_end_exclusive: end - 1,
    extracted_js_start: 0,
    extracted_js_end_exclusive: source.length,
    sha256: sha256(source),
  });
  return source;
}

const primary = extractedChunk('primary', 4509717);
const helpers = extractedChunk('helpers', 227886);

function sourceRange(chunkName, bytes, label, start, end, expectedHash, startText) {
  assert(start >= 0 && end > start, `${label}: invalid relative source range`);
  const sourceBytes = bytes.subarray(start, end);
  assert.equal(sourceBytes.length, end - start, `${label}: source range is short`);
  if (startText) {
    assert.equal(sourceBytes.subarray(0, Buffer.byteLength(startText)).toString(), startText,
      `${label}: source start marker moved`);
  }
  const digest = sha256(sourceBytes);
  assert.equal(digest, expectedHash, `${label}: source range hash changed`);
  const chunk = sourceChunks[chunkName];
  const binaryStart = chunk.start + start;
  const binaryEnd = chunk.start + end;
  const binaryBytes = binary.subarray(binaryStart, binaryEnd);
  assert.deepEqual(binaryBytes, sourceBytes, `${label}: extracted JS / native byte mapping changed`);
  ranges.push({
    label,
    binary_start: binaryStart,
    binary_end_exclusive: binaryEnd,
    extracted_js_start: start,
    extracted_js_end_exclusive: end,
    sha256: digest,
  });
  return sourceBytes.toString('utf8');
}

function binaryRange(label, start, end, expectedHash, startText) {
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: binary range is short`);
  if (startText) {
    assert.equal(bytes.subarray(0, Buffer.byteLength(startText)).toString(), startText,
      `${label}: binary start marker moved`);
  }
  const digest = sha256(bytes);
  assert.equal(digest, expectedHash, `${label}: binary range hash changed`);
  ranges.push({
    label,
    binary_start: start,
    binary_end_exclusive: end,
    sha256: digest,
  });
  return bytes.toString('utf8');
}

const nativeSource = {
  fAt: sourceRange('primary', primary, 'native FAt error-result helper', 1326507, 1326553,
    '045f105e8a6e8de36b56189cfb4a4a40534f75ce98a68423376f9fd1777cf51f', 'var FAt='),
  hfo: sourceRange('primary', primary, 'native hfo synthetic tool_use builder', 1326591, 1326696,
    '4e933392955334171fd9efc7dd0a1ab917ecebf507c0037e30e6c010318ee6f7', 'var hfo='),
  bfo: sourceRange('primary', primary, 'native bfo user-result frame extractor', 1327043, 1327129,
    'fe7a3d4a251aa07165ee0a7aa300e4ec6aea2efd2d05349a8a24044c217b4858', 'function bfo('),
  aq: sourceRange('primary', primary, 'native Aq result clone/parser', 550411, 550449,
    '825dbf7480279df62d806647d028466d4c531bb1f139dae21a93b68f78057aff', 'var Aq='),
  eU: sourceRange('primary', primary, 'native eU $.tool.call Promise wrapper', 1671409, 1673850,
    'a8488d8e2775de457b6ff288249a54f1367ef3116b6f9b64b78863bedf22dd66', 'async function eU('),
  w1: sourceRange('primary', primary, 'native W1 tool execution generator (source-only)', 1632733, 1637437,
    '2dac343f7459b2386e673fc7726ae690d0b2bb0304ae2e8727fda593fcbaa3d4', 'async function*W1('),
  dvt: sourceRange('primary', primary, 'native DVt tool.call event outcome (source-only)', 2382590, 2384735,
    '2d239227114d8a8a6f9c14bce7f44192a2758fd8c3e8ef4226498d90f9eaf52a', 'function DVt('),
  se: sourceRange('helpers', helpers, 'native _se reserved-input/shadow merge helper', 97719, 97821,
    '5bd63444d844fa9131e1eaf820ddf5d57212fcf472794ec2f1a245b89adf8702', 'function _se('),
  r5r: sourceRange('helpers', helpers, 'native r5r tool-result text extractor', 97878, 97934,
    '3c1cf0c0fced0ed217d10afd8a44a8d74874a7c95176225e695cb6569ccd2894', 'function r5r('),
  uoe: binaryRange('native Uoe Projects consent predicate', 183805687, 183805858,
    '7507090d046e83385390644695e89b695b66ec7b919048cc1dda2e69e98fa3cb', 'function Uoe('),
  webFetchAlias: binaryRange('native WebFetch alias used by Uoe', 183190392, 183190431,
    'a54a825ea4a9f70c0e0a26180866f0076cc8d8b3dcb14f4cf1e22eaf7ae9fcde', 'var _r="WebFetch"'),
  webSearchAlias: binaryRange('native WebSearch alias used by Uoe', 183304712, 183304751,
    'dd6beace88fabb5ea7984a88a74082229eeaf84b73195d7e7be1de6802f400f9', 'var Pm="WebSearch"'),
};

const state = {
  mode: 'success',
  result: {structured: 'raw-result'},
  text: 'model-facing text',
  denial: 'permission-rule',
  projectsSignal: false,
  projectsEnv: false,
  projectsSession: false,
  payloadPermit: false,
  usePayloadPermit: false,
  toolCalls: 0,
  cleanupCalls: 0,
  capturedToolUse: undefined,
  capturedContext: undefined,
  started: undefined,
};

class NativeApiError extends Error {}
const context = vm.createContext({
  AbortController,
  Date,
  JSON,
  Promise,
  String,
  Error,
  NativeApiError,
  Ie: NativeApiError,
  Pe: '$shadowed',
  _r: 'WebFetch',
  Pm: 'WebSearch',
  N: value => value !== null && typeof value === 'object',
  S: value => JSON.stringify(value),
  J: value => JSON.parse(value),
  Is: () => ({value: state.projectsSignal}),
  el: {},
  a: {get CLAUDE_CODE_PROJECTS_SESSION() { return state.projectsEnv; }},
  Sbt: () => state.projectsSession,
  SS: () => ({}),
  Tf: {},
  io: () => ({value: state.payloadPermit, source: 'payload'}),
  Use: () => state.usePayloadPermit,
  Mx: () => false,
  t: () => undefined,
  l: error => String(error),
  _fo: () => undefined,
  BOt: () => { throw new Error('unexpected Agent launch-only path'); },
  LAt: () => { throw new Error('unexpected Agent record path'); },
  ptr: value => value,
  UOt: async () => { throw new Error('unexpected Agent wait path'); },
  gfo: () => '11111111-2222-3333-4444-555555555555',
  NAt: {type: 'direct'},
  tools: [{name: 'Read'}, {name: 'WebFetch'}, {name: 'WebSearch'}, {name: 'Other'}],
  Hu: () => ({
    tools: () => context.tools,
    canUseTool: {source: 'native-host-stub'},
  }),
  yn: (available, name) => available.find(tool => tool.name === name),
  av: (outerSignal, innerController) => {
    let listener;
    if (outerSignal) {
      listener = () => innerController.abort(outerSignal.reason);
      if (outerSignal.aborted) listener();
      else outerSignal.addEventListener('abort', listener, {once: true});
    }
    return () => {
      if (outerSignal && listener) outerSignal.removeEventListener('abort', listener);
      state.cleanupCalls += 1;
    };
  },
  $At: (_catalog, controller, options) => {
    const result = {abortController: controller, messages: [], plugin: options.plugin, origin: options.origin};
    state.capturedContext = result;
    return result;
  },
  Re: message => ({type: 'user', message: {content: message.content}, origin: message.origin}),
  wc: message => ({type: 'assistant', uuid: 'virtual-assistant', message, isVirtual: true}),
  BAt: (plugin, error, label) => {
    if (error instanceof NativeApiError) throw error;
    throw new NativeApiError(`${plugin}: ${label} failed: ${String(error)}`);
  },
  r5r: undefined,
  W1: undefined,
});

vm.runInContext(
  `${nativeSource.fAt}\n${nativeSource.hfo}\n${nativeSource.bfo}\n${nativeSource.aq}\n${nativeSource.se}\n${nativeSource.r5r}\n${nativeSource.uoe}\n${nativeSource.eU}\nthis.nativeToolCall = eU;`,
  context,
);

context.W1 = async function* (toolUse, assistant, canUseTool, toolContext) {
  state.toolCalls += 1;
  state.capturedToolUse = toolUse;
  assert.equal(assistant.isVirtual, true);
  assert.equal(canUseTool.source, 'native-host-stub');
  if (state.started) state.started();
  if (state.mode === 'cancel') {
    await new Promise(resolve => {
      toolContext.abortController.signal.addEventListener('abort', resolve, {once: true});
    });
    return;
  }
  if (state.mode === 'no-result') return;
  const denied = state.mode === 'deny';
  const errored = state.mode === 'error' || denied;
  const text = denied ? '<tool_use_error>permission denied</tool_use_error>' :
    state.mode === 'error' ? 'tool failed' : state.text;
  const raw = denied ? 'Error: permission denied' :
    state.mode === 'error' ? {error: 'tool failed'} : state.result;
  yield {
    message: {
      type: 'user',
      message: {
        content: [{
          type: 'tool_result',
          tool_use_id: toolUse.id,
          content: text,
          is_error: errored,
        }],
      },
      toolUseResult: raw,
      ...(denied ? {toolDenialKind: state.denial} : {}),
    },
  };
};

function nativeToolCall(input, signal) {
  return context.nativeToolCall(input, {plugin: 'oracle-probe', origin: ['oracle-probe'], signal});
}
function plain(value) {
  return JSON.parse(JSON.stringify(value));
}
function reset(mode) {
  state.mode = mode;
  state.toolCalls = 0;
  state.cleanupCalls = 0;
  state.capturedToolUse = undefined;
  state.capturedContext = undefined;
  state.started = undefined;
  state.projectsSignal = false;
  state.projectsEnv = false;
  state.projectsEnv = false;
  state.projectsSession = false;
  state.payloadPermit = false;
  state.usePayloadPermit = false;
}

reset('success');
const success = plain(await nativeToolCall({
  tool: 'Read', tool_use_id: 'caller-id-is-not-reused', agentId: 'caller-agent',
  consent: 'The user pressed the permission card', path: '/before', $shadowed: {path: '/after'},
}));
assert.deepEqual(success, {result: {structured: 'raw-result'}, text: 'model-facing text'});
assert.equal(state.capturedToolUse.id, 'toolu_plugin_11111111222233334444555555555555');
assert.deepEqual(plain(state.capturedToolUse.input), {path: '/after'});
assert.deepEqual(plain(state.capturedContext.messages.at(-1)), {
  type: 'user',
  message: {content: 'The user pressed the permission card'},
  origin: {kind: 'human'},
});
assert.equal(state.cleanupCalls, 1);

reset('error');
const ordinaryError = plain(await nativeToolCall({tool: 'Read'}));
assert.deepEqual(ordinaryError, {result: {error: 'tool failed'}, text: 'tool failed', isError: true});

reset('deny');
const denial = plain(await nativeToolCall({tool: 'Read'}));
assert.deepEqual(denial, {deny: 'permission denied'});

reset('no-result');
await assert.rejects(nativeToolCall({tool: 'Read'}), /produced no result/);
assert.equal(state.cleanupCalls, 1);

reset('cancel');
let notifyStarted;
const started = new Promise(resolve => { notifyStarted = resolve; });
state.started = notifyStarted;
const abort = new AbortController();
const cancelledCall = nativeToolCall({tool: 'Read'}, abort.signal);
await started;
abort.abort(new Error('probe cancellation'));
await assert.rejects(cancelledCall, /produced no result \(aborted\)/);
assert.equal(state.cleanupCalls, 1);

reset('success');
state.projectsSignal = true;
const callsBeforeProjectCheck = state.toolCalls;
await assert.rejects(
  nativeToolCall({tool: 'WebFetch', consent: 'human text', url: 'https://example.invalid'}),
  /WebFetch does not accept `consent` in a Projects session/,
);
assert.equal(state.toolCalls, callsBeforeProjectCheck, 'Projects consent check must precede W1');

reset('success');
assert.equal(context.Uoe('Other'), false, 'only the two native web tools are in Uoe');
assert.equal(context.Uoe('WebFetch'), false, 'ordinary session accepts consent path');
state.projectsEnv = true;
assert.equal(context.Uoe('WebFetch'), true, 'native Projects environment input blocks WebFetch consent');
state.projectsEnv = false;
state.projectsSession = true;
assert.equal(context.Uoe('WebFetch'), true, 'native Projects session blocks WebFetch consent');
assert.equal(context.Uoe('WebSearch'), true, 'native Projects session blocks WebSearch consent');
state.payloadPermit = true;
state.usePayloadPermit = false;
assert.equal(context.Uoe('WebFetch'), false, 'native payload-sourced exception is preserved');

const output = {
  native_binary_sha256: '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  extracted_source_sha256: {
    'src_187181803.js': sourceChunks.primary.sha256,
    'src_182637043.js': sourceChunks.helpers.sha256,
  },
  source_ranges: ranges,
  executed_native_wrapper: {
    function: 'eU',
    cases: {
      success: {result: success, assertions: ['raw structured result', 'model text', 'fresh toolu_plugin id', 'reserved field stripping', 'shadow merge', 'human-origin consent', 'composed-signal cleanup']},
      ordinary_tool_error: ordinaryError,
      permission_denial: denial,
      missing_result: 'rejects with no-result error',
      cancellation: 'composed caller signal aborts wrapper; no result rejects with (aborted)',
      projects_consent: 'source Uoe branch rejects before W1',
    },
    fake_host_boundary: 'W1 was replaced by an async generator yielding synthetic Native-shaped user/result frames. This executes eU and extracted _se/r5r/bfo/Aq/FAt/hfo/Uoe helpers, not native W1 permission/tool hooks or an installed Mod runtime.',
  },
  projects_predicate: {
    source_expression: nativeSource.uoe,
    native_tool_names: ['WebFetch', 'WebSearch'],
    project_inputs_used_by_native: ['Is(el).value', 'CLAUDE_CODE_PROJECTS_SESSION', 'Sbt(SS())'],
    additional_payload_condition: 'io(Tf, false) returning {value:true, source:"payload"} with !Use() makes Uoe return false.',
    actual_projects_state_is_stubbed_in_eU_test: true,
  },
  source_only: {
    W1: 'Pinned exactly by the recorded range/hash but not executed; it depends on the native tool catalog, permission, hooks, process and session subsystems.',
    DVt: 'Pinned exactly by the recorded range/hash but not executed; outcome-to-message projection is not needed to test eU wrapper frame extraction.',
    native_llm_usage: 'No usage field is read or returned by eU; tool-created model usage remains outside this Promise wrapper.',
  },
};
if (check) {
  const expected = JSON.parse(readFileSync(
    new URL('../../docs/parity/mods-tool-call-2.1.289.json', import.meta.url),
    'utf8',
  ));
  assert.deepEqual(output, expected, 'Native tool.call source/result fixture drift');
  console.log('checked 2.1.289 tool.call oracle (14 pinned ranges, Native wrapper result/error/deny/cancel/Projects cases)');
} else {
  console.log(JSON.stringify(output, null, 2));
}
