#!/usr/bin/env node
// Evaluate the shipped renderers, preserving every byte except the named brand.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync, mkdirSync} from 'node:fs';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: memory_prompts_319_oracle.mjs <2.1.319 executable> [--check]');
const binary = readFileSync(process.argv[2]);
const digest = data => createHash('sha256').update(data).digest('hex');
const nativeSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(digest(binary), nativeSha256, 'Unexpected Claude Code artifact');
const ranges = [];
function take(start, end, label) {
  const bytes = binary.subarray(start, end);
  ranges.push({label, start, endExclusive: end, sha256: digest(bytes)});
  return bytes.toString('utf8');
}
function between(first, last, offset, label) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start, label);
  return take(start, end, label);
}
const sources = [
  'var ' + between('qho=', ';function a2i(', 264486500, 'rewritten memory hygiene') + ';',
  between('function en(r)', 'function fcn(', 260585800, 'size formatter'),
  between('var lm="MEMORY.md"', 'function Kze(', 263563200, 'memory constants'),
  between('var SZe=', 'function T2(', 263563500, 'directory instruction'),
  take(265488215, 265511666, 'memory protocol literals'),
  between('function Al(', 'import{lstat as Af', 265523500, 'lean renderer'),
  take(265541137, 265543269, 'standard renderer'),
  between('var Nfo=', 'function bTt(', 274493200, 'update source labels'),
  between('var GL=', 'function cmr(', 274678500, 'ambient trailer'),
];
const context = vm.createContext({
  k: (_key, fallback) => fallback, rn: (_key, fallback) => fallback,
  yq: () => false, JTo: () => null, TR: () => undefined,
  vvr: 'tengu_memory_citation', Ml: (...parts) => parts.join('/'),
  mu: values => values, Ie: value => value,
  js: values => values.filter(value => typeof value === 'string'),
  nf: value => value, le: () => false, $i: () => false,
});
vm.runInContext(sources.join('\n'), context, {timeout: 2000});
const run = expression => vm.runInContext(expression, context, {timeout: 2000});
const rebrand = text => text.replaceAll('CLAUDE.md', 'LINGXI.md').replaceAll('Claude Code', 'LingXi');
const templates = {
  lean: rebrand(run('Al({autoDir:"{memory_path}",teamDir:null,skipIndex:false,citeMemories:false})')),
  lean_rewrite: rebrand(run('Al({autoDir:"{memory_path}",teamDir:null,skipIndex:false,citeMemories:false,rewriteMemoryHygiene:true})')),
  auto: rebrand(run('ci("auto memory","{memory_path}").join("\\n")')),
  auto_static: rebrand(run('ci("auto memory",null).join("\\n")')),
};
const cases = [];
for (const path of ['/cfg/projects/repo/memdir', '/cfg/你好/🧠', '/cfg/{memory_path}/memdir']) {
  for (const rewrite of [false, true]) {
    context.memoryInput = {autoDir: path, teamDir: null, skipIndex: false, citeMemories: false, rewriteMemoryHygiene: rewrite};
    cases.push({kind: 'lean', path, rewrite, expected: rebrand(run('Al(memoryInput)'))});
  }
  context.memoryPath = path;
  cases.push({kind: 'auto', path, expected: rebrand(run('ci("auto memory",memoryPath).join("\\n")'))});
}
const updateSource = between('case"memory_update":', '}}if(["autocheckpointing"', 274673000, 'memory update renderer') + '}}';
run(`function memoryUpdate(e){switch(e.type){${updateSource}}`);
const updates = [];
for (const source of ['dream', 'extract', 'other', 'sync_unsaved']) {
  for (const paths of [[], ['a.md', '你好.md']]) {
    const input = {type: 'memory_update', source, summary: 'Updated 🧠\nnotes', paths, inContextPaths: paths.slice(0, 1)};
    context.updateInput = input;
    updates.push({input, expected: JSON.parse(JSON.stringify(run('memoryUpdate(updateInput)')))});
  }
}
const output = {
  version: '2.1.319', native_sha256: nativeSha256,
  permitted_substitutions: ['CLAUDE.md -> LINGXI.md', 'Claude Code -> LingXi'],
  source_byte_ranges: ranges,
  host_features: {team_memory: false, citations: false, project_skill_upkeep: false},
  templates, cases, updates,
};
const check = process.argv.includes('--check');
const fixture = new URL('./fixtures/memory_prompts_319.json', import.meta.url);
const serialized = JSON.stringify(output, null, 2) + '\n';
const templateRoot = new URL('../../crates/orchestrator/src/prompt/templates/claude-code-2.1.319/', import.meta.url);
for (const [name, body] of Object.entries(templates)) {
  const path = new URL(`memory-${name.replaceAll('_', '-')}.md`, templateRoot);
  if (check) assert.equal(readFileSync(path, 'utf8'), body, name);
  else { mkdirSync(templateRoot, {recursive: true}); writeFileSync(path, body); }
}
if (check) assert.equal(readFileSync(fixture, 'utf8'), serialized);
else { mkdirSync(new URL('./fixtures/', import.meta.url), {recursive: true}); writeFileSync(fixture, serialized); }
console.log(`Claude Code 2.1.319: four memory templates, ${cases.length} renders and ${updates.length} updates match`);
