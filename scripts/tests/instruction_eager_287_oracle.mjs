#!/usr/bin/env bun
// Use Bun to execute the native frontmatter parser's actual Bun.YAML binding.
// Usage: bun scripts/tests/instruction_eager_287_oracle.mjs /path/to/2.1.287 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';
assert(globalThis.Bun?.YAML, 'Run this oracle with Bun for the real YAML intrinsic');
const binary = readFileSync(process.argv[2]);
const nativeSha256 = createHash('sha256').update(binary).digest('hex');
assert.equal(nativeSha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges = [];
function take(offset, startMarker, endMarker) {
  const start = binary.indexOf(startMarker, offset);
  const end = binary.indexOf(endMarker, start + startMarker.length);
  assert(start >= offset && end > start);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({ start, end, startMarker, endMarker,
    sha256: createHash('sha256').update(bytes).digest('hex') });
  return bytes.toString('utf8');
}
const lexerSource = take(184171613, 'function Lw()', 'class _d{');
const frontmatterSource = [
  take(181978701, 'function Ww(', 'function jE('),
  take(181978701, 'function DH(', 'function z('),
  take(181978701, 'var gQ=', 'function dht('),
  take(181978701, 'function dht(', 'function oXn('),
].join('\n');
const source = [
  take(185762130, 'var h0n=', 'function zI('),
  take(185762130, 'function y0n(', 'function xTo('),
  take(185762130, 'function cEe(', 'var dat='),
  take(185762130, 'function v0n(', 'var T0n='),
  take(185762130, 'function _at(', 'async function $Ne('),
  take(185762130, 'async function $Ne(', 'function C0n('),
  take(185762130, 'async function mat(', 'function v0n('),
  take(185762130, 'function b0n(', 'async function w0n('),
  take(185762130, 'function xJ(', 'async function Eat('),
  take(185762130, 'function iAo(', 'function g9e('),
  take(185762130, 'async function M0n(', 'function TEe('),
  take(185762130, 'function TEe(', 'function TA('),
  take(181324307, 'var ws=', 'function Ye('),
].join('\n');
const { Rq } = vm.runInNewContext(`(()=>{${lexerSource};return {Rq};})()`);
const { Hs, dht } = vm.runInNewContext(`(()=>{${frontmatterSource};return {Hs,dht};})()`, {Bun, t:()=>{}});
const normalize = value => JSON.parse(JSON.stringify(value));
const named = '/repo/LINGXI.md', rules = '/repo/.lingxi/rules', child = '/repo/imported.md';
const tests = [
  { name: 'named-whitespace-only', raw: ' \t\r\n\n', origin: 'named' },
  { name: 'named-frontmatter-only', raw: '---\npaths: src/**\n---\n', origin: 'named' },
  { name: 'named-frontmatter-whitespace-body', raw: '---\npaths: src/**\n---\n \t\r\n\n', origin: 'named' },
  { name: 'named-with-globs-renders-eagerly', raw: '---\npaths: src/**\n---\n  Named guide\n', origin: 'named' },
  { name: 'conditional-rule-keeps-unconditional-import', raw: '---\npaths: src/**\n---\n@../../imported.md\n', childRaw: '  Imported guide\n', origin: 'rules' },
  { name: 'named-import-keeps-globbed-child', raw: 'Named guide\n@./imported.md\n', childRaw: '---\npaths: src/**\n---\n  Imported guide\n', origin: 'named' },
  { name: 'named-bom-without-frontmatter-preserves-body', raw: '\uFEFF  Named guide\n', origin: 'named' },
  { name: 'named-bom-frontmatter-detects-paths', raw: '\uFEFF---\npaths: src/**\n---\n  Named guide\n', origin: 'named' },
  { name: 'import-crlf-body-keeps-native-disk-content', raw: ' \nparent guidance\n@imported.md\n\t', childRaw: '\r\n imported guidance \t\n', origin: 'named' },
  { name: 'import-crlf-comment-body-uses-marked-sanitization', raw: ' \nparent guidance\n@imported.md\n\t', childRaw: '\r\n<!-- hidden -->\r\n imported guidance \t\n', origin: 'named' },
  { name: 'path-gated-body-keeps-trailing-newline', raw: '---\npaths: src/**\n---\nnew src body\n', origin: 'named' },
  { name: 'virtual-managed-whitespace-is-truthy', virtual: { path: '<policy>', type: 'Managed', content: ' \t\n', globs: [], contentDiffersFromDisk: true, rawContent: ' \t\n' } },
];
const cases = [];
for (const input of tests) {
  const filename = input.origin === 'rules' ? rules + '/scoped.md' : named;
  const disk = new Map([[filename, input.raw], [child, input.childRaw]]);
  const processed = new Set, reads = [], reports = [];
  const io = { readdir: async () => [{ name: 'scoped.md', isDirectory: () => false, isFile: () => true }] };
  const native = vm.runInNewContext(`(()=>{${source};return {g7,$Ne,uat,xJ,CPe,iAo,L0n};})()`, {
    Bun, Buffer, Map, Set, Error, Rq, Hs, dht,
    ae: () => io, bo: (_fs, p) => ({ resolvedPath: p, isSymlink: false, isCanonical: true }),
    rO: async (_fs, p) => { reads.push(p); return disk.get(p); },
    kJ: 102400, l0n: path.extname, kw: path.dirname, _p: path.join,
    Xe: (p, base) => path.resolve(base, p), ap: p => p, T0n: 5,
    PR: () => false, MF: () => false, zI: () => true, yat: () => false,
    ep: () => false, AIe: () => '/home', bat: () => true, rat: async () => false,
    C0n: () => false, Xn: () => false, k: () => false,
    E: error => error.code, wEe: () => ({}), we: () => '/home',
    p: (...args) => reports.push(args), i: (...args) => reports.push(args), t: () => {},
  });
  const parsed = input.virtual ? null : native.uat(input.raw, filename, 'Project', filename);
  const acquired = input.virtual ? [input.virtual] : input.origin === 'rules'
    ? await native.$Ne({ rulesDir: rules, type: 'Project', processedPaths: processed,
      includeExternal: false, conditionalRule: false })
    : await native.g7(filename, 'Project', processed, false);
  const renderedFiles = native.xJ(acquired);
  cases.push({ name: input.name, input, expected: normalize({
    parsed, acquired, descriptors: native.iAo(renderedFiles),
    rendered: native.CPe(renderedFiles), reads, reports,
    contentTruthy: acquired.map(file => native.L0n(file)), processed: [...processed],
  }) });
}
assert.deepEqual(cases.map(test => test.expected.acquired.length), [0, 0, 0, 1, 1, 2, 1, 1, 2, 2, 1, 1]);
// Execute the complete eager producer, including its real InstructionsLoaded
// loop. The native project spelling is translated only by the Rust consumer's
// explicitly permitted branding boundary.
const hookCases = [];
for (const input of tests.filter(test => !test.virtual)) {
  const nativeNamed = '/repo/CLAUDE.md', nativeRules = '/repo/.claude/rules';
  const filename = input.origin === 'rules' ? nativeRules + '/scoped.md' : nativeNamed;
  const disk = new Map([[filename, input.raw], [child, input.childRaw]]);
  const hooks = [], reports = [], snapshots = [];
  const io = { readdir: async p => p === nativeRules && input.origin === 'rules'
    ? [{ name: 'scoped.md', isDirectory: () => false, isFile: () => true }] : [] };
  const state = { walkEpoch: 0, hasLoggedInitialLoad: true, shouldFireHook: true,
    nextEagerLoadReason: 'session_start' };
  const native = vm.runInNewContext(`(()=>{${source};return {M0n};})()`, {
    Bun, Buffer, Map, Set, Error, Rq, Hs, dht,
    ae: () => io, bo: (_fs, p) => ({ resolvedPath: p, isSymlink: false, isCanonical: true }),
    rO: async (_fs, p) => {
      const raw = disk.get(p);
      if (raw !== undefined) return raw;
      const error = new Error('controlled missing file'); error.code = 'ENOENT'; throw error;
    },
    kJ: 102400, l0n: path.extname, kw: path.dirname, _p: path.join,
    Xe: (p, base) => path.resolve(base, p), ap: p => p, T0n: 5,
    PR: () => false, MF: () => false, zI: () => true, yat: () => false,
    ep: () => false, AIe: () => '/home/.claude/rules', bat: () => true, rat: async () => false,
    C0n: () => false, Xn: () => false, k: () => false,
    E: error => error.code, wEe: () => ({}), we: () => '/home',
    p: (...args) => reports.push(args), i: (...args) => reports.push(args), t: () => {},
    Ft: () => false, aat: (_cache, files) => snapshots.push(files), ne: () => {},
    ci: () => ({}), hat: () => () => false, gT: () => '/managed',
    pZ: type => type === 'Managed' ? '/managed/CLAUDE.md' : '/home/CLAUDE.md',
    j_t: () => '/managed/.claude/rules', tr: () => false,
    OT: type => type === 'projectSettings', Ee: () => '/repo', Elt: () => null,
    d0n: path.parse, vlt: () => false, hEe: () => false, A0n: () => new Set,
    lCn: () => undefined, ge: () => undefined, TJ: { peek: () => undefined },
    kat: () => [], qd: () => false, yGn: () => true, IKe: () => false,
    Wan: (_root, filePath, memoryType, loadReason, options) =>
      hooks.push({ file_path: filePath, memory_type: memoryType, load_reason: loadReason,
        globs: options.globs, parent_file_path: options.parentFilePath }),
  });
  const acquired = await native.M0n({ id: 'controlled-root' }, state, {}, false);
  assert.equal(snapshots.length, 1);
  assert.equal(snapshots[0], acquired);
  hookCases.push({ name: input.name, input, expected: normalize({ acquired, hooks, state, reports }) });
}
assert.deepEqual(hookCases.map(test => test.expected.hooks.length), [0, 0, 0, 1, 1, 2, 1, 1, 2, 2, 1]);
const output = {
  version: '2.1.287', native_sha256: nativeSha256, helper_runtime: `Bun ${Bun.version}`,
  sourceByteRanges,
  scope: 'Actual native Hs/DH/Bun.YAML, marked lexer, y0n/uat/g7/$Ne, xJ/L0n/CPe/iAo and complete M0n/TEe/I0n eager producer bodies unchanged. File reads/Dirents, canonical/external policy, settings, cache snapshots, feature state, hook dispatcher and telemetry are controlled. Local Bun helper execution, not the native CLI; quoteLossyValues, AutoMem, virtual-policy hook exclusion and host error/health acceptance are not covered.',
  cases, hookCases,
};
const file = fileURLToPath(new URL('../../crates/orchestrator/tests/fixtures/instruction_eager_2_1_287.json', import.meta.url));
const json = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(file, 'utf8'), json);
  process.stdout.write(`checked native 2.1.287 eager oracle: ${cases.length} acquisition/render and ${hookCases.length} eager-hook cases\n`);
} else {
  writeFileSync(file, json);
  process.stdout.write(`wrote native 2.1.287 eager oracle: ${cases.length} acquisition/render and ${hookCases.length} eager-hook cases\n`);
}
