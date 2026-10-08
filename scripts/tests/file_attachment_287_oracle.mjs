#!/usr/bin/env node
// Usage: node scripts/tests/file_attachment_287_oracle.mjs /path/to/2.1.287 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
const binary = readFileSync(process.argv[2]);
const nativeSha256 = createHash('sha256').update(binary).digest('hex');
assert.equal(nativeSha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges = [];
function take(offset, startMarker, endMarker) {
  const start = binary.indexOf(startMarker, offset);
  const end = binary.indexOf(endMarker, start + startMarker.length);
  assert(start >= offset && end > start, startMarker);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({ start, end, startMarker, endMarker,
    sha256: createHash('sha256').update(bytes).digest('hex') });
  return bytes.toString('utf8');
}
const readDefinition = binary.indexOf('Sv=Wt(', 185762130);
assert(readDefinition > 185762130);
const mapper = take(readDefinition, 'mapToolResultToToolResultBlockParam(', '});async function AKn(');
const branch = take(185762130, 'case"file":{let s=e.content;', 'case"invoked_skills":');
const reference = take(185762130, 'compact_file_reference:(e)=>', ',pdf_reference:');
const source = [
  take(185762130, 'function Re(', 'function L6('),
  take(185762130, 'function a6(', 'function Nt('),
  take(185762130, 'function cl(', 'function Hrn('),
  take(185762130, 'function ya(', 'function i_r('),
  take(185762130, 'var Jgt=', 'async function MKn('),
  take(178898604, 'function xZe(', 'function j8o('),
  take(178136358, 'var pD=', ',aet=') + ';',
].join('\n');
let ordinal = 0;
class FixtureDate extends Date {
  constructor(...args) { super(...(args.length ? args : ['2026-10-01T00:00:00.000Z'])); }
}
const native = vm.runInNewContext(`(()=>{${source};
  const Sv={name:dt,${mapper}};
  const refs={${reference}};
  return {render:e=>{if(e.type==='compact_file_reference')return refs.compact_file_reference(e);switch(e.type){${branch}}}};
})()`, {
  Map, Set, WeakMap, Date: FixtureDate,
  dt: 'Read', _: JSON.stringify, hg: () => `controlled-${++ordinal}`,
  E7e: () => false,
  Yh: filename => filename,
});
const tests = [
  ['plain-file', 'Folder guidance', 1, 1],
  ['untrimmed-multiline-file', ' \nFolder guidance\n\t', 3, 3],
  ['crlf-file', 'one\r\ntwo\r\n', 3, 3],
  ['empty-file', '', 0, 0],
  ['empty-slice-at-offset', '', 1, 5],
];
const normalize = value => JSON.parse(JSON.stringify(value));
const cases = tests.map(([name, content, numLines, totalLines]) => {
  const input = { type: 'file', filename: '/repo/pkg/LINGXI.md',
    content: { type: 'text', file: { filePath: '/repo/pkg/LINGXI.md', content,
      numLines, startLine: 1, totalLines } }, displayPath: 'pkg/LINGXI.md' };
  const rendered = native.render(input);
  return { name, input, expected: normalize({
    rendered: rendered.map(row => row.message.content),
  }) };
});
assert(cases.every(test => test.expected.rendered.length === 2));
const referenceInput = { type: 'compact_file_reference', filename: '/repo/pkg/LINGXI.md', displayPath: 'pkg/LINGXI.md' };
cases.push({ name: 'compact-file-reference', input: referenceInput,
  expected: normalize({ rendered: native.render(referenceInput).map(row => row.message.content) }) });
const output = {
  version: '2.1.287', native_sha256: nativeSha256, sourceByteRanges,
  scope: 'Actual unchanged file switch branch, compact_file_reference renderer, Sv Read mapper, a6/l6/Re/ya/cl and xZe/grr. JSON serializer, path display formatter, UUID/clock and current E7e default false are controlled. No I/O or Read-side weak-map metadata. This is Node helper execution, not native CLI or embedded Bun acceptance. Text-file attachments without truncation; other media, truncation and non-default tab separators are outside the fixture scope.',
  cases,
};
const target = fileURLToPath(new URL('../../crates/orchestrator/tests/fixtures/file_attachment_2_1_287.json', import.meta.url));
const json = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(target, 'utf8'), json);
  process.stdout.write(`checked native 2.1.287 file renderer oracle: ${cases.length} cases\n`);
} else {
  mkdirSync(path.dirname(target), { recursive: true });
  writeFileSync(target, json);
  process.stdout.write(`wrote native 2.1.287 file renderer oracle: ${cases.length} cases\n`);
}
