#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: media_limits_319_oracle.mjs <2.1.319 binary> [--check] [--evidence-dir <dir>]');
const binary = readFileSync(process.argv[2]);
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const binarySha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(hash(binary), binarySha256);
const ranges = [];
function take(start, end, label) {
  const bytes = binary.subarray(start, end);
  ranges.push({ label, start, end_exclusive: end, sha256: hash(bytes) });
  return bytes.toString('utf8');
}
function between(first, last, offset, label) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start, label);
  return take(start, end, label);
}
function fn(name, offset, next) {
  return between(`function ${name}(`, next ?? 'function ', offset, name);
}
const mediaSource = [
  'var ' + between('wGe=', ';function the(', 269653000, 'media placeholder and unsent prefix') + ';',
  'var ' + between('Abt=33554432', ',Rzn=', 260615000, 'native media limits') + ';',
  'var ' + between('o="A doc of this project."', ';function DMi(', 269307000, 'project document context prefix') + ';',
  fn('fdo', 269307956, 'function S_r('),
  fn('cue', 265110240),
  fn('the', 269653282),
  fn('v9e', 270947194),
  fn('P6', 272172598),
  fn('xvn', 272172676),
  fn('nit', 272172729),
  fn('Pvn', 272172782),
  fn('Kvn', 272173025, 'var ULr='),
  fn('jLr', 272175011),
  fn('WLr', 272175153),
].join('\n');
const gateSource = [
  take(260742015, 260762295, 'native model catalog'),
  fn('xR', 263095569),
  fn('Yh', 263095638),
  fn('uyt', 263095684),
  fn('ZOr', 263095724),
  fn('m$', 263095806),
  fn('jr', 263095982),
  fn('SA', 263096042),
  fn('kA', 263096102),
  fn('fT', 263096398),
  fn('bm', 263096460),
  fn('b$t', 263098520),
  fn('jOr', 263060961),
].join('\n');
const gate = between('ZOr(s,void 0)||fT(s)||n.betas.includes(Cv)', ',Y=XYt(', 272177854, 'Ovn media-count gate');
const context = vm.createContext({
  Aa: value => typeof value === 'object' && value !== null,
  D: value => typeof value === 'object' && value !== null,
  F: () => ({}),
  Nsn: { peek: () => undefined },
  l: () => {},
  i: {},
  $fe: () => false,
  oit: () => undefined,
  // No served model aliases, unavailable-model latches or window experiments.
  rt: model => model.replace(/\[1m\]$/i, ''),
  $t: model => model.replace(/\[1m\]$/i, ''),
  sd: () => undefined,
  cSt: 'unavailable-served-alias',
  Ce: () => 'firstParty',
  qgt: () => false,
  Qvo: () => undefined,
  Oo: () => false,
  GMs: () => null,
  Cv: { header: 'context-1m-2025-08-07' },
});
vm.runInContext(mediaSource + '\n' + gateSource + '\nfunction sd(id){return kAo.models.find(model=>model.id===id)}', context, { timeout: 2000 });
vm.runInContext(`function mediaCount(model, disabled, beta) { i.CLAUDE_CODE_DISABLE_1M_CONTEXT=disabled?'1':undefined; let s=model,n={betas:beta?[Cv]:[]}; return (${gate})?PNr:Qta; }`, context);
const run = expression => vm.runInContext(expression, context, { timeout: 2000 });
const models = JSON.parse(run('JSON.stringify(kAo.models.map(model=>model.id))'));
const policies = [];
for (const model of [...models, 'claude-sonnet-4-6[1m]', 'custom-model[1m]', 'custom-model']) {
  for (const disabled of [false, true]) {
    for (const beta of [false, true]) {
      context.policy = { model, disabled, beta };
      policies.push({ model, disabled, context_beta: beta, expected_count: run('mediaCount(policy.model,policy.disabled,policy.beta)') });
    }
  }
}
const defaults = [];
for (const constrained of [false, true]) {
  context.$fe = () => constrained;
  defaults.push({ constrained_endpoint: constrained, bytes: run('jLr()'), document_bytes: run('WLr()'), count_headroom: run('oBs'), byte_headroom: run('iBs') });
}
const image = data => ({ type: 'image', source: { type: 'base64', media_type: 'image/png', data } });
const document = data => ({ type: 'document', source: { type: 'base64', media_type: 'application/pdf', data } });
const text = text => ({ type: 'text', text });
const result = content => ({ type: 'tool_result', tool_use_id: 'call', content });
const user = content => ({ type: 'user', message: { role: 'user', content } });
const cases = [
  { name: 'within_limits_is_identity', args: [2, 0, 100, 0, 0], input: [user([image('a'), image('b')])] },
  { name: 'top_level_before_nested_is_removed_first', args: [1, 0, 100, 0, 0], input: [user([image('old'), result([image('new')])])] },
  { name: 'nested_before_top_level_gets_placeholder', args: [1, 0, 100, 0, 0], input: [user([result([image('old')]), image('new')])] },
  { name: 'nested_count_preserves_placeholder_slots', args: [1, 0, 100, 0, 0], input: [user([result([image('a'), image('b'), image('c'), text('keep')])])] },
  { name: 'empty_top_level_gets_placeholder', args: [0, 0, 100, 0, 0], input: [user([image('a')])] },
  { name: 'count_overflow_reserves_headroom', args: [3, 2, 100, 0, 0], input: [user([image('a'), image('b')]), user([image('c'), image('d')])] },
  { name: 'count_headroom_larger_than_limit_clears_batch', args: [1, 20, 100, 0, 0], input: [user([image('a'), image('b')])] },
  { name: 'native_default_count_headroom_at_first_overflow', args: [run('Qta'), run('oBs'), run('jLr()'), run('iBs'), run('WLr()')], input: [user(Array.from({length: 101}, () => image('YQ==')))] },
  { name: 'url_images_have_zero_byte_cost', args: [3, 0, 5, 2, 0], input: [user([image('123456'), { type: 'image', source: { type: 'url', url: 'https://example.test/image' } }])] },
  { name: 'byte_overflow_reserves_headroom', args: [100, 20, 10, 3, 0], input: [user([image('11111'), image('22222'), image('33333')])] },
  { name: 'document_budget_removes_only_documents', args: [100, 20, 20, 2, 6], input: [user([document('1111'), image('2222'), document('3333')])] },
  { name: 'disabled_byte_budget_does_not_strip', args: [100, 20, 0, 0, 6], input: [user([document('123456789')])] },
  { name: 'unsent_payload_is_always_replaced', host_rejected_media_indices: [0], args: [100, 20, 100, 0, 0], input: [user([result([image('unsent:12'), text('keep')])])] },
  { name: 'unsent_cost_participates_before_removal', host_rejected_media_indices: [1], args: [100, 20, 5, 0, 0], input: [user([image('abc'), image('unsent:10')])] },
  { name: 'project_text_document_is_not_media', args: [0, 0, 1, 0, 0], input: [user([{ type: 'document', source: { type: 'text', media_type: 'text/plain', data: 'project' }, context: 'A doc of this project. version: x' }])] },
  { name: 'ordinary_text_document_counts_as_media', args: [0, 0, 1, 0, 0], input: [user([{ type: 'document', source: { type: 'text', media_type: 'text/plain', data: 'ordinary' } }])] },
  { name: 'unrelated_empty_row_stays_empty', args: [0, 0, 100, 0, 0], input: [user([]), user([image('a')])] },
  { name: 'raw_nested_result_preserves_unknown_fields', raw_blocks: [[0, 0]], args: [0, 0, 100, 0, 0], input: [user([{ ...result([image('a'), text('keep')]), vendor_field: 'preserved' }])] },
];
for (const testCase of cases) {
  context.input = structuredClone(testCase.input);
  context.args = testCase.args;
  testCase.expected = JSON.parse(JSON.stringify(run('Kvn(input,...args)')));
}
const fixture = { version: '2.1.319', binary_sha256: binarySha256, source_byte_ranges: ranges, host_stubs: ['object predicate', 'no live unsent source WeakSet', 'telemetry sink', 'no served model aliases/catalog experiments/unavailable latches', 'canonical model IDs', 'default media feature flags'], production_scope: 'LingXi does not produce Native private rejected/unsent source state. host_rejected_media_indices is explicit TEST-ONLY host-fact input; production always uses an empty set and never infers rejection from source text.', policies, defaults, cases };
const output = JSON.stringify(fixture, null, 2) + '\n';
const path = fileURLToPath(new URL('../../crates/llm-runtime/tests/fixtures/media_limits_2_1_319.json', import.meta.url));
if (process.argv.includes('--check')) assert.equal(readFileSync(path, 'utf8'), output);
else { mkdirSync(dirname(path), { recursive: true }); writeFileSync(path, output); }
const evidenceArg = process.argv.indexOf('--evidence-dir');
if (evidenceArg !== -1) {
  const directory = resolve(process.argv[evidenceArg + 1]); mkdirSync(directory, { recursive: true });
  writeFileSync(resolve(directory, 'native_media_2_1_319.js'), mediaSource + '\n' + gateSource + '\n// Ovn media-count expression:\n' + gate);
}
console.log(JSON.stringify({ version: fixture.version, media_cases: cases.length, policies: policies.length, defaults: defaults.length, checked: process.argv.includes('--check') }));
