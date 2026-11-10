#!/usr/bin/env node
// Evaluate the unmodified dream renderer from the hash-pinned native release.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: node dream_319_oracle.mjs <official 2.1.319 binary> [--check]');
const binary = readFileSync(process.argv[2]);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const nativeSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(sha256(binary), nativeSha256, 'The upstream executable changed');
const ranges = [];
function take(start, endExclusive, name) {
  const bytes = binary.subarray(start, endExclusive);
  ranges.push({ name, start, endExclusive, sha256: sha256(bytes) });
  return bytes.toString('utf8');
}
function between(first, last, from, name) {
  const start = binary.indexOf(first, from);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= from && end > start, name);
  return take(start, end, name);
}
let settings = {};
let flag = null;
let host = { agent: false, simple: false, autoMemory: true, excluded: false };
const context = vm.createContext({
  k: () => flag,
  yt: () => settings,
  rr: () => host.agent ? 'child-agent' : null,
  h4: () => host.simple,
  fh: () => host.autoMemory,
  xue: () => host.excluded,
  qGr: 'rm',
});
const sources = [
  between('var lm="MEMORY.md"', 'function Kze(', 263563200, 'memory constants'),
  between('var SZe=', 'function T2(', 263563500, 'memory directory instructions'),
  between('var Rk="memory_list"', '\nexport{', 265393000, 'memory tool names'),
  between('var o_n=50,', 'function LEr(', 271708000, 'forked agent default turn cap'),
  between('function K(){return k("tengu_onyx_plover"', 'class Z{', 278850000, 'availability and setting gates'),
  between('var le="## Team memory', 'var ue=30;', 278850000, 'dream literals and Q renderer'),
  between('var w=null,ce=600000', 'function IXs(', 278856000, 'thresholds and host gates'),
];
vm.runInContext(sources.join('\n'), context, { timeout: 2000 });
const run = expression => vm.runInContext(expression, context, { timeout: 2000 });
// Only the owning product's instruction filename and session brand differ.
const brand = text => text.replaceAll('CLAUDE.md', 'LINGXI.md').replaceAll('Claude sessions', 'LingXi sessions');
const render = input => {
  context.input = input;
  return brand(run('Q(input.memory_directory,input.transcript_directory,input.additional_context,input.team_memory,input.mcp_memory,input.unused_flag)'));
};
const base = { memory_directory: '{memory_directory}', transcript_directory: '{transcript_directory}', additional_context: '', team_memory: false, mcp_memory: false, unused_flag: false };
const plain = render(base);
// Recover the exact inserted bytes rather than retyping optional instructions.
function inserted(before, after) {
  let offset = 0;
  while (before[offset] === after[offset] && offset < before.length) offset++;
  const count = after.length - before.length;
  assert(count > 0);
  assert.equal(before.slice(offset), after.slice(offset + count));
  return { offset, text: after.slice(offset, offset + count) };
}
const team = inserted(plain, render({ ...base, team_memory: true }));
const mcp = inserted(plain, render({ ...base, mcp_memory: true }));
let template = plain;
for (const [name, insertion] of [['team_memory', team], ['mcp_memory', mcp]].sort((a, b) => b[1].offset - a[1].offset)) {
  template = template.slice(0, insertion.offset) + `{${name}}` + template.slice(insertion.offset);
}
template += '{additional_context}';
const cases = [];
for (const team_memory of [false, true]) for (const mcp_memory of [false, true]) for (const unused_flag of [false, true]) {
  for (const additional_context of ['', 'Prioritize build feedback.\nPreserve the useful explanation.']) {
    const input = { memory_directory: '/home/u/.lingxi/projects/-repo/memdir', transcript_directory: '/home/u/.lingxi/projects/-repo', additional_context, team_memory, mcp_memory, unused_flag };
    cases.push({ input, expected: render(input) });
  }
}
for (const input of [
  { ...base, memory_directory: '/记忆/🧠/{transcript_directory}', transcript_directory: '/sessions/{team_memory}/项目', additional_context: 'Line one\r\nLine two\u2028{memory_directory}', team_memory: true, mcp_memory: true },
  { ...base, memory_directory: '/a path/with `backticks` and $dollars', transcript_directory: '/a path/with "quotes"', additional_context: ' ' },
]) cases.push({ input, expected: render(input) });
const gateCases = [];
for (const currentFlag of [null, {}, { available: true }, { enabled: true }, { available: false, enabled: false }]) {
  for (const setting of [null, false, true]) for (const autoMemory of [false, true]) {
    flag = currentFlag;
    settings = setting === null ? {} : { autoDreamEnabled: setting };
    host = { agent: false, simple: false, autoMemory, excluded: false };
    gateCases.push({ flag, setting, host: { ...host }, available: run('sgn()'), enabled: run('Tvt()'), can_run: run('he()') });
  }
}
for (const blocked of ['agent', 'simple', 'excluded']) {
  flag = { enabled: true };
  settings = {};
  host = { agent: false, simple: false, autoMemory: true, excluded: false, [blocked]: true };
  gateCases.push({ flag, setting: null, host: { ...host }, available: run('sgn()'), enabled: run('Tvt()'), can_run: run('he()') });
}
const thresholds = [];
for (const currentFlag of [null, {}, { minHours: 0, minSessions: -1 }, { minHours: 1.5, minSessions: 2.5 }, { minHours: '3', minSessions: '4' }, { minHours: 48, minSessions: 10 }]) {
  flag = currentFlag;
  thresholds.push({ flag, expected: JSON.parse(JSON.stringify(run('fe()'))) });
}
const fixture = {
  version: '2.1.319', native_sha256: nativeSha256, source_byte_ranges: ranges,
  permitted_substitutions: ['CLAUDE.md -> LINGXI.md', 'Claude sessions -> LingXi sessions'],
  cases, scheduler_gates: gateCases, scheduler_thresholds: thresholds,
  scan_throttle_ms: run('ce'),
  default_max_turns: run('o_n'),
};
const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const outputs = [
  ['scripts/tests/fixtures/dream_319.json', JSON.stringify(fixture, null, 2) + '\n'],
  ['crates/tasks/src/handlers/templates/dream-2.1.319.md', template],
  ['crates/tasks/src/handlers/templates/dream-team-2.1.319.md', team.text],
  ['crates/tasks/src/handlers/templates/dream-mcp-2.1.319.md', mcp.text],
];
for (const [relative, expected] of outputs) {
  const path = resolve(root, relative);
  if (process.argv.includes('--check')) assert.equal(readFileSync(path, 'utf8'), expected, relative);
  else { mkdirSync(dirname(path), { recursive: true }); writeFileSync(path, expected); }
}
console.log(`Claude Code 2.1.319: ${cases.length} dream renders, ${gateCases.length} scheduler gates, ${thresholds.length} thresholds and production templates ${process.argv.includes('--check') ? 'match' : 'written'}`);
