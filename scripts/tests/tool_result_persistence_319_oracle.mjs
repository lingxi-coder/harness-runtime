#!/usr/bin/env node
// Execute the shipped tool-result persistence guards and Unicode helpers.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: node tool_result_persistence_319_oracle.mjs <official 2.1.319 binary> [--check]');
const binary = fs.readFileSync(binaryPath);
const hash = value => createHash('sha256').update(value).digest('hex');
const artifactSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(hash(binary), artifactSha256, 'Use the pinned official darwin-arm64 artifact');
const sourceRanges = [];
function take(label, offset, first, after) {
  const start = binary.indexOf(first, offset);
  const endExclusive = binary.indexOf(after, start + first.length);
  assert(start >= offset && endExclusive > start && endExclusive - start < 20000, label);
  const bytes = binary.subarray(start, endExclusive);
  sourceRanges.push({ label, start, endExclusive, sha256: hash(bytes) });
  return bytes.toString('utf8');
}
const writes = [];
let writeError;
const world = vm.createContext({
  Buffer, TextEncoder,
  pe: path.join, Y: path.dirname, ce: path.basename,
  Aa: value => value !== null && typeof value === 'object' && !Array.isArray(value),
  Fv: () => false,
  Bg: () => undefined, X: () => false,
  bM: async () => {}, Ozn: async () => {}, uBs: async () => {},
  On: () => ({ writeExclusive: async (filepath, body) => {
    if (writeError !== undefined) throw Object.assign(Error('fixture write failure'), { code: writeError });
    writes.push({ filepath, body });
  } }),
  N: error => error, E: error => error?.code,
  _: JSON.stringify, k: (_key, fallback) => fallback,
  l: () => {}, M: () => {}, t: () => {}, In: value => value,
});
vm.runInContext([
  take('native persistence constants', 261514806, 'var mC=', '\nexport{'),
  take('native surrogate-safe prefix', 259201128, 'function Q(', 'function Pa('),
  take('native UTF-16 roundtrip', 259201128, 'function Ocn(', 'function m('),
  take('native UTF-8 byte cap', 259207451, 'function v4(', 'function l$r('),
  take('native size formatter', 260585809, 'function en(', 'function fcn('),
  take('native persistence implementation', 267397971, 'var Hoe=', 'function DSr('),
  take('native media and size guards', 267397971, 'function Oms(', 'function Te('),
  take('native filesystem error formatter', 267397971, 'function b(e){', 'async function HCe('),
].join('\n'), world, { timeout: 2000 });
const units = value => Array.from({ length: value.length }, (_, i) => value.charCodeAt(i));
const previewCases = [];
for (const [name, input, limit] of [
  ['short-pair', '😀ab', 1],
  ['complete-pair', '😀ab', 2],
  ['high-surrogate-at-preview-edge', 'a'.repeat(1999) + '😀tail', 2000],
  ['lone-high-at-preview-edge', 'a\ud800z', 2],
  ['lone-high-in-short-result', 'a\ud800', 2],
  ['lone-high-inside-preview', 'a\ud800bc', 3],
  ['lone-low-at-preview-edge', 'a\udfffz', 2],
  ['newline-after-midpoint', 'aaaa\nbcdef', 6],
  ['newline-at-midpoint', 'aaa\nbcdef', 6],
  ['zero-limit', 'x', 0],
]) {
  const result = world.LCe(input, limit);
  previewCases.push({ name, inputUnits: units(input), limit, expectedPreviewUnits: units(result.preview), hasMore: result.hasMore });
}
const capCases = [];
for (const [name, input, capBytes] of [
  ['ascii', 'abcd', 3],
  ['cjk-does-not-fit', '中a', 2],
  ['cjk-at-byte-cap', '中a', 3],
  ['pair-does-not-fit', 'a😀b', 4],
  ['pair-at-byte-cap', 'a😀b', 5],
  ['lone-high-does-not-fit', 'a\ud800z', 3],
  ['lone-high-at-byte-cap', 'a\ud800z', 4],
  ['lone-low-at-byte-cap', '\udfffz', 3],
  ['isolated-high-followed-by-pair', '\ud800😀z', 7],
  ['exact-byte-size', '中😀', 7],
  ['zero-cap', 'x', 0],
]) {
  writes.length = 0;
  const result = await world.LK(input, 'toolu_fixture', '/fixture/tool-results', undefined, capBytes);
  assert.equal(writes.length, 1, name);
  const body = writes[0].body;
  capCases.push({ name, inputUnits: units(input), capBytes, expectedWrittenUnits: units(body), expectedFileHex: Buffer.from(body).toString('hex'), originalSize: result.originalSize, truncatedAtBytes: result.truncatedAtBytes ?? null, expectedPreviewUnits: units(result.preview), hasMore: result.hasMore });
}
const blankCases = [];
for (const [name, input] of [
  ['bom', '\ufeff'], ['next-line', '\u0085'], ['empty', ''], ['ordinary-whitespace', ' \t\r\n'],
  ['empty-block-array', []], ['bom-text-block', [{ type: 'text', text: '\ufeff' }]],
  ['next-line-text-block', [{ type: 'text', text: '\u0085' }]],
  ['non-string-text-block', [{ type: 'text', text: 7 }]],
]) blankCases.push({ name, input, expectedBlank: world.xms(input) });
const dispatchCases = [];
for (const [name, input, threshold] of [
  ['at-threshold', 'x'.repeat(100), 100],
  ['over-threshold', 'x'.repeat(101), 100],
  ['text-array', [{ type: 'text', text: 'x'.repeat(101) }], 100],
  ['resource-link-array', [{ type: 'text', text: 'x'.repeat(101) }, { type: 'resource_link', uri: 'https://example.invalid/result' }], 100],
  ['tool-reference-array', [{ type: 'text', text: 'x'.repeat(101) }, { type: 'tool_reference', tool_name: 'Deferred' }], 100],
  ['primitive-in-array', [{ type: 'text', text: 'x'.repeat(101) }, 7], 100],
  ['image-array', [{ type: 'text', text: 'x'.repeat(101) }, { type: 'image', source: {} }], 100],
]) {
  writes.length = 0;
  const result = await world._e({ tool_use_id: 'toolu_fixture', type: 'tool_result', content: input }, 'Fixture', '/fixture/tool-results', threshold);
  dispatchCases.push({ name, input, threshold, expectedContent: result.content, stored: writes.length !== 0 });
}
const normalStorageFailures = [];
for (const code of ['ENOSPC', 'EACCES', 'EIO']) {
  writes.length = 0;
  writeError = code;
  const input = 'x'.repeat(101);
  const result = await world._e({ tool_use_id: 'toolu_fixture', type: 'tool_result', content: input }, 'Fixture', '/fixture/tool-results', 100);
  assert.equal(writes.length, 0, code);
  assert.equal(result.content, input, code);
  normalStorageFailures.push({ code, input, threshold: 100, expectedContent: result.content });
}
writeError = undefined;
const fixture = {
  version: '2.1.319', artifactSha256, sourceRanges,
  boundary: 'Unmodified shipped LK/_e/LCe/xms/Q/Ocn/v4 execute in an isolated VM. Local storage writes are captured in memory; the configured host storage is absent. No application bootstrap, provider, credentials, real filesystem mutation, or feature service executes. Filename cases use ordinary safe provider IDs.',
  previewCases, capCases, blankCases, dispatchCases, normalStorageFailures,
};
const destination = fileURLToPath(new URL('../../crates/orchestrator/tests/fixtures/tool_result_persistence_319.json', import.meta.url));
const serialized = JSON.stringify(fixture, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(destination, 'utf8'), serialized, 'native persistence fixture drift');
else fs.writeFileSync(destination, serialized);
console.log(`Claude Code 2.1.319 tool-result persistence: ${previewCases.length + capCases.length + blankCases.length + dispatchCases.length + normalStorageFailures.length} cases ${process.argv.includes('--check') ? 'matched' : 'written'}`);
