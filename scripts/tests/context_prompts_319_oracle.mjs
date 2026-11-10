#!/usr/bin/env node
// Execute prompt renderers from the hash-pinned Claude Code 2.1.319 artifact.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

assert(process.argv[2], 'Usage: context_prompts_319_oracle.mjs <2.1.319 binary> [--check]');
const binary = readFileSync(process.argv[2]);
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const binarySha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(hash(binary), binarySha256);
const ranges = [];
function take(start, end, label) {
  const bytes = binary.subarray(start, end);
  ranges.push({ label, start, endExclusive: end, sha256: hash(bytes) });
  return bytes.toString('utf8');
}
function between(first, last, offset, label) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start, label);
  return take(start, end, label);
}
const catalogSource = between('var kAo=', ';function M()', 260740000, 'baked model catalog');
const context = vm.createContext({
  // Only external host facts are substituted; the selected native renderers
  // and their literal/array dependencies are evaluated without rewriting.
  k: (_flag, fallback) => fallback,
  rn: (_flag, fallback) => fallback,
  yq: () => false,
  JTo: () => null,
  TR: () => undefined,
  vvr: 'tengu_memory_citation',
  Ml: (...parts) => parts.join('/'),
  mu: rows => rows,
  Ie: value => value,
  le: () => false,
  $i: () => false,
  js: value => value,
  nf: value => value,
});
const source = [
  catalogSource + ';',
  'var ' + between('qho=', ';function a2i(', 264486500, 'rewrite memory hygiene instructions') + ';',
  between('function en(r)', 'function fcn(', 260585800, 'byte size formatter'),
  between('var lm="MEMORY.md"', 'function Kze(', 263563200, 'memory constants'),
  between('var SZe=', 'function T2(', 263563500, 'memory directory instructions'),
  take(265488215, 265511666, 'memory protocol literals and formatters'),
  between('function Al(', 'import{lstat as Af', 265523500, 'lean memory renderer'),
  take(265541137, 265543269, 'standard memory renderer'),
  between('function RWi(e)', 'var xWi=', 264462800, 'rewrite model catalog renderer'),
  take(271738465, 271738836, 'model catalog selection and renderer'),
  between('function jCr(e,n,r)', 'var Ywi=', 271788000, 'static environment catalog section'),
  between('var JG=new Set(', 'function csn(', 263090000, 'model display overrides and 1M suffix models'),
  between('function jm(e)', 'function COs(', 263090800, 'model marketing name renderer'),
  between('function uyt(e)', 'function ZOr(', 263095600, '1M suffix predicate'),
  between('function Mk(e)', 'var Q7r=', 269443000, 'environment bullet formatter'),
  between('var wWe=', ',bWe=p', 269443300, 'environment headers and worktree notices') + ';',
  between('var tEo=', 'function oEo(', 269447900, 'downloaded data isolation instruction'),
  between('function oEo(', 'function vWe(', 269448000, 'host environment renderer'),
  between('function a2i(e)', 'function oks(', 264487400, 'rewrite model identity renderer'),
  between('function hEo(e)', 'async function aAt(', 269454000, 'model identity renderer'),
  between('var Nfo=', 'function bTt(', 274493200, 'memory update source labels'),
  between('var GL=', 'function cmr(', 274678500, 'ambient context instruction'),
].join('\n');
// rt/sd/qq are the existing model-registry boundary, populated from the exact
// catalog above. Canonical ids only are used here; route aliases are tested by
// the runtime's own normalization tests.
vm.runInContext(source, context, { timeout: 2_000 });
vm.runInContext(`
  function qq(){ return kAo; }
  function sd(id){ return kAo.models.find(row => row.id === id); }
  function rt(id){ return id.replace(/\\[1m\\]/gi, ''); }
  function Ce(){ return 'firstParty'; }
  function Rq(id){ return id; }
`, context);
const run = expression => vm.runInContext(expression, context, { timeout: 2_000 });
const rebrand = text => text.replaceAll('CLAUDE.md', 'LINGXI.md').replaceAll('Claude Code', 'LingXi');
const memory = Object.fromEntries([
  ['lean', 'Al({autoDir:"{memory_path}",teamDir:null,skipIndex:false,citeMemories:false})'],
  ['lean_rewrite', 'Al({autoDir:"{memory_path}",teamDir:null,skipIndex:false,citeMemories:false,rewriteMemoryHygiene:true})'],
  ['auto', 'ci("auto memory","{memory_path}").join("\\n")'],
  ['auto_static', 'ci("auto memory",null).join("\\n")'],
].map(([name, expression]) => [name, rebrand(run(expression))]));
const models = JSON.parse(JSON.stringify(run('kAo.models'))).map(row => ({
  id: row.id,
  marketing_name: run(`jm(${JSON.stringify(row.id)})`) ?? null,
  knowledge_cutoff: row.knowledge_cutoff ?? null,
}));
const modelCatalog = {
  standard: run('mvr(cvr(),false)'),
  rewrite: run('mvr(cvr(),true)'),
  latest: JSON.parse(JSON.stringify(run('cvr()'))),
  body: rebrand(run('jCr(cvr(),false,false)')),
  subagent_body: rebrand(run('jCr(cvr(),true,false)')),
  rewrite_body: rebrand(run('jCr(cvr(),false,true)')),
};
const identity = [
  { modelId: 'claude-opus-5-5', marketingName: 'Opus 5.5', knowledgeCutoff: 'June 2026' },
  { modelId: 'custom-model', marketingName: null, knowledgeCutoff: null },
].map(input => {
  context.identityInput = input;
  return { input, expected: run('iAt(identityInput)'), rewrite: run('[a2i(identityInput),l2i(identityInput)].filter(value=>value!==null).join(" " )') };
});
const environment = [false, true, 'additional-dirs'].map(variant => {
  const isWorktree = variant === true;
  const input = { workingDirectory: '/repo', isWorktree, isGitRepo: isWorktree, additionalWorkingDirectories: variant === 'additional-dirs' ? ['/tools', '/workspace/second project'] : [], platform: 'darwin', shell: 'zsh', osVersion: 'Darwin 27.2.0' };
  context.environmentInput = input;
  return { input, expected: rebrand(run('QRt(environmentInput)')) };
});
const listingSource = between('skill_listing:(e)=>', ',dynamic_skill:', 274628000, 'skill listing attachment renderer');
run(`var skillListing = ({${listingSource}}).skill_listing;`);
const skillListing = [null, '', '- verify: Check changes'].map(content => {
  context.listingInput = { content };
  return { content, expected: JSON.parse(JSON.stringify(run('skillListing(listingInput)'))) };
});
const updateSource = between('case"memory_update":', '}}if(["autocheckpointing"', 274673000, 'memory update attachment renderer') + '}}';
run(`function memoryUpdate(e){switch(e.type){${updateSource}}`);
const memoryUpdates = [
  { source: 'dream', summary: 'Merged notes', paths: ['a.md', 'b.md'], inContextPaths: ['a.md'] },
  { source: 'extract', summary: 'Extracted feedback', paths: [], inContextPaths: [] },
  { source: 'other', summary: 'Updated notes', paths: [], inContextPaths: [] },
  { source: 'sync_unsaved', summary: 'Pending memory changes', paths: ['a.md'], inContextPaths: ['a.md'] },
].map(input => {
  context.updateInput = { type: 'memory_update', ...input };
  return { input, expected: JSON.parse(JSON.stringify(run('memoryUpdate(updateInput)'))) };
});
const output = JSON.stringify({
  version: '2.1.319', binary_sha256: binarySha256, source_byte_ranges: ranges,
  defaults: { citations: false, project_skill_upkeep: false, team_memory: false },
  permitted_substitutions: ['CLAUDE.md -> LINGXI.md', 'Claude Code -> LingXi'],
  memory, model_catalog: modelCatalog, models, model_identity: identity, environment,
  skill_listing: skillListing, memory_updates: memoryUpdates,
}, null, 2) + '\n';
const templatesRoot = fileURLToPath(new URL('../../crates/orchestrator/src/prompt/templates/claude-code-2.1.319/', import.meta.url));
// Explicit platform-capability exception: retain native oracle bodies above,
// but replace exactly their complete product-availability line at rendering.
const nativeAvailabilityLine = ' - LingXi is available as a CLI in the terminal, desktop app (Mac/Windows), web app (claude.ai/code), and IDE extensions (VS Code, JetBrains).';
const hostAvailability = 'LingXi runs in the current host client. Use the interfaces and capabilities explicitly exposed in this session.';
for (const name of ['body', 'subagent_body', 'rewrite_body']) {
  const lines = modelCatalog[name].split('\n');
  assert.equal(lines.filter(line => line === nativeAvailabilityLine).length, 1, `Native availability line differs: ${name}`);
}
const templates = [
  ...Object.entries(memory).map(([name, body]) => [`memory-${name.replaceAll('_', '-')}.md`, body]),
  ['model-catalog-standard.md', modelCatalog.standard],
  ['model-catalog-rewrite.md', modelCatalog.rewrite],
  ['product-availability.md', hostAvailability],
];
for (const [name, expected] of templates) {
  const target = templatesRoot + name;
  if (process.argv.includes('--check')) {
    assert.equal(readFileSync(target, 'utf8'), expected, `Production context template differs: ${name}`);
  } else {
    mkdirSync(templatesRoot, { recursive: true });
    writeFileSync(target, expected);
  }
}
const target = fileURLToPath(new URL('fixtures/context_prompts_319.json', import.meta.url));
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(target, 'utf8'), output, 'Current context oracle differs');
  console.log('Claude Code 2.1.319 context prompt oracle and seven production templates match (explicit host availability exception)');
} else {
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, output);
  console.log(`Wrote ${target}`);
}
