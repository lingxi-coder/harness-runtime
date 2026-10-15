#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha256(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');

const sourceByteRanges = [];
function take(first, last, offset, label, maxBytes = 2000) {
  const start = binary.indexOf(Buffer.from(first), offset);
  const end = binary.indexOf(Buffer.from(last), start + first.length);
  assert(start >= offset && end > start && end - start < maxBytes, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha256(bytes)});
  return bytes.toString();
}

// Actual 2.1.288 Tx/Ox/yQe run with explicit synthetic tariff facts. No
// native model catalog/default price table is loaded or implied here.
const pricingMath = take(
  'function Tx(e,n){',
  'function ad(e,n){',
  181694400,
  'native cache-write, exact geo multiplier, and per-unit usage arithmetic',
  1200,
);
assert.match(pricingMath, /function Tx\(e,n\)/);
assert.match(pricingMath, /function Ox\(e\)\{return e\.inference_geo==="us"\?Rx:1\}/);
assert.match(pricingMath, /function yQe\(e,n\)/);

const context = vm.createContext({});
vm.runInContext(`${pricingMath}\nglobalThis.__nativeYQe = yQe;`, context);

const rates = {
  fallback_a_fast: {
    inputTokens: 4,
    outputTokens: 8,
    promptCacheReadTokens: 2,
    promptCacheWriteTokens: 6,
    promptCacheWrite1hTokens: 12,
    webSearchRequests: 0.5,
  },
  fallback_b_fast: {
    inputTokens: 10,
    outputTokens: 16,
    promptCacheReadTokens: 4,
    promptCacheWriteTokens: 8,
    promptCacheWrite1hTokens: 16,
    webSearchRequests: 1,
  },
  normal_cache_write_only: {
    inputTokens: 0,
    outputTokens: 0,
    promptCacheReadTokens: 0,
    promptCacheWriteTokens: 3,
    promptCacheWrite1hTokens: 99,
    webSearchRequests: 0,
  },
};

const inputs = [
  {
    name: 'fallback_a_iteration_fast_rates_standard_geo',
    rates: rates.fallback_a_fast,
    usage: {
      input_tokens: 11,
      output_tokens: 2,
      cache_read_input_tokens: 3,
      cache_creation_input_tokens: 1,
      inference_geo: 'us-east-1',
    },
  },
  {
    name: 'fallback_b_iteration_fast_rates_standard_geo',
    rates: rates.fallback_b_fast,
    usage: {
      input_tokens: 5,
      output_tokens: 4,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 2,
      inference_geo: 'us-east-1',
    },
  },
  {
    name: 'first_model_zero_token_server_search',
    rates: rates.fallback_a_fast,
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 0,
      inference_geo: 'us-east-1',
      server_tool_use: {web_search_requests: 2, web_fetch_requests: 1},
    },
  },
  {
    name: 'exact_us_multiplier_applies_to_tokens_but_not_search',
    rates: rates.fallback_a_fast,
    usage: {
      input_tokens: 1000000,
      output_tokens: 0,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 0,
      inference_geo: 'us',
      server_tool_use: {web_search_requests: 2},
    },
  },
  {
    name: 'non_us_region_has_no_geo_multiplier',
    rates: rates.fallback_a_fast,
    usage: {
      input_tokens: 1000000,
      output_tokens: 0,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 0,
      inference_geo: 'us-east-1',
      server_tool_use: {web_search_requests: 2},
    },
  },
  {
    name: 'mwe_without_cache_creation_ttl_uses_normal_cache_write_rate',
    rates: rates.normal_cache_write_only,
    usage: {
      input_tokens: 0,
      output_tokens: 0,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 200,
    },
  },
];

const cases = inputs.map(input => ({
  ...input,
  native_yQe_usd_shaped_amount: context.__nativeYQe(input.rates, input.usage),
}));
const close = (actual, expected) => assert(Math.abs(actual - expected) < 1e-12);
close(cases[0].native_yQe_usd_shaped_amount, 0.000072);
close(cases[1].native_yQe_usd_shaped_amount, 0.00013);
close(cases[2].native_yQe_usd_shaped_amount, 1);
close(cases[3].native_yQe_usd_shaped_amount, 5.4);
close(cases[4].native_yQe_usd_shaped_amount, 5);
close(cases[5].native_yQe_usd_shaped_amount, 0.0006);

const data = {
  version: '2.1.288',
  native_sha256: sha256(binary),
  sourceByteRanges,
  scope: {
    execution: 'Actual extracted native Tx, Ox, and yQe execute in a Node VM.',
    supplied_facts: 'All rate rows below are explicit synthetic USD-shaped test inputs. They are not vendor tariffs, catalog facts, or a claim about real billing.',
    limits: 'Validates native arithmetic and per-request search separation. Does not verify tariff acquisition, full Mwe model selection, or actual credits/provider invoices.',
  },
  synthetic_rates: rates,
  cases,
};

const output = JSON.stringify(data, null, 2) + '\n';
const fixture = new URL('../../deps/llm-client/tests/fixtures/server_fallback_quote_math_2_1_288.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(fixture, 'utf8'), output);
else writeFileSync(fixture, output);
console.log(JSON.stringify({
  mode: process.argv.includes('--check') ? 'checked' : 'wrote',
  cases: cases.length,
  sourceByteRanges: sourceByteRanges.length,
}));
