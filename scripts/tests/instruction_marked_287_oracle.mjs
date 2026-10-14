#!/usr/bin/env bun
// Actual native instruction parser + marked tokens, without filesystem calls.
// Usage: bun scripts/tests/instruction_marked_287_oracle.mjs /path/to/2.1.287 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

assert(globalThis.Bun?.YAML, 'Run with Bun for the actual YAML intrinsic');
const binary = readFileSync(process.argv[2]);
const sha256 = createHash('sha256').update(binary).digest('hex');
assert.equal(sha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
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
].join('\n');
// The bundled lexer and frontmatter parser have colliding minified bindings;
// preserve their actual separate module scopes.
const { Rq } = vm.runInNewContext(`(()=>{${lexerSource};return {Rq};})()`);
const { Hs, dht } = vm.runInNewContext(`(()=>{${frontmatterSource};return {Hs,dht};})()`,
  { Bun, t: () => {} });
const native = vm.runInNewContext(`(()=>{${source};return {uat,y0n,cEe,v0n};})()`, {
  Bun, Buffer, Map, Set, Error, Rq, Hs, dht,
  l0n: path.extname, kw: path.dirname, Xe: (p, base) => path.resolve(base, p),
  PR: () => false, t: () => {},
});
const filename = '/repo/LINGXI.md';
const tests = [
  ['comment-token-consumes-following-blank-lines', 'before\r\n<!-- hidden -->\r\n\r\n\r\nafter\r\n'],
  ['html-container-keeps-interior-comment', '<div>\r\n<!-- inside -->\r\n</div>\r\n'],
  ['interrupted-multiline-code-span', '`open\r\n<!-- inside -->\r\nclose`\r\n'],
  ['standalone-indented-code-keeps-comment', '    <!-- code -->\r\nnext\r\n'],
  ['inline-comment-keeps-comment', 'before <!-- inline -->\r\nnext\r\n'],
  ['fenced-code-keeps-comment', '```\r\n<!-- inside -->\r\n```\r\n'],
  ['unclosed-comment-is-preserved', '<!-- unclosed\r\nrest\r\n'],
  ['comment-residue-keeps-following-blank-line', '<!-- hidden --> text\r\n\r\nnext\r\n'],
  ['import-without-comment-keeps-raw-crlf', 'before\r\n@child.md\r\nafter\r\n'],
  ['html-container-does-not-load-import', '<div>\n@child.md\n</div>\n'],
  ['paragraph-indentation-keeps-comment', 'before\n    <!-- code -->\nafter\n'],
  ['paragraph-indentation-still-loads-import', 'before\n    @child.md\nafter\n'],
  ['multiline-code-span-does-not-load-import', '`before\n@child.md\nafter`\n'],
  ['frontmatter-closing-bom-is-js-whitespace', '---\npaths: src/**\n---\uFEFF\nbody\n'],
  ['frontmatter-closing-nel-is-not-js-whitespace', '---\npaths: src/**\n---\u0085\nbody\n'],
  ['frontmatter-opening-bom-is-js-whitespace', '---\uFEFF\npaths: src/**\n---\nbody\n'],
  ['multiple-comment-spans-in-one-token', '<!-- a --><!-- b -->\n\nrest\n'],
  ['blockquote-comment-keeps-container-raw', '> <!-- inside -->\n> quote\n'],
  ['list-comment-keeps-container-raw', '- <!-- inside -->\n  list\n'],
  ['bare-cr-with-inline-comment-is-normalized', 'before <!-- inline -->\rafter\r'],
];
const normalize = value => JSON.parse(JSON.stringify(value));
const cases = tests.map(([name, raw]) => {
  const frontmatter = native.y0n(raw);
  const tokens = new Rq({ gfm: false }).lex(frontmatter.content);
  return { name, input: { path: filename, type: 'Project', raw }, expected: normalize({
    parsed: native.uat(raw, filename, 'Project', filename),
    // Keep only top-level raw views here; actual uat's v0n still traverses the
    // complete native token trees, including items and codespans.
    tokens: tokens.map(token => ({ type: token.type, raw: token.raw })),
  }) };
});
assert.equal(cases.length, 20);
const output = {
  version: '2.1.287', native_sha256: sha256, helper_runtime: `Bun ${Bun.version}`,
  sourceByteRanges,
  scope: 'Unchanged actual native Rq marked lexer, Hs/DH/Bun.YAML/dht/y0n/uat/cEe/v0n. Path resolver and remote-path guard are controlled; no filesystem acquisition or quoteLossyValues. The native parser decides conditional lexer invocation and rawContent/contentDiffersFromDisk. Tokens are diagnostic native views. Local Bun helper execution, not the native CLI or its embedded runtime. Filenames use the permitted LingXi branding difference.',
  cases,
};
const target = fileURLToPath(new URL('../../crates/memory/tests/fixtures/instruction_marked_2_1_287.json', import.meta.url));
const json = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(target, 'utf8'), json);
  process.stdout.write(`checked native 2.1.287 marked oracle: ${cases.length} cases\n`);
} else {
  mkdirSync(path.dirname(target), { recursive: true });
  writeFileSync(target, json);
  process.stdout.write(`wrote native 2.1.287 marked oracle: ${cases.length} cases\n`);
}
