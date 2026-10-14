#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha256(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges = [];

function take(first, last, offset, label, maxBytes = 5000) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start && end - start < maxBytes, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha256(bytes)});
  return bytes.toString();
}

// Ucn imports this exact model-string normalizer from chunk-81kzzj6q.js.
const modelNormalizer = take(
  'function Mn(e){', 'function Gy(e)', 179326720,
  'native Mn model-string normalizer used by Ucn', 250,
);
const iterationAccounting = take(
  'var wle=', 'function jHt(', 188773000,
  'native usage-iteration parser, cNe iteration pricing gates, and Ucn model selection', 2500,
);
assert.match(iterationAccounting, /function cNe\(e,n,r\)/);
assert.match(iterationAccounting, /function \$cn\(e\)/);
assert.match(iterationAccounting, /function Ucn\(e,n\)/);

const helperRates = {
  'fallback-a': {input: 0.01, output: 0.02, cacheRead: 0.003, cacheCreate: 0.004, search: 0.05, fetch: 0.01},
  'fallback-b': {input: 0.03, output: 0.04, cacheRead: 0.005, cacheCreate: 0.006, search: 0.07, fetch: 0.02},
  'fallback-terminal': {input: 0.06, output: 0.08, cacheRead: 0.009, cacheCreate: 0.01, search: 0.11, fetch: 0.03},
  '': {input: 0.13, output: 0.17, cacheRead: 0.019, cacheCreate: 0.023, search: 0.29, fetch: 0.07},
};
let activeRowIndex = new WeakMap();
let activeCalls = [];

function injectedMwePrice(model, usage, options = {}) {
  const iterationIndex = activeRowIndex.get(usage);
  const kind = iterationIndex === undefined ? 'zero_token_server_tool' : 'fallback_iteration';
  const rate = helperRates[model] ?? {input: 0.001, output: 0.002, cacheRead: 0.0003, cacheCreate: 0.0004, search: 0.005, fetch: 0.001};
  const input = usage.inputTokens ?? 0;
  const output = usage.outputTokens ?? 0;
  const cacheRead = usage.cacheReadInputTokens ?? 0;
  const cacheCreate = usage.cacheCreationInputTokens ?? 0;
  const serverToolUse = options.serverToolUse ?? null;
  const syntheticUnits = input * rate.input
    + output * rate.output
    + cacheRead * rate.cacheRead
    + cacheCreate * rate.cacheCreate
    + (serverToolUse?.web_search_requests ?? 0) * rate.search
    + (serverToolUse?.web_fetch_requests ?? 0) * rate.fetch;
  activeCalls.push({
    sequence: activeCalls.length,
    kind,
    iteration_index: iterationIndex ?? null,
    model,
    token_usage: {
      input_tokens: input,
      output_tokens: output,
      cache_read_input_tokens: cacheRead,
      cache_creation_input_tokens: cacheCreate,
    },
    options: structuredClone(options),
    synthetic_units: syntheticUnits,
  });
  return syntheticUnits;
}

const context = vm.createContext({__injectedMwePrice: injectedMwePrice});
vm.runInContext(`
${modelNormalizer}
function Mwe(model, usage, options) {
  return __injectedMwePrice(model, usage, options);
}
${iterationAccounting}
`, context);

const usageContext = {
  speed: 'fast',
  inferenceGeo: 'us-east-1',
  serverToolUse: {web_search_requests: 2, web_fetch_requests: 1},
};

function tracedRows(entries) {
  const gateTrace = [];
  let reduceSequence = 0;
  const rows = new Proxy(entries, {
    get(target, key, receiver) {
      if (key !== 'reduce') return Reflect.get(target, key, receiver);
      return (callback, initialValue) => {
        const reduceIndex = ++reduceSequence;
        return Array.prototype.reduce.call(target, (accumulator, entry, index, source) => {
          const beforeCalls = activeCalls.length;
          const next = callback(accumulator, entry, index, source);
          gateTrace.push({
            reduce_index: reduceIndex,
            iteration_index: index,
            type: entry.type,
            model: entry.model ?? null,
            accumulator_before: accumulator,
            accumulator_after: next,
            mwe_calls_during_gate: activeCalls.length - beforeCalls,
          });
          return next;
        }, initialValue);
      };
    },
  });
  return {rows, gateTrace};
}

function runCase(input) {
  const parsed = context.$cn(input.usage);
  const entries = parsed.entries;
  activeCalls = [];
  activeRowIndex = new WeakMap(entries.map((entry, index) => [entry, index]));
  const {rows, gateTrace} = tracedRows(entries);
  const result = context.cNe(rows, usageContext, input.stop_reason);
  const lastFallbackGate = gateTrace
    .filter(gate => gate.reduce_index === 1)
    .at(-1)?.accumulator_after ?? -1;
  const pricedIterationIndexes = activeCalls
    .filter(call => call.kind === 'fallback_iteration')
    .map(call => call.iteration_index);
  const expectedTerminalDiscard = input.stop_reason === 'refusal' && lastFallbackGate >= 0
    ? lastFallbackGate
    : null;
  const observedMweSum = activeCalls.reduce((sum, call) => sum + call.synthetic_units, 0);
  assert.equal(result, observedMweSum, 'native cNe must return the sum of injected Mwe calls');
  if (expectedTerminalDiscard !== null) {
    assert(!pricedIterationIndexes.includes(expectedTerminalDiscard), 'refusal tail must not call Mwe');
  }
  return {
    input: structuredClone(input),
    parsed_usage_iterations: structuredClone(parsed),
    stop_reason: input.stop_reason,
    last_fallback_message_index_from_native_first_reduce: lastFallbackGate,
    excluded_terminal_fallback_index: expectedTerminalDiscard,
    included_iteration_indexes_from_mwe_calls: pricedIterationIndexes,
    gate_trace: gateTrace,
    mwe_calls_in_native_order: activeCalls,
    native_cNe_result_using_injected_helper: result,
    observed_injected_mwe_sum: observedMweSum,
    served_fallback_model_from_native_parser: parsed.servedFallbackModel ?? null,
  };
}

const cases = [
  runCase({
    name: 'refusal_excludes_last_fallback_message_and_then_prices_zero_token_server_tools',
    stop_reason: 'refusal',
    usage: {iterations: [
      {type: 'primary_message', model: 'primary-model', input_tokens: 40, output_tokens: 5},
      {type: 'fallback_message', model: 'fallback-a', input_tokens: 11, output_tokens: 2, cache_read_input_tokens: 3, cache_creation_input_tokens: 1},
      {type: 'fallback_message', model: 'fallback-b', input_tokens: 5, output_tokens: 4, cache_read_input_tokens: 0, cache_creation_input_tokens: 2},
      {type: 'fallback_message', input_tokens: 100, output_tokens: 100},
      {type: 'fallback_message', model: 'fallback-terminal', input_tokens: 7, output_tokens: 8, cache_read_input_tokens: 1, cache_creation_input_tokens: 1},
    ]},
  }),
  runCase({
    name: 'non_refusal_prices_all_model_bearing_fallback_messages',
    stop_reason: 'end_turn',
    usage: {iterations: [
      {type: 'primary_message', model: 'primary-model', input_tokens: 40, output_tokens: 5},
      {type: 'fallback_message', model: 'fallback-a', input_tokens: 2, output_tokens: 1},
      {type: 'tool_message', model: 'fallback-b', input_tokens: 70, output_tokens: 70},
      {type: 'fallback_message', model: 'fallback-b', input_tokens: 3, output_tokens: 4},
    ]},
  }),
  runCase({
    name: 'empty_string_fallback_model_is_present_and_priced',
    stop_reason: 'end_turn',
    usage: {iterations: [
      {type: 'fallback_message', model: '', input_tokens: 3, output_tokens: 2},
    ]},
  }),
  runCase({
    name: 'non_fallback_iterations_produce_no_mwe_calls',
    stop_reason: 'end_turn',
    usage: {iterations: [
      {type: 'primary_message', model: 'primary-model', input_tokens: 40, output_tokens: 5},
      {type: 'tool_message', model: 'fallback-a', input_tokens: 10, output_tokens: 10},
    ]},
  }),
  runCase({
    name: 'parser_normalizes_bad_counters_before_native_iteration_pricing',
    stop_reason: 'end_turn',
    usage: {iterations: [
      {type: 'fallback_message', model: 'fallback-a', input_tokens: -4, output_tokens: 'not-a-number', cache_read_input_tokens: 0.5},
    ]},
  }),
];

const modelSelections = [
  {case: 'configured_model_exact_match', configured_lane_model: 'fallback-a', served_model: 'fallback-a'},
  {case: 'configured_model_normalizer_match', configured_lane_model: 'fallback-a[1m]', served_model: 'fallback-a'},
  {case: 'configured_model_differs', configured_lane_model: 'fallback-a', served_model: 'fallback-b'},
  {case: 'no_configured_model', configured_lane_model: undefined, served_model: 'fallback-b'},
  {case: 'empty_served_model', configured_lane_model: 'fallback-a', served_model: ''},
  {case: 'empty_configured_and_served_model', configured_lane_model: '', served_model: ''},
].map(input => ({
  ...input,
  normalized_configured: input.configured_lane_model === undefined
    ? null
    : context.Mn(input.configured_lane_model),
  normalized_served: context.Mn(input.served_model),
  native_Ucn_selection: context.Ucn(input.configured_lane_model, input.served_model),
}));

assert.deepEqual(cases[0].included_iteration_indexes_from_mwe_calls, [1, 2]);
assert.equal(cases[0].excluded_terminal_fallback_index, 4);
assert.deepEqual(cases[0].mwe_calls_in_native_order.map(call => call.kind), [
  'fallback_iteration', 'fallback_iteration', 'zero_token_server_tool',
]);
assert.deepEqual(cases[0].mwe_calls_in_native_order[2].token_usage, {
  input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0,
});
assert.deepEqual(cases[0].mwe_calls_in_native_order[2].options, {
  speed: 'fast', inferenceGeo: 'us-east-1', serverToolUse: {web_search_requests: 2, web_fetch_requests: 1},
});
assert.deepEqual(cases[1].included_iteration_indexes_from_mwe_calls, [1, 3]);
assert.deepEqual(cases[2].included_iteration_indexes_from_mwe_calls, [0]);
assert.equal(cases[2].served_fallback_model_from_native_parser, '');
assert.deepEqual(cases[3].mwe_calls_in_native_order, []);
assert.equal(cases[4].parsed_usage_iterations.entries[0].inputTokens, 0);
assert.equal(cases[4].parsed_usage_iterations.entries[0].outputTokens, 0);
assert.equal(cases[4].parsed_usage_iterations.entries[0].cacheReadInputTokens, 0.5);
assert.equal(modelSelections[1].native_Ucn_selection, 'fallback-a[1m]');
assert.equal(modelSelections[2].native_Ucn_selection, 'fallback-b');
assert.equal(modelSelections[3].native_Ucn_selection, 'fallback-b');
assert.equal(modelSelections[4].native_Ucn_selection, '');

const data = {
  version: '2.1.288',
  native_sha256: sha256(binary),
  sourceByteRanges,
  scope: {
    execution: 'Actual extracted native Mn, $cn, cNe, and Ucn execute in a Node VM; Mwe is injected to record price-helper calls.',
    injected_helper: 'Synthetic test units and per-model rates only; these values are not vendor prices or an estimate of real credit/cost.',
    limits: 'Validates native iteration selection, call order, model normalization/selection, and helper inputs. Does not prove full credit accounting or provider billing.',
  },
  injected_mwe_price_helper: {
    unit: 'synthetic test units; not USD',
    per_model_rates: helperRates,
    records: ['model', 'iteration index when present', 'token counters', 'helper options', 'call order', 'synthetic return value'],
  },
  usage_context: usageContext,
  cases,
  model_selections: modelSelections,
};
const output = JSON.stringify(data, null, 2) + '\n';
const fixture = new URL('../../deps/llm-client/tests/fixtures/server_fallback_accounting_2_1_288.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(fixture, 'utf8'), output);
else writeFileSync(fixture, output);
console.log(JSON.stringify({
  mode: process.argv.includes('--check') ? 'checked' : 'wrote',
  cases: cases.length,
  modelSelections: modelSelections.length,
  sourceByteRanges: sourceByteRanges.length,
}));
