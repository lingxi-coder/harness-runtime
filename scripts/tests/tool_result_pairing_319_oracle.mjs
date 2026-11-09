#!/usr/bin/env node
// Evaluate the shipped 2.1.319 pairing function without reimplementing it.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: tool_result_pairing_319_oracle.mjs <2.1.319 binary> [--check] [--evidence-dir <dir>]');
const binary = readFileSync(process.argv[2]);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const binarySha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(sha256(binary), binarySha256, 'The oracle requires the pinned native artifact');
const ranges = [];
function take(start, end, label) {
  const source = binary.subarray(start, end);
  ranges.push({ label, start, end_exclusive: end, sha256: sha256(source) });
  return source.toString('utf8');
}
const source = [
  take(259365908, 259365929, 'NO_CONTENT_MESSAGE literal') + ';',
  take(274506447, 274506500, 'missing tool-result literal') + ';',
  take(274527076, 274529181, 'createUserMessage'),
  take(274691163, 274691209, 'misplaced assistant tool-result predicate'),
  take(274691209, 274695064, 'ensureToolResultPairing'),
].join('\n');
let uuid = 0;
const context = vm.createContext({
  // The exact creator is evaluated. Only nondeterministic identifiers, clocks,
  // classifier sidecars, telemetry and the strict-mode host gate are stubbed.
  Kk: () => `oracle-generated-${++uuid}`,
  Date: class extends Date { constructor(...args) { super(...(args.length ? args : ['2026-11-09T00:00:00.000Z'])); } },
  j7: () => undefined,
  Zme: () => false,
  ZHi: () => undefined,
  Ycn: () => false,
  l: () => {},
  t: () => {},
  S: value => value,
  So: value => String(value),
  Ap: (...values) => values.join(''),
  _e: value => value,
  f: value => value,
  PE: (values, separator = ',') => values.join(separator),
  ccn: values => values.join(','),
  A7: () => { throw new Error('Every oracle case explicitly selects reminderFold:false'); },
});
vm.runInContext(source, context, { timeout: 2000 });

const text = value => ({ type: 'text', text: value });
const thinking = value => ({ type: 'thinking', thinking: value, signature: `sig-${value}` });
const redacted = value => ({ type: 'redacted_thinking', data: value });
const use = id => ({ type: 'tool_use', id, name: 'Read', input: { file_path: '/repo/a' } });
const result = (id, content = 'ok') => ({ type: 'tool_result', tool_use_id: id, content });
const server = (id, name = 'web_search') => ({ type: 'server_tool_use', id, name, input: { query: 'test' } });
let row = 0;
const user = (content, extra = {}) => ({ type: 'user', uuid: `user-${++row}`, message: { role: 'user', content }, ...extra });
const assistant = content => ({ type: 'assistant', uuid: `assistant-${++row}`, message: { role: 'assistant', id: `msg-${row}`, content } });
const cases = [
  { name: 'clean_pair_is_identity', input: [user('hello'), assistant([use('a')]), user([result('a')])] },
  { name: 'missing_results_follow_tool_use_order', input: [assistant(['z', 'a', 'm', 'b'].map(use))] },
  { name: 'missing_results_precede_existing_content', input: [assistant(['z', 'a', 'm', 'b'].map(use)), user([text('after'), result('m')])] },
  { name: 'leading_orphan_becomes_placeholder', input: [user([result('gone')])] },
  { name: 'leading_orphan_preserves_text', input: [user([result('gone'), text('keep')])] },
  { name: 'later_orphan_only_user_is_removed', input: [user('hello'), user([result('gone')])] },
  { name: 'duplicate_call_between_thinking_gets_spacer', input: [assistant([use('a')]), user([result('a')]), assistant([thinking('before'), use('a'), redacted('after')])] },
  { name: 'orphan_server_between_thinking_gets_spacer', input: [assistant([thinking('before'), server('gone'), thinking('after')])] },
  { name: 'misplaced_result_between_thinking_gets_spacer', input: [assistant([thinking('before'), result('gone'), redacted('after')])] },
  { name: 'misplaced_result_only_becomes_interrupted', input: [assistant([result('gone')])] },
  { name: 'removed_adjacent_calls_do_not_invent_spacer', input: [assistant([thinking('before'), server('gone-1'), server('gone-2'), thinking('after')])] },
  { name: 'hosted_web_result_retains_server_use', input: [assistant([server('web'), { type: 'web_search_tool_result', tool_use_id: 'web', content: [] }])] },
  { name: 'hosted_mcp_result_retains_mcp_use', input: [assistant([{ type: 'mcp_tool_use', id: 'mcp', name: 'search', server_name: 'example', input: {} }, { type: 'mcp_tool_result', tool_use_id: 'mcp', content: [text('found')] }])] },
  { name: 'orphan_mcp_use_is_removed_with_spacer', input: [assistant([redacted('before'), { type: 'mcp_tool_use', id: 'gone', name: 'search', server_name: 'example', input: {} }, thinking('after')])] },
  { name: 'advisor_result_retains_its_server_use', input: [assistant([server('advisor', 'advisor'), { type: 'advisor_tool_result', tool_use_id: 'advisor', content: [text('advice')], is_error: false }])] },
  { name: 'any_nonclient_result_identity_retains_server_use', input: [assistant([server('hosted'), { type: 'future_hosted_result', tool_use_id: 'hosted', content: null }])] },
  { name: 'misplaced_client_result_does_not_pair_server_use', input: [assistant([server('gone'), result('gone')])] },
  { name: 'duplicate_and_orphan_results_are_removed', input: [assistant([use('a')]), user([result('orphan'), result('a', 'first'), text('tail'), result('a', 'duplicate')])] },
  { name: 'orphan_only_after_assistant_becomes_meta_placeholder', input: [assistant([text('answer')]), user([result('orphan')])] },
  { name: 'repaired_user_preserves_shutdown_and_refusal_metadata', input: [assistant([use('a'), use('b')]), user([result('a')], { interruptedByShutdown: true, refusedMessageId: 'refused-id', isMeta: true })] },
  { name: 'empty_assistant_remains_empty', input: [assistant([])] },
  { name: 'empty_assistant_stays_empty_when_other_rows_repair', input: [user([result('orphan')]), assistant([])] },
  { name: 'raw_client_tool_use_retains_fields_and_pairs', raw_blocks: [[0, 0]], input: [assistant([{ ...use('raw'), caller: { type: 'direct' } }])] },
  { name: 'raw_duplicate_tool_use_is_removed', raw_blocks: [[2, 1]], input: [assistant([use('a')]), user([result('a')]), assistant([thinking('before'), { ...use('a'), caller: { type: 'direct' } }, thinking('after')])] },
  { name: 'raw_client_result_keeps_first_and_removes_duplicate', raw_blocks: [[1, 0], [1, 1]], input: [assistant([use('a')]), user([{ ...result('a'), extra_field: 'kept' }, { ...result('a'), extra_field: 'duplicate' }])] },
  { name: 'raw_misplaced_result_is_removed', raw_blocks: [[0, 1]], input: [assistant([thinking('before'), { ...result('gone'), extra_field: 'raw' }, thinking('after')])] },
  { name: 'raw_thinking_neighbors_receive_spacer', raw_blocks: [[0, 0], [0, 2]], input: [assistant([{ ...thinking('before'), extra_field: 'raw' }, result('gone'), { ...redacted('after'), extra_field: 'raw' }])] },
  { name: 'raw_result_without_identity_is_removed', raw_blocks: [[1, 0]], input: [assistant([use('a')]), user([{ type: 'tool_result', content: 'invalid' }])] },
];

function project(messages) {
  return messages.map(message => ({
    role: message.type,
    content: typeof message.message.content === 'string' ? [text(message.message.content)] : message.message.content,
    ...(message.type === 'user' ? {
      is_meta: message.isMeta === true,
      ...(message.interruptedByShutdown !== undefined ? { interrupted_by_shutdown: message.interruptedByShutdown } : {}),
      ...(message.refusedMessageId !== undefined ? { refused_message_id: message.refusedMessageId } : {}),
    } : {}),
  }));
}
for (const testCase of cases) {
  context.input = structuredClone(testCase.input);
  const result = JSON.parse(JSON.stringify(vm.runInContext('tCn(input, {reminderFold:false})', context, { timeout: 2000 })));
  testCase.native_expected = result;
  testCase.expected = project(result);
}
// Store exact documents as JSON strings so consumers never parse source IDs
// through serde_json::Value's scalar-only string representation.
const sourceCases = [
  { name: 'distinct_lone_client_ids', input: [assistant([use('\ud800'), use('\udfff')])], wire: true },
  { name: 'lone_and_replacement_client_ids_are_distinct', input: [assistant([use('\ud800'), use('\ufffd')])], wire: false },
  { name: 'wrong_lone_result_is_orphaned', input: [assistant([use('\ud800')]), user([result('\udfff', 'wrong')], { interruptedByShutdown: true, refusedMessageId: 'refused\ud800', isMeta: true })], wire: true },
  { name: 'matching_lone_result_preserves_source_fields', input: [assistant([{ ...use('\ud800'), provider_metadata: { label: '\udfff' } }]), user([{ ...result('\ud800', 'body\udfff'), provider_metadata: { label: '\ud800' } }])], wire: true },
  { name: 'duplicate_lone_id_preserves_first_and_thinking_spacer', input: [assistant([use('\ud800')]), user([result('\ud800')]), assistant([thinking('before'), use('\ud800'), redacted('after')])], wire: true },
  { name: 'duplicate_lone_result_preserves_first_source_fields', input: [assistant([use('\ud800')]), user([{ ...result('\ud800'), provider_metadata: { label: 'first\udfff' } }, { ...result('\ud800'), provider_metadata: { label: 'duplicate\ud800' } }])], wire: true },
  { name: 'wrong_lone_hosted_result_does_not_pair_server_use', input: [assistant([server('\ud800'), { type: 'web_search_tool_result', tool_use_id: '\udfff', content: [] }])], wire: false },
  { name: 'matching_lone_hosted_result_preserves_mcp_use', input: [assistant([{ type: 'mcp_tool_use', id: '\ud800', name: 'search', server_name: 'example', input: {} }, { type: 'mcp_tool_result', tool_use_id: '\ud800', content: [text('found\udfff')] }])], wire: false },
].map(({ name, input, wire }) => {
  context.input = structuredClone(input);
  const result = vm.runInContext('tCn(input, {reminderFold:false})', context, { timeout: 2000 });
  const projected = project(result);
  return {
    name, wire, input_json: JSON.stringify(input), expected_rows_json: JSON.stringify(projected),
    expected_sdk_messages_json: JSON.stringify(projected.map(({ role, content }) => ({ role, content }))),
  };
});
const fixture = {
  version: '2.1.319', binary_sha256: binarySha256, source_byte_ranges: ranges,
  scope: 'Native ensureToolResultPairing with reminderFold:false; strings project to protocol text-block arrays; generated UUID/time and classifier sidecars are excluded from Rust comparison.',
  host_stubs: ['UUID and clock', 'classifier metadata lookups', 'telemetry renderers/sinks', 'strict pairing gate=false'],
  cases, source_cases: sourceCases,
};
const fixturePath = fileURLToPath(new URL('../../crates/llm-runtime/tests/fixtures/tool_result_pairing_2_1_319.json', import.meta.url));
const output = JSON.stringify(fixture, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(fixturePath, 'utf8'), output, 'Native pairing fixture differs');
} else {
  mkdirSync(dirname(fixturePath), { recursive: true });
  writeFileSync(fixturePath, output);
}
const evidenceArg = process.argv.indexOf('--evidence-dir');
if (evidenceArg !== -1) {
  const directory = resolve(process.argv[evidenceArg + 1]);
  mkdirSync(directory, { recursive: true });
  writeFileSync(resolve(directory, 'native_pairing_2_1_319.js'), source);
  writeFileSync(resolve(directory, 'pairing_source_ranges.json'), JSON.stringify(ranges, null, 2) + '\n');
}
console.log(JSON.stringify({ version: fixture.version, cases: cases.length + sourceCases.length, legacy_cases: cases.length, source_cases: sourceCases.length, source_ranges: ranges.length, checked: process.argv.includes('--check') }));
