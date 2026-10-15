#!/usr/bin/env node
// Execute the pinned 2.1.289 server-fallback category gate and its stream /
// completed-response callers. This is a source oracle, not a provider call.
//
// Usage:
//   node scripts/tests/mods_fallback_category_289_probe.mjs \
//     /tmp/harness-claude-code-2.1.289-darwin-arm64

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const [binaryPath] = process.argv.slice(2);
assert(binaryPath,
  'usage: node scripts/tests/mods_fallback_category_289_probe.mjs <2.1.289-binary>');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 binary SHA-256');

const sourceWindows = [];
function pin(label, start, end, expectedHash, marker) {
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: source window is short`);
  if (marker) assert.ok(bytes.toString('utf8').startsWith(marker), `${label}: marker changed`);
  assert.equal(sha256(bytes), expectedHash, `${label}: source hash changed`);
  sourceWindows.push({label, start, end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

const gateAndCallers = pin(
  'category gate and stream/completed-block helpers', 189077574, 189078881,
  '90cc9dd6bbfa9385897227fba4a268d7d9cb6c7290b27ffa297ad5435d726499',
  'function vNe(e){return{type:"fallback"');
const streamCaller = pin(
  'physical stream event loop invokes TNe on each decoded event', 189361945, 189362753,
  '6936612c6e9b578654a43e6610de55b407f1e76d1df03440f256538f759bb96a',
  'for await(let ii of zGo(_Gt(J3,g),lr,Uqt,he)){');
const completedProjector = pin(
  'completed response content projector invokes Sjt for fallback blocks',
  189341909, 189343262,
  'd43c94214c4a5ded8987c61306284e5a9ab6e3873a27f16be5a3c73c0cb700a4',
  'dwn=(Wn)=>{');
const completedCaller = pin(
  'completed response invokes the projector on parsed provider response',
  189348533, 189348599,
  '04957b66a4efebaeedb000a982f39c000672b6991c52b50dc28cf0a05c3d5d70',
  'let Ji=dwn(lr),Aa=df?CPe(Ji.content):void 0;');

assert.match(gateAndCallers,
  /function kjt\(e\)\{if\(typeof e!=="object"\|\|e===null\)return null;let n=e;if\(n\.type!=="refusal"\)return null;let r=n\.category;return typeof r==="string"&&r\.length>0&&r\.length<=64\?r:null\}/);
assert.match(gateAndCallers, /category:kjt\(s\.trigger\)/,
  'stream fallback extraction must use the category gate');
assert.match(gateAndCallers, /category:kjt\(n\.trigger\)/,
  'completed fallback extraction must use the same category gate');
assert.match(streamCaller, /let od=TNe\(ii\),Wu=!od&&bjt\(ii\);/);
assert.match(completedProjector, /let Yl=Sjt\(ac\)/);
assert.match(completedCaller, /let Ji=dwn\(lr\)/);

const context = vm.createContext({});
vm.runInContext(`${gateAndCallers}\nglobalThis.nativeCategory = kjt;\nglobalThis.nativeStreamStart = TNe;\nglobalThis.nativeCompletedBlock = Sjt;`, context);

const vectors = [
  {label: 'empty string', trigger: {type: 'refusal', category: ''}, accepted: false},
  {label: 'one ASCII unit', trigger: {type: 'refusal', category: 'x'}, accepted: true},
  {label: '64 ASCII units', trigger: {type: 'refusal', category: 'x'.repeat(64)}, accepted: true},
  {label: '65 ASCII units', trigger: {type: 'refusal', category: 'x'.repeat(65)}, accepted: false},
  {label: '32 astral characters (64 UTF-16 units)', trigger: {type: 'refusal', category: '😀'.repeat(32)}, accepted: true},
  {label: '33 astral characters (66 UTF-16 units)', trigger: {type: 'refusal', category: '😀'.repeat(33)}, accepted: false},
  {label: '63 units then an astral pair (65 units)', trigger: {type: 'refusal', category: 'x'.repeat(63) + '😀'}, accepted: false},
  {label: 'lone high surrogate', trigger: {type: 'refusal', category: String.fromCharCode(0xd800)}, accepted: true},
  {label: 'lone low surrogate', trigger: {type: 'refusal', category: String.fromCharCode(0xdc00)}, accepted: true},
  {label: 'null category', trigger: {type: 'refusal', category: null}, accepted: false},
  {label: 'number category', trigger: {type: 'refusal', category: 1}, accepted: false},
  {label: 'boolean category', trigger: {type: 'refusal', category: true}, accepted: false},
  {label: 'object category', trigger: {type: 'refusal', category: {}}, accepted: false},
  {label: 'array category', trigger: {type: 'refusal', category: []}, accepted: false},
  {label: 'missing category', trigger: {type: 'refusal'}, accepted: false},
  {label: 'wrong trigger type', trigger: {type: 'other', category: 'cyber'}, accepted: false},
  {label: 'null trigger', trigger: null, accepted: false},
  {label: 'string trigger', trigger: 'refusal', accepted: false},
];

const results = vectors.map(vector => {
  const expected = vector.accepted ? vector.trigger.category : null;
  assert.equal(context.nativeCategory(vector.trigger), expected, `${vector.label}: direct gate`);

  const streamed = context.nativeStreamStart({
    type: 'content_block_start', index: 7,
    content_block: {
      type: 'fallback', from: {model: 'source'}, to: {model: 'target'},
      trigger: vector.trigger,
    },
  });
  assert.equal(streamed?.category, expected, `${vector.label}: streamed extractor`);

  const completed = context.nativeCompletedBlock({
    type: 'fallback', from: {model: 'source'}, to: {model: 'target'},
    trigger: vector.trigger,
  });
  assert.equal(completed?.category, expected, `${vector.label}: completed extractor`);

  return {
    label: vector.label,
    trigger: vector.trigger,
    utf16_length: typeof vector.trigger?.category === 'string'
      ? vector.trigger.category.length : null,
    accepted: vector.accepted,
    direct_result: context.nativeCategory(vector.trigger),
    streamed_result: streamed?.category,
    completed_result: completed?.category,
  };
});

assert.equal(context.nativeStreamStart({
  type: 'content_block_stop', index: 7,
  content_block: {type: 'fallback', from: {model: 'source'}, to: {model: 'target'}},
}), undefined, 'the stream extractor only admits content_block_start');
assert.equal(context.nativeCompletedBlock({type: 'text', text: 'ordinary'}), undefined,
  'the completed extractor only admits fallback blocks');

const fixture = {
  oracle: 'Claude Code 2.1.289 server-fallback refusal-category producer gate',
  native_binary_sha256: sha256(binary),
  execution_scope: 'Executes the exact Native category gate and start/completed extraction helpers; source-callsite windows establish their physical stream and completed-response use. No provider request or CLI workflow is executed.',
  source_windows: sourceWindows,
  contract: {
    accepted_category: 'A string whose JavaScript length is 1 through 64 UTF-16 code units, returned unchanged.',
    rejected_category: 'Missing, null, non-string, empty, or longer than 64 UTF-16 code units yields null category while a structurally valid fallback block remains recognized.',
    raw_lone_surrogates: 'The extracted JS helper accepts lone high and low surrogates as one-unit strings. This tests the helper value contract, not the separate Rust serde_json ingress path.',
    formatter_255_unit_slice: 'Separate contract: the refusal Details formatter later slices sanitized text to 255 UTF-16 units. This oracle does not execute or merge that formatter rule.',
  },
  cases: results,
};

const output = JSON.stringify(fixture, null, 2) + '\n';
const fixturePath = new URL('../../docs/parity/mods-fallback-category-2.1.289.json', import.meta.url);
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(fixturePath, 'utf8'), output,
    'checked-in 2.1.289 category fixture is stale; regenerate without --check');
} else {
  writeFileSync(fixturePath, output);
}
console.log(JSON.stringify({cases: results.length, sourceWindows: sourceWindows.length,
  nativeSha256: sha256(binary)}));
