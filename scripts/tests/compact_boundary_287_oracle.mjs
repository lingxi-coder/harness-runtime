#!/usr/bin/env node
// Usage: node scripts/tests/compact_boundary_287_oracle.mjs /path/to/2.1.287 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
const binary = readFileSync(process.argv[2]);
const nativeSha256 = createHash('sha256').update(binary).digest('hex');
assert.equal(nativeSha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const start = binary.indexOf('function Bi(', 185762130);
const end = binary.indexOf('function Tln(', start);
assert.equal(start, 189915132);
assert.equal(end, 189915421);
const bytes = binary.subarray(start, end);
const sourceSha256 = createHash('sha256').update(bytes).digest('hex');
assert.equal(sourceSha256, '7aca14849e79f2eb99182646db1adb69c158e994b1d0702615fa62b4d3801c40');
const native = vm.runInNewContext(`(()=>{${bytes.toString('utf8')};return {Bi,Pye,hdt,ai};})()`);
const prefix = { type: 'user', content: 'before' };
const tail = { type: 'user', content: 'after' };
const rows = [
  ['ordinary-system-same-body', { type: 'system', content: 'Conversation compacted' }],
  ['ordinary-system-null-subtype', { type: 'system', subtype: null, content: 'Conversation compacted' }],
  ['other-system-subtype-same-body', { type: 'system', subtype: 'note', content: 'Conversation compacted' }],
  ['typed-boundary-arbitrary-body', { type: 'system', subtype: 'compact_boundary', content: 'arbitrary body' }],
  ['typed-boundary-standard-body', { type: 'system', subtype: 'compact_boundary', content: 'Conversation compacted' }],
  ['user-compact-summary', { type: 'user', isCompactSummary: true, content: 'summary' }],
];
const normalize = value => JSON.parse(JSON.stringify(value));
const cases = rows.map(([name, row]) => {
  const messages = [prefix, row, tail];
  return { name, input: { messages }, expected: normalize({
    boundaries: messages.map(native.Bi), artifacts: messages.map(native.Pye),
    boundaryIndex: native.hdt(messages), visible: native.ai(messages),
  }) };
});
assert.deepEqual(cases.map(test => test.expected.boundaryIndex), [-1, -1, -1, 1, 1, -1]);
const output = {
  version: '2.1.287', native_sha256: nativeSha256,
  sourceByteRanges: [{ start, end, sha256: sourceSha256 }],
  scope: 'Unchanged actual native Bi/Pye/hdt/ai bodies, with controlled message records. No subtype or body compatibility substitution. This is Node helper execution, not native CLI, full message-schema validation or Bun acceptance.',
  cases,
};
const fixture = fileURLToPath(new URL('../../crates/compaction/tests/fixtures/compact_boundary_2_1_287.json', import.meta.url));
const json = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(fixture, 'utf8'), json);
  process.stdout.write(`checked native 2.1.287 typed boundary oracle: ${cases.length} cases\n`);
} else {
  mkdirSync(path.dirname(fixture), { recursive: true });
  writeFileSync(fixture, json);
  process.stdout.write(`wrote native 2.1.287 typed boundary oracle: ${cases.length} cases\n`);
}
