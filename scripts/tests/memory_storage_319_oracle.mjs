#!/usr/bin/env node
// Evaluate the actual, hash-pinned 2.1.319 memory storage helpers, never a
// rewritten reference algorithm. Host filesystem facts are controlled seams.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import * as path from 'node:path';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const hash = value => createHash('sha256').update(value).digest('hex');
const native_sha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(hash(binary), native_sha256, 'Native memory artifact changed');
const ranges = [];
function take(start, end, signature) {
  const bytes = binary.subarray(start, end);
  assert(bytes.toString().startsWith(signature), `Moved native source: ${signature}`);
  ranges.push({ signature, start, end_exclusive: end, sha256: hash(bytes) });
  return bytes.toString('utf8');
}
function between(startMarker, endMarker, from) {
  const start = binary.indexOf(startMarker, from);
  const end = binary.indexOf(endMarker, start + startMarker.length);
  assert(start >= from && end > start, startMarker);
  return take(start, end, startMarker);
}
function pureModule(start, imports = {}, extraExports = []) {
  const end = binary.indexOf(0, start);
  const source = take(start, end, '// @bun @bytecode')
    .replace(/import\{[^}]+\}from"[^"]+";/g, '')
    .replace(/export\{([^}]+)\};/, `return {$1,${extraExports.join(',')}};`);
  return vm.runInNewContext(`(()=>{${source}})()`, imports, { timeout: 5000 });
}
const util = vm.runInNewContext(`
${between('function Q(t,n)', 'function Pa(', 259201000)}
${between('function Ocn(t)', 'function m(t)', 259201400)}
${between('function LH(t,n)', 'var W9=', 259205000)}
${between('function Dt(t,n', 'function Gae(', 259203600)}
({Q,LH,Dt})`, { Buffer });
const unicode = pureModule(259208112, util);
const scrub = pureModule(261451797, { ...unicode, ...util }, ['Su', 'Eu']);
const tags = vm.runInNewContext(`(()=>{
${between('var tqe=', 'function Sme(', 259537685)}
return {tags:Hae, channel:SG};})()`);
const neutralize = scrub.qLs([
  { tags: tags.tags.filter(tag => tag !== tags.channel) },
  { tags: [tags.channel], tail: scrub.VLs },
  { tags: ['antml'], tail: ({ colon }) => `[${colon}]` },
]);
const memory = vm.runInNewContext(`
${between('var lm="MEMORY.md"', 'function D0(', 263563200)}
${between('function Rfe(t)', 'export{', 263563900)}
${between('function en(r)', 'function fcn(', 260585800)}
${between('function xvr(e)', 'function nEs(', 265538700)}
${between('var EXo=', 'import{lstat', 270721800)}
({Kze,xvr,ZTe,UFt,Rfe,r1n,en})`, { ...util, Jx: text => neutralize.neutralize(text) });
let gateInput;
// Use one linked context for the gate inputs so the native gate body remains
// unchanged while the flag/settings facts vary.
const gateEnv = { env: {} }, gateState = {};
const gateContext = vm.createContext({
  process: gateEnv, i: gateState, CD: () => false, Gxe: () => false,
  to: () => false, OC: () => false, q: () => false, lin: () => false,
  yt: () => gateInput.setting === null ? {} : { autoMemoryEnabled: gateInput.setting },
});
vm.runInContext(
  between('function Pe(e){if(!e)', 'function ev(', 259274600) +
  between('function fh()', 'function ube(', 262671900), gateContext);
const gateCases = [];
for (const disable of [null, '', '1', 'true', 'YES', ' on ', '0', 'false', 'no', 'OFF', '\ufeffFalse\ufeff', '\u0085false\u0085', 'maybe']) {
  for (const simple of [false, true]) for (const setting of [null, false, true]) {
    const input = { disable, simple, setting };
    gateInput = input;
    gateEnv.env = disable === null ? {} : { CLAUDE_CODE_DISABLE_AUTO_MEMORY: disable };
    gateState.CLAUDE_CODE_SIMPLE = simple;
    gateCases.push({ ...input, expected: vm.runInContext('fh()', gateContext) });
  }
}
const cases = [
  ['empty', ''], ['trim-bom-preserve-nel', '\ufeff\u0085 rules \u0085\ufeff'],
  ['trim-all-js-space', '\t\u00a0\u200a\ufeff hello \u3000\n'],
  ['exact-units', 'x'.repeat(25000)], ['over-units', 'x'.repeat(25001)],
  ['exact-lines', Array.from({ length: 200 }, (_, i) => `L${i + 1}`).join('\n')],
  ['over-lines', Array.from({ length: 201 }, (_, i) => `L${i + 1}`).join('\n')],
  ['trim-final-line', ('x\n').repeat(200)],
  ['both-caps', Array.from({ length: 220 }, (_, i) => `L${i + 1} ${'x'.repeat(128)}`).join('\n')],
  ['byte-cap-keeps-whole-lines', 'x'.repeat(15000) + '\n' + 'y'.repeat(15000) + '\nnext'],
  ['newline-at-cap', 'x'.repeat(25000) + '\nnext'],
  ['surrogate-split-at-cap', 'x'.repeat(24999) + '😀tail'],
  ['surrogate-pair-at-cap', 'x'.repeat(24998) + '😀tail'],
  ['astral-unicode-is-two-units', '😀'.repeat(12501)],
  ['unicode-newlines-and-preview', ('条目\n').repeat(200) + '  长期规则 😀  '],
  ['long-word-preview', ('x\n').repeat(200) + 'a'.repeat(100)],
  ['whole-word-preview', ('x\n').repeat(200) + ('long word '.repeat(12))],
  ['astral-preview-boundary', ('x\n').repeat(200) + 'x'.repeat(78) + '😀tail'],
];
const index_cases = cases.map(([name, input]) => ({ name, input, expected: memory.xvr(input) }));
const advisory_cases = [];
const byte_format_cases = [0, 1, 1023, 1024, 1279, 1280, 1281, 1536, 1792, 1048576, 1310720, 1073741824, 1342177280]
  .map(bytes => ({ bytes, expected: memory.en(bytes) }));
for (const bytes of [0, 1280, 19999, 20000, 24999, 25000, 25001, 30000]) {
  for (const lines of [1, 159, 160, 200, 201, 240]) {
    const expected = memory.ZTe({ label: 'memory index', displayPath: 'MEMORY.md', sizeBytes: bytes, byteCap: 25000, lineCount: lines, lineCap: 200 });
    advisory_cases.push({ bytes, lines, expected });
  }
}
const sanitize_inputs = [
  '', 'ordinary 😀 memory\n', '\ufeffa\u0085b\r\nc\rd\u2028e\u2029f\u200bg',
  'preserve \u200c\u200d\ufe0e\ufe0f remove \ue000\u0378',
  ...tags.tags.map(tag => `<${tag}>data</${tag}>`),
  '<untrusted-widget-state>data</untrusted-widget-state>',
  '<channel source="fixture">data</channel>', '<channel>plain</channel>', '<antml:thing>data</antml:thing>',
  '<SyStEm-ReMiNdEr>caps</SYSTEM-REMINDER>', '<\\system-reminder>already neutralized',
  '＜ｓｙｓｔｅｍ－ｒｅｍｉｎｄｅｒ＞confusable', '<s\u0301ystem-reminder>combining',
  '<s\u200bystem-reminder>invisible', '<ſyſtem-reminder>long-s', '<system&#45;reminder>entity',
  '<channel source = "fixture">space</channel>', '<antml ordinary>not a tag',
];
const sanitize_cases = sanitize_inputs.map(input => ({ input, expected: memory.Rfe(input) }));
const loaded_index_cases = [
  ['sanitize-after-splice', 'x'.repeat(24999) + '\u200b😀tail'],
  ['remove-truncated-surrogate', 'x'.repeat(24999) + '😀tail'],
  ['normalize-after-line-splice', ('x\n').repeat(199) + 'y\rhidden\u2028tail\nnext'],
  ['neutralize-new-tag', '- <untrusted-widget-state>reference</untrusted-widget-state>'],
].map(([name, input]) => ({ name, input, expected: memory.Rfe(memory.xvr(input).content) }));
// V9o consumes td's content independently of whether YAML metadata decodes.
// Only the external YAML decoder is stubbed; the native BOM, delimiter and
// content-range extraction execute unchanged, before the native xvr/Rfe pair.
const indexFrontmatter = vm.runInNewContext(`
${between('function aE(e){return e.charCodeAt(0)', 'function _w(e)', 265395400)}
var ${between('mv=/^---', ',Gz=', 265481200)};
${between('function td(e,n,r)', 'function vHn(e)', 265481300)}
${between('function V9o(e)', 'function pXe(', 270748000)}
({V9o})`, { vHn: () => ({ ok: true, value: {} }), il: value => value, t: () => {} });
const index_frontmatter_cases = [
  ['ordinary-frontmatter', '---\nname: index\n---\n\n- [Guidance](guidance.md)'],
  ['bom-crlf-frontmatter', '\ufeff---\r\nname: index\r\n---\r\n\r\n- [Guidance](guidance.md)\r\n'],
  ['delimiter-at-eof', '---\nname: index\n---'],
  ['delimiter-trailing-space-eof', '---\nname: index\n--- \t'],
  ['loose-inline-closing-delimiter', '---\ndescription: before---after\n---\nbody'],
  ['empty-frontmatter', '---\n---\nbody'],
  ['yaml-error-does-not-restore-frontmatter', '---\nname: [invalid\n---\nbody'],
  ['js-space-before-opening-newline', '---\ufeff\u200a\nname: index\n---\nbody'],
  ['nel-is-not-delimiter-whitespace', '---\u0085\nname: index\n---\nbody'],
  ['bom-without-frontmatter', '\ufeffbody'],
  ['double-bom-is-not-frontmatter', '\ufeff\ufeff---\nname: index\n---\nbody'],
  ['unterminated-frontmatter', '---\nname: index\nbody'],
].map(([name, input]) => {
  const expected_body = indexFrontmatter.V9o(input).content;
  return { name, input, expected_body,
    expected: memory.Rfe(memory.xvr(expected_body).content) };
});
let recallInput;
const recallContext = vm.createContext({
  Buffer, ...memory, Vze: 200, DSe: 4096, Ge: 'Read',
  JGr: async () => ({ isFile: () => true }), xji: async () => true, jme: () => '',
});
vm.runInContext(between('function Uys(', 'function v(n)', 268946000), recallContext);
recallContext.tLe = async (...args) => vm.runInContext('Uys(recallInput, Buffer.byteLength(recallInput), 0, 0, 200, 4096)', recallContext);
vm.runInContext(between('async function UEe(', 'var j7o=', 270849700) +
  between('async function Bzr(', 'var Uzr=', 272911600), recallContext);
const recall_cases = [];
for (const [name, input, file = '/memory/notes.md'] of [
  ['empty', ''], ['ordinary', 'one\ntwo\n'], ['bom-crlf', '\ufeffone\r\ntwo\r\n'],
  ['frontmatter-retained', '---\nname: guidance\nmetadata:\n  type: feedback\n---\n\nUse narrow checks.\n'],
  ['byte-limit', 'x'.repeat(4097)], ['exact-byte-limit', 'x'.repeat(4096)],
  ['unicode-byte-limit', '😀'.repeat(1024) + '\nlast'],
  ['line-limit', ('x\n').repeat(200) + 'tail'],
  ['sanitize-after-byte-splice', 'x'.repeat(4095) + '\u200b' + '\nnext'],
  ['sanitize-control-and-tag', 'a\rhidden\u2028<untrusted-widget-state>reference'],
  ['path-single-line-sanitized', 'x'.repeat(4097), '/memory/\u200bno\n\t<system-reminder>.md'],
]) {
  recallInput = input;
  recallContext.recallInput = input;
  const rows = await vm.runInContext(`Bzr([{path:${JSON.stringify(file)},mtimeMs:0}],undefined)`, recallContext);
  recall_cases.push({ name, input, path: file, expected: rows[0].content });
}
const nativePath = vm.runInNewContext(`
${between('function sr(t)', 'function th(', 259309000)}
${between('function OD(t)', 'export{', 259326400)}
${between('function jS(t)', 'function JIo(', 259310000)}
${between('function A(t)', 'function p6(', 259310000)}
${between('function x(t)', 'function kKn(', 259310000)}
${between('function Ba(t)', 'function usa(', 259312000)}
${between('function Sl(t)', 'var Lt=', 259315000)}
${between('function WF(t)', 'function ay(', 259315800)}
${between('function fc(e,n)', 'function va(', 262673000)}
({fc})`, { K: path, Ea: path.posix.normalize, hi: path.posix.join, LI: path.posix.isAbsolute, NI: () => '/home/user', go: '/' });
const path_cases = [null, '', '/', '/a', '/ab', 'relative', 'relative/notes', '~', '~/', '~\\',
  '~/notes', '~\\notes', '~//notes', '~//', '~/../outside', '~/a/../../outside', '~/a/../notes',
  '/tmp/notes/../memory/', '/tmp/re\u0301sume\u0301', '/tmp/notes\\', '/net/host/memory',
  '/NET/host/memory', '/Network/Servers/host/memory', '/network/local', '/networK/local',
  '/.vol/123/memory', '/.file/123/memory', '/.nofollow/path', '/.resolve/path',
  '/??/Device/path', '/tmp/a\0b', '/tmp/file', '/tmp/a\\b', '//server/share',
].map(input => ({ input, expected: nativePath.fc(input === null ? undefined : input, true)?.replace(/\/$/, '') ?? null }));
const exclusions = [], reading = [];
let start;
for (let codepoint = 0; codepoint <= 0x10ffff; codepoint++) {
  const character = String.fromCodePoint(codepoint), excluded = memory.UFt(character);
  if (excluded && start === undefined) start = codepoint;
  if (!excluded && start !== undefined) { exclusions.push([start, codepoint - 1]); start = undefined; }
  if (!excluded && codepoint >= 128) {
    const normalized = scrub.Eu(character);
    if (normalized !== character) reading.push([codepoint, normalized]);
  }
}
if (start !== undefined) exclusions.push([start, 0x10ffff]);
const patterns = [
  scrub.Su({ tags: tags.tags.filter(tag => tag !== tags.channel) }).source,
  scrub.Su({ tags: [tags.channel], tail: scrub.VLs }).source,
  scrub.Su({ tags: ['antml'], tail: ({ colon }) => `[${colon}]` }).source,
];
const table = { version: '2.1.319', native_sha256, tags: tags.tags, exclusions, reading, patterns };
const output = { version: '2.1.319', native_sha256, source_byte_ranges: ranges,
  seams: ['Host unsupported session/policy/backend gates disabled; simple flag and autoMemoryEnabled are explicit inputs',
    'Native pure helper imports linked to extracted functions; filesystem I/O is not executed',
    'Index frontmatter vectors stub only YAML metadata decoding; native V9o/td BOM, delimiters and body extraction execute unchanged',
    'String inputs are valid Unicode; index output retains native lone surrogate code units'],
  index_cases, advisory_cases, byte_format_cases, gate_cases: gateCases, sanitize_cases,
  loaded_index_cases, index_frontmatter_cases, recall_cases, path_cases,
};
for (const [url, value] of [
  [new URL('../../crates/memory/tests/fixtures/memory_storage_2_1_319.json', import.meta.url), output],
  [new URL('../../crates/core/src/host/instruction_memory_sanitize_319.json', import.meta.url), table],
]) {
  const text = JSON.stringify(value, null, 2) + '\n';
  if (process.argv.includes('--check')) assert.equal(readFileSync(url, 'utf8'), text, `${url.pathname} differs`);
  else writeFileSync(url, text);
}
const caseCount = index_cases.length + advisory_cases.length + byte_format_cases.length + gateCases.length + sanitize_cases.length + loaded_index_cases.length + index_frontmatter_cases.length + recall_cases.length + path_cases.length;
console.log(`Memory 2.1.319: ${caseCount} cases (${index_cases.length} index, ${advisory_cases.length} advisory, ${byte_format_cases.length} size format, ${gateCases.length} gate, ${sanitize_cases.length} sanitizer, ${loaded_index_cases.length} loaded-index, ${index_frontmatter_cases.length} frontmatter, ${recall_cases.length} recall, ${path_cases.length} path); ${reading.length} Unicode readings`);
