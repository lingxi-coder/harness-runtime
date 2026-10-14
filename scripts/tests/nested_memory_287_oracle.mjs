#!/usr/bin/env node
// Execute exact current native schema, history/dedup and attachment rendering.
// Usage: node scripts/tests/nested_memory_287_oracle.mjs /path/to/2.1.287 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

assert(process.argv[2], 'Pass the current native binary');
const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
assert.equal(sha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges = [];
function chunk(offset) {
  const marker = '// @bun @bytecode\n';
  assert.equal(binary.subarray(offset, offset + marker.length).toString(), marker);
  const end = binary.indexOf(marker, offset + marker.length);
  const bytes = binary.subarray(offset, end < 0 ? binary.length : end);
  const nul = bytes.indexOf(0, Math.max(bytes.lastIndexOf('export{'), 0));
  return bytes.subarray(0, nul < 0 ? bytes.length : nul);
}
function record(bytes, start, label) {
  sourceByteRanges.push({ label, start, end: start + bytes.length,
    sha256: createHash('sha256').update(bytes).digest('hex') });
  return bytes.toString('utf8');
}
function range(offset, startMarker, endMarker, label = startMarker) {
  const bytes = chunk(offset);
  const start = bytes.indexOf(startMarker);
  const end = bytes.indexOf(endMarker, start + startMarker.length);
  assert(start >= 0 && end > start, `Missing actual native range: ${label}`);
  return record(bytes.subarray(start, end), offset + start, label);
}
function pureModule(offset, imports = {}) {
  const source = record(chunk(offset), offset, 'native pure schema dependency')
    .replace(/import\{[^}]+\}from"[^"]+";/g, '')
    .replace(/export\{([^}]+)\};/, 'return {$1};');
  return vm.runInNewContext(`(()=>{${source}})()`, imports, { timeout: 2_000 });
}
const schemaCore = pureModule(178308215);
const schema = pureModule(178319858, schemaCore);
const lazy = pureModule(178237228);
const source = [
  range(185762130, 'var T3=', 'function aN('),
  range(185762130, 'var C3=', 'function iAo('),
  range(185762130, 'var nor=', 'var oor='),
  range(185762130, 'function m9e(', 'function aAo('),
  range(185762130, 'function hdt(', 'function Tln('),
  range(185762130, 'function a1e(', 'function WXt('),
  range(185762130, 'function Trr(', 'async function m7t('),
  range(185762130, 'async function m7t(', 'async function O7t('),
  range(185762130, 'async function O7t(', 'async function*Err('),
  range(185762130, 'async function tzn(', 'import{isAbsolute as uKn'),
  range(181978701, 'function Ww(', 'function o6o('),
  range(185762130, 'function Re(', 'function L6('),
  range(185762130, 'function cl(', 'function Hrn('),
  range(185762130, 'function ya(', 'function i_r('),
  range(178136358, 'var pD=', ',aet=') + ';',
].join('\n');
const renderer = range(185762130, 'nested_memory:(e)=>', ',read_truncation_notice:');
const normalize = value => JSON.parse(JSON.stringify(value));
class FixtureDate extends Date {
  constructor(...args) { super(...(args.length ? args : ['2026-10-01T00:00:00.000Z'])); }
  static now() { return 1_000; }
}
function host(test) {
  const hookEvents = [], statCalls = [], errors = [];
  let uuid = 0;
  const native = vm.runInNewContext(`(()=>{${source};
    return {O7t,m7t,l1e,render: e=>({${renderer}}).nested_memory(e)};
  })()`, {
    ...schema, ...lazy, Map, Set, Date: FixtureDate,
    // Context acquisition and live feature/hook/filesystem sources are the
    // substitutes. Native schema parsing and all selected helpers are intact.
    Bi: row => row.type === 'system' && row.subtype === 'compact_boundary',
    Aat: async () => test.currentInstructions ?? [], F: () => false,
    yGn: () => true, Eb: async filename => { statCalls.push(filename); return 77; },
    pv: path.relative, oe: () => '/repo',
    fv: (filename, root) => filename === root || filename.startsWith(root + '/'),
    UXt: path.join, HXt: path.relative,
    Wan: (_session, filename, type, reason, extra) => {
      hookEvents.push(normalize({ filename, type, reason, extra }));
    },
    d: error => errors.push(String(error)),
    hg: () => `fixture-message-${++uuid}`,
  }, { timeout: 2_000 });
  return { native, hookEvents, statCalls, errors };
}
const file = {
  path: '/repo/pkg/LINGXI.md', type: 'Project', content: '  Folder guide\n',
  contentDiffersFromDisk: false,
};
const prior = { type: 'attachment', uuid: 'fixture-prior', attachment: {
  type: 'nested_memory', path: file.path, content: file, displayPath: 'pkg/LINGXI.md',
} };
const boundary = { type: 'system', subtype: 'compact_boundary', uuid: 'fixture-boundary' };
const tests = [
  { name: 'resume-unchanged-body', history: [prior], files: [file] },
  { name: 'compaction-retained-body', history: [boundary, prior], files: [file] },
  { name: 'compaction-discarded-body', history: [prior, boundary], files: [file] },
  { name: 'resume-changed-body', history: [prior], files: [{ ...file,
    content: 'New folder guide\n', globs: ['pkg'], contentDiffersFromDisk: true,
    rawContent: '---\npaths: pkg/**\n---\nNew folder guide\n',
    parent: '/repo/LINGXI.md',
  }] },
  { name: 'read-for-unchanged-body', history: [prior], files: [file], readFor: true },
  { name: 'read-for-new-body-batch-dedup', history: [], files: [file, file], readFor: true },
];
const acquisitionPath = fileURLToPath(new URL('../../crates/orchestrator/tests/fixtures/instruction_eager_2_1_287.json', import.meta.url));
const acquisitionBytes = readFileSync(acquisitionPath);
const acquisition = JSON.parse(acquisitionBytes);
assert.equal(acquisition.native_sha256, sha256);
for (const name of [
  'import-crlf-body-keeps-native-disk-content',
  'import-crlf-comment-body-uses-marked-sanitization',
  'path-gated-body-keeps-trailing-newline',
]) {
  const acquired = acquisition.cases.find(test => test.name === name);
  assert(acquired, `Missing actual native acquisition case ${name}`);
  tests.push({ name: `native-acquired-${name}`, history: [],
    files: acquired.expected.acquired, acquisitionCase: name });
}
const cases = [];
for (const test of tests) {
  const input = { trigger: '/repo/pkg/code.rs', readFor: false, ...normalize(test) };
  const { native, hookEvents, statCalls, errors } = host(input);
  const ctx = { session: {}, options: {}, readFileState: new Map };
  const baseline = await native.O7t(ctx, input.history);
  const attachments = await native.m7t(input.files, ctx, input.trigger, baseline, null, input.readFor);
  cases.push({ name: test.name, input, expected: normalize({
    attachments,
    rendered: attachments.flatMap(attachment => native.render(attachment).map(row => row.message.content)),
    readState: [...ctx.readFileState].map(([path, entry]) => ({ path, entry })),
    hooks: hookEvents, statCalls, errors, nested: [...baseline.nested],
  }) });
}
assert.deepEqual(cases.map(test => test.expected.attachments.length), [0, 0, 1, 1, 0, 1, 2, 2, 1]);
assert.deepEqual(cases.map(test => test.expected.readState.length), [1, 1, 1, 1, 0, 0, 2, 2, 1]);
const output = {
  version: '2.1.287', native_sha256: sha256, sourceByteRanges,
  acquisitionOracle: { path: 'instruction_eager_2_1_287.json',
    sha256: createHash('sha256').update(acquisitionBytes).digest('hex') },
  scope: 'Actual native l1e/O7t/m7t/ai/csn/a1e, pure native schemas, Re/ya/cl/nested_memory rendering and Ww/jE/tzn. Filesystem stats, feature/hook sources, UUID/clock and compact-boundary predicate are controlled. Acquisition is controlled except the three named native-acquired cases chained from the separately hashed actual g7/marked/Bun helper oracle. This is a Node helper oracle, not native CLI or Bun acceptance. Literal paths; explicit worktree topology is absent. Filenames use the permitted LingXi branding difference.',
  cases,
};
const fixture = fileURLToPath(new URL('../../crates/orchestrator/tests/fixtures/nested_memory_2_1_287.json', import.meta.url));
const serialized = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(fixture, 'utf8'), serialized, 'Native nested-memory fixture differs');
  process.stdout.write(`checked native 2.1.287 nested-memory oracle: ${cases.length} cases\n`);
} else {
  mkdirSync(dirname(fixture), { recursive: true });
  writeFileSync(fixture, serialized);
  process.stdout.write(`wrote native 2.1.287 nested-memory oracle: ${cases.length} cases\n`);
}
