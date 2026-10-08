#!/usr/bin/env node
// Execute complete vEt/dEo source from the SHA-pinned native artifact.
// Usage: node scripts/tests/tool_turn_end_286_oracle.mjs /path/to/2.1.286 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binaryPath = process.argv[2];
if (!binaryPath || binaryPath === '--check') {
  throw new Error('Supply the native 2.1.286 binary path');
}
const hash = value => createHash('sha256').update(value).digest('hex');
const binary = readFileSync(binaryPath);
const binarySha256 = hash(binary);
assert.equal(binarySha256,
  '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433',
  'Unexpected native oracle SHA-256');

// These absolute offsets are bytes in that exact binary, not JS string offsets.
// Include the native key declaration and both entire functions, unchanged.
const sourceStart = 185265466;
const sourceEnd = 185265767;
const sourceBytes = binary.subarray(sourceStart, sourceEnd);
const sourceSha256 = hash(sourceBytes);
assert.equal(sourceSha256,
  '50f625eb5bc7534891b6f205d09e20b7a937a4f6bf0c0d6b5284752a3eca4296',
  'Unexpected native helper source SHA-256');
const source = sourceBytes.toString('utf8');
assert.ok(source.startsWith('var wEt="claude/endTurn";function vEt('));
assert.ok(source.endsWith('return n}'));
const oracle = vm.runInNewContext(`(()=>{${source};return {vEt,dEo}})()`, {}, {
  timeout: 2_000,
});

function row(toolEndsTurn, mcpMeta, isError = false) {
  return {
    type: 'user',
    toolEndsTurn,
    mcpMeta,
    message: {
      content: [{
        type: 'tool_result',
        tool_use_id: 'fixture-tool-use',
        content: 'completed tool result',
        is_error: isError,
      }],
    },
  };
}
const cases = [];
function booleanCase(name, toolEndsTurn, isError, mcpMeta = null, extraFrameFields = {}) {
  assert.equal(typeof toolEndsTurn, 'boolean');
  assert.equal(typeof isError, 'boolean');
  const input = { ...row(toolEndsTurn, mcpMeta, isError), ...extraFrameFields };
  const expected = oracle.dEo(input);
  cases.push({
    name,
    domain: 'single_result_boolean',
    input,
    helper_input: {
      tool_ends_turn: toolEndsTurn,
      is_error: isError,
      mcp_meta: mcpMeta,
    },
    expected,
    // Only represent false as Option::None; do not independently classify it.
    expected_helper_source: expected === false ? null : expected,
  });
}
function rawCase(name, input, unmappedReason) {
  cases.push({
    name,
    domain: 'raw_frame_only',
    input,
    unmapped_reason: unmappedReason,
    expected: oracle.dEo(input),
  });
}

const metadataCases = [
  ['absent', null],
  ['nested_true', { _meta: { 'claude/endTurn': true } }],
  ['nested_false', { _meta: { 'claude/endTurn': false } }],
  ['nested_one', { _meta: { 'claude/endTurn': 1 } }],
  ['nested_zero', { _meta: { 'claude/endTurn': 0 } }],
  ['nested_string_true', { _meta: { 'claude/endTurn': 'true' } }],
  ['nested_string_false', { _meta: { 'claude/endTurn': 'false' } }],
  ['nested_null', { _meta: { 'claude/endTurn': null } }],
  ['nested_object', { _meta: { 'claude/endTurn': {} } }],
  ['nested_array', { _meta: { 'claude/endTurn': [] } }],
  ['flat_key', { 'claude/endTurn': true }],
  ['other_key', { _meta: { 'vendor/endTurn': true } }],
  ['missing_key', { _meta: {} }],
  ['meta_null', { _meta: null }],
  ['meta_true', { _meta: true }],
  ['meta_array', { _meta: [true] }],
  ['top_level_array', [{ _meta: { 'claude/endTurn': true } }]],
  ['top_level_string', 'true'],
  ['top_level_true', true],
  ['top_level_one', 1],
];
const mcpMarkerCases = metadataCases.map(([name, input]) => ({
  name,
  input,
  expected: oracle.vEt(input),
}));
for (const [name, metadata] of metadataCases) {
  booleanCase(`mcp_${name}`, false, false, metadata);
}
booleanCase('no_marker_error', false, true);
booleanCase('tool_marker', true, false);
booleanCase('tool_marker_error', true, true);
booleanCase('mcp_marker_error', false, true, { _meta: { 'claude/endTurn': true } });
booleanCase('tool_precedes_mcp', true, false, { _meta: { 'claude/endTurn': true } });
booleanCase('tool_and_mcp_error', true, true, { _meta: { 'claude/endTurn': true } });
booleanCase('tool_precedes_invalid_mcp', true, false, { _meta: { 'claude/endTurn': 'true' } });
// A frame-level is_error is ignored. The helper bool always comes from the
// contained tool_result, so this is still a valid single-result mapping.
booleanCase('top_level_error_is_ignored', true, false, null, { is_error: true });

for (const [name, value] of [
  ['one', 1], ['negative_one', -1], ['string_false', 'false'],
  ['empty_object', {}], ['empty_array', []], ['zero', 0],
  ['empty_string', ''], ['null', null],
]) {
  rawCase(`raw_tool_marker_${name}`, row(value, null),
    'toolEndsTurn is not a boolean. Native dEo uses JS truthiness; the Rust trusted carrier accepts bool only.');
}
rawCase('raw_falsey_tool_marker_with_mcp', row(0, { _meta: { 'claude/endTurn': true } }),
  'The falsey numeric toolEndsTurn value is outside the Rust bool carrier; native falls through to the MCP marker.');
for (const [name, value] of [
  ['one', 1], ['string_true', 'true'], ['empty_object', {}],
  ['null', null], ['zero', 0],
]) {
  rawCase(`raw_error_${name}`, row(true, null, value),
    'is_error is not a boolean. Native suppresses only ===true; the Rust ToolResult field is bool.');
}
const missingError = row(true, null);
delete missingError.message.content[0].is_error;
rawCase('raw_missing_error_field', missingError,
  'No is_error field exists on the raw result; the Rust helper requires an explicit bool.');
const missingToolMarker = row(false, { _meta: { 'claude/endTurn': true } });
delete missingToolMarker.toolEndsTurn;
rawCase('raw_missing_tool_marker', missingToolMarker,
  'The raw frame omits toolEndsTurn; the Rust helper requires an explicit bool.');

for (const type of ['assistant', 'attachment', 'system']) {
  rawCase(`raw_non_user_${type}`, { ...row(true, { _meta: { 'claude/endTurn': true } }), type },
    'Native dEo first requires a user frame; the helper is called at the typed tool-result boundary and has no frame-type argument.');
}
for (const [name, blocks] of [
  ['error_first', [{ type: 'tool_result', is_error: true }, { type: 'tool_result', is_error: false }]],
  ['error_last', [{ type: 'tool_result', is_error: false }, { type: 'tool_result', is_error: true }]],
  ['all_success', [{ type: 'tool_result', is_error: false }, { type: 'tool_result', is_error: false }]],
  ['non_boolean_error', [{ type: 'tool_result', is_error: 1 }, { type: 'tool_result', is_error: false }]],
]) {
  rawCase(`raw_multiple_results_${name}`, { ...row(true, null), message: { content: blocks } },
    'One raw user frame contains multiple results. Native scans every result for an exact true error; a single invocation helper cannot represent this frame.');
}
rawCase('raw_multiple_results_mcp_error_last', {
  ...row(false, { _meta: { 'claude/endTurn': true } }),
  message: { content: [{ type: 'tool_result', is_error: false }, { type: 'tool_result', is_error: true }] },
}, 'The MCP marker is on a raw multi-result frame; any contained exact true tool_result error suppresses it.');
for (const [name, content] of [
  ['string_content', 'no tool-result array'],
  ['null_content', null],
  ['object_content', {}],
  ['empty_array', []],
  ['text_error_field', [{ type: 'text', text: 'visible text', is_error: true }]],
]) {
  rawCase(`raw_${name}`, { ...row(true, null), message: { content } },
    'The content is not one typed tool_result. Native only scans arrays and only suppresses tool_result blocks; the helper consumes an actual invocation result.');
}
const missingToolUseId = row(true, null);
delete missingToolUseId.message.content[0].tool_use_id;
rawCase('raw_missing_tool_use_id', missingToolUseId,
  'Native does not require a tool_use_id here. The typed invocation boundary supplies a real tool-use id; this malformed raw frame is intentionally not mapped.');

const fixture = {
  version: '2.1.286',
  binary_sha256: binarySha256,
  source_byte_ranges: [{
    source_chunk: 'src_183727495.js',
    source_line: 1528,
    signature: 'var wEt="claude/endTurn"; complete vEt and dEo',
    start: sourceStart,
    end: sourceEnd,
    sha256: sourceSha256,
    source,
  }],
  execution: {
    native_helpers: ['vEt', 'dEo'],
    source_rewrites: [],
    dependency_substitutions: [],
    result_values: 'Native false, "tool", or "mcp_meta" are preserved without independently implementing classification.',
  },
  helper_mapping: {
    single_result_boolean: 'A user frame containing exactly one tool_result, with boolean toolEndsTurn and is_error. helper_input maps to tool_result_turn_end(tool_ends_turn, is_error, mcp_meta.as_ref()). Null mcp_meta maps to None; all other JSON values remain Some(value). Native false maps to None, and a native source string maps to the corresponding typed source.',
    raw_frame_only: 'No helper_input is supplied. Non-boolean/missing control fields, non-user frames, multiple results, and non-tool-result content are outside that single-result bool domain. Do not coerce these raw fixtures into Rust bool arguments or treat a whole aggregated frame as one invocation.',
  },
  mcp_marker_cases: mcpMarkerCases,
  cases,
};
const output = `${JSON.stringify(fixture, null, 2)}\n`;
const fixturePath = fileURLToPath(new URL('../../crates/core/tests/fixtures/tool_turn_end_2_1_286.json', import.meta.url));
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(fixturePath, 'utf8'), output,
    'Tool turn-end fixture differs from the executed native source');
} else {
  mkdirSync(dirname(fixturePath), { recursive: true });
  writeFileSync(fixturePath, output);
}
const mapped = cases.filter(test => test.domain === 'single_result_boolean').length;
console.log(`Verified native tool turn-end fixture: ${mcpMarkerCases.length} MCP checks, ${mapped} mapped bool cases, ${cases.length - mapped} raw-frame cases (${binarySha256})`);
