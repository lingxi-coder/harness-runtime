#!/usr/bin/env node
// Evaluate the unmodified request object templates from the pinned release.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: request_layout_319_oracle.mjs <2.1.319 binary> [--check]');
const binary = readFileSync(process.argv[2]);
const sha = value => createHash('sha256').update(value).digest('hex');
const nativeSha = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(sha(binary), nativeSha);
const sourceByteRanges = [];
function take(first, last, lowerBound, label) {
  const start = binary.indexOf(first, lowerBound);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= lowerBound && end > start, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, endExclusive: end, sha256: sha(bytes)});
  return bytes.toString('utf8');
}
const main = take('Bl={model:oC(ne.model)', ';if(ne.noServerToolCalls', 272200000, 'main request template');
const side = take('Kn={model:oC(Mt)', ';if(i$r(Kn))', 272330000, 'side-query request template');
const configStart = take('let Hu={...Pu.output_config??{}};', 'if(delete Pu.output_config', 272222000, 'main extra output_config initializer');
const configDelete = take('delete Pu.output_config', ',iit(', 272222000, 'main output_config removal before spread');
const effortConfig = take('function iit(', 'function gLr(', 272162900, 'main effort insertion');
const formatConfig = take('function hLr(', 'async function Bki(', 272162900, 'main format insertion');
const extras = [
  {},
  {extension: true},
  {model: 'override', max_tokens: 7, metadata: {user_id: 'override'}, stream: false,
    speed: 'slow', thread: 'extra', diagnostics: 'extra'},
  {'2': 2, '0': 0, before: 'one', after: 'two'},
  {unicode: '\ud800\udc00\ud800\u2028\u2029', nested: {'10': 'ten', '2': 'two'}},
  {tools: null, output_config: {effort: 'low'}, temperature: 0, thinking: null},
  {extension: 'before', system: 'custom system', tool_choice: {type: 'none'}, metadata: {user_id: 'override'}},
  {extension: 'before', thinking: {type: 'disabled'}, tool_choice: null, system: null},
];
const cases = [];
for (const kind of ['main', 'side']) for (const full of [false, true])
for (const sparse of [false, true])
for (const streaming of [false, true]) for (const extra of extras)
for (const reverse of [false, true]) {
  if (kind === 'side' && streaming) continue;
  let base = {max_tokens: 16, model: 'claude-sonnet-4-6', metadata: {user_id: 'fixture'},
    messages: [{role: 'user', content: 'fixture'}], system: []};
  if (full) {
    Object.assign(base, {tools: [], tool_choice: {type: 'any'}, temperature: 0.5,
      thinking: {type: 'adaptive'}, output_config: {effort: 'high',
        format: {type: 'json_schema', schema: {type: 'object'}}}});
    if (kind === 'main') Object.assign(base, {context_management: {fixture: true},
      safeguards: [{type: 'fixture', classifier_context: {fixture: true}}], speed: 'fast',
      thread: {id: 'thread'}, diagnostics: {previous_message_id: 'previous'}});
    else base.stop_sequences = ['stop'];
  }
  if (streaming) base.stream = true;
  if (sparse) {
    delete base.system;
    delete base.metadata;
  }
  if (reverse) base = Object.fromEntries(Object.entries(base).reverse());
  const input = {kind, base, extra};
  const body_json = vm.runInNewContext(`(() => {
    const B = INPUT.base, oC = value => value,
      Sw = () => true, Y1n = () => true, zPe = () => false, hbe = 'effort-beta', C7 = 'format-beta';
    ${effortConfig}; ${formatConfig};
    let body;
    if (INPUT.kind === 'main') {
      const ne = {model: B.model}, xw = B.thread ? {sliceFrom: 0, thread: B.thread} : null,
        r_ = B.messages, zS = [], dk = {system: B.system, tools: B.tools}, hx = B.tool_choice,
        bR = false, UVe = () => ({}), wv = [], Y2e = () => B.metadata, qT = B.max_tokens,
        Fh = B.thinking, j8 = B.temperature, lN = B.context_management, KL = true,
        pi = ['ctx'], mbe = 'ctx', Ru = {}, nN = Boolean(B.safeguards),
        cg = B.safeguards?.[0].classifier_context, Igs = 'fixture', Rb = false,
        dN = null, NO = null, W_ = {}, Pu = {...INPUT.extra},
        Rw = B.speed, VM = () => Boolean(B.diagnostics), Ct = Boolean(B.diagnostics),
        Rt = B.diagnostics?.previous_message_id;
      ${configStart}
      ${configDelete};
      iit(B.output_config?.effort, Hu, Pu, pi, ne.model);
      hLr(B.output_config?.format, Hu, pi, ne.model);
      let ${main};
      body = Bl;
      if (B.stream !== undefined) body = {...body, stream: B.stream};
    } else {
      const Mt = B.model, bo = false, b = B.max_tokens, Io = B.system, Eo = B.messages,
        h = B.tools, y = B.tool_choice, Mo = Boolean(B.output_config),
        Jn = B.output_config ? {format: B.output_config.format, effort: B.output_config.effort} : undefined,
        Ee = B.temperature, Jt = fn => fn(), oIr = () => true, _o = B.model,
        He = B.stop_sequences, Cn = B.thinking, UVe = () => ({}), fn = [], s_t = () => ({}),
        Y2e = () => B.metadata, gt = undefined, Ne = INPUT.extra, To = () => ({});
      let ${side};
      body = Kn;
    }
    return JSON.stringify(body);
  })()`, {INPUT: input}, {timeout: 2000});
  cases.push({input, expected: {body_json}});
}
const fixture = {
  version: '2.1.319', native_sha256: nativeSha, sourceByteRanges,
  scope: 'Exact main and side-query object templates with computed host fields supplied explicitly. Covers key order, omission, extra-body collisions, stream spread and UTF-16 JSON. Feature admission, headers and private vendor identity are separate surfaces.',
  cases,
};
const output = JSON.stringify(fixture, null, 2) + '\n';
const target = new URL('../../crates/llm-runtime/tests/fixtures/request_layout_2_1_319.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(target, 'utf8'), output);
else writeFileSync(target, output);
console.log(`Claude Code 2.1.319 request layout: ${cases.length} byte cases ${process.argv.includes('--check') ? 'checked' : 'written'}`);
