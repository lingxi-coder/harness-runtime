#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges = [];
function take(first, last, offset, label, maxLength = 3000) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start && end - start < maxLength, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha256(bytes)});
  return bytes.toString();
}

const visibleReason = take('function Tle(e)', 'function _No(', 188771000, 'native visible reason predicate');
const control = take('function oHo(e,n)', 'function sHo(', 188775250, 'native server fallback control decision');
const modelIdentity = take('function Mn(e){return e.replace', 'function Gy(', 179326700, 'native context-tag removal');
const receivedModel = take('function Ucn(e,n)', 'function jHt(', 188774000, 'native received-model alias preference');
const unavailableError = take(
  'cs({content:"The server routed this response',
  ',error:"invalid_request"',
  195138300,
  'native unavailable allowlist response error',
  500,
);

const context = vm.createContext({
  _No: reason => reason,
  f3: () => null,
  Mre: category => category,
  cs: event => event,
});
vm.runInContext(`${visibleReason}${control}${modelIdentity}${receivedModel}`, context);

const controlCases = [];
for (const reason of ['refusal', 'sticky', 'other', 'Refusal', '']) {
  for (const discardedHadToolUse of [false, true]) {
    for (const isMainThread of [false, true]) {
      for (const emitsLocalScope of [false, true]) {
        const input = {reason, discardedHadToolUse, isMainThread, emitsLocalScope};
        const native = context.oHo({
          reason,
          discardedMessages: discardedHadToolUse
            ? [{message: {content: [{type: 'tool_use'}]}}]
            : [],
          midStream: false,
          fromModel: 'origin',
          toModel: 'served',
          requestId: null,
          finalStopReason: null,
          apiRefusalCategory: null,
        }, {isMainThread, emitsLocalScope});
        const expected = {
          userVisible: native.userVisible,
          tombstonedToolUse: native.tombstonedToolUse,
          swapSession: native.swapSession,
          showBanner: native.showBanner,
          bannerScope: native.bannerScope,
        };
        controlCases.push({input, expected});
      }
    }
  }
}

const receivedModelCases = [
  {declared: undefined, received: 'claude-sonnet-4-6', expected: 'claude-sonnet-4-6'},
  {declared: 'claude-opus-4-7[1m]', received: 'claude-opus-4-7[1M]', expected: 'claude-opus-4-7[1m]'},
  {declared: 'Claude-Opus-4-7[1m]', received: 'claude-opus-4-7[1m]', expected: 'claude-opus-4-7[1m]'},
  {declared: 'a[1m]B[2M]', received: 'aB', expected: 'a[1m]B[2M]'},
  {declared: '', received: '[1m]', expected: ''},
  {declared: 'model[2m]', received: 'model-[2m]', expected: 'model-[2m]'},
];
for (const item of receivedModelCases) {
  const native = context.Ucn(item.declared, item.received);
  assert.equal(native, item.expected, JSON.stringify(item));
}

const nativeError = vm.runInContext(
  `${unavailableError},error:"invalid_request",now:()=>0,uuid:()=>""})`,
  context,
);
assert.equal(typeof nativeError.content, 'string');

const data = {
  version: '2.1.288',
  native_sha256: sha256(binary),
  sourceByteRanges,
  scope: 'Actual native Tle/oHo, Mn/Ucn and the rejected-response error string. Control inputs are supplied facts. Organization availableModels acquisition, allowlist acceptance, loop swaps, query event delivery, and user-visible UI rendering remain outside this pure helper fixture.',
  unavailableAllowlistError: nativeError.content,
  controlCases,
  receivedModelCases: receivedModelCases.map(({declared, ...item}) => ({declared: declared ?? null, ...item})),
};
const path = new URL('../../crates/core/tests/fixtures/server_fallback_control_2_1_288.json', import.meta.url);
const output = JSON.stringify(data, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(path, 'utf8'), output);
} else {
  writeFileSync(path, output);
}
console.log(JSON.stringify({controlCases: controlCases.length, receivedModelCases: receivedModelCases.length, sourceRanges: sourceByteRanges.length}));
