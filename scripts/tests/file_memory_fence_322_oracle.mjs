#!/usr/bin/env node
// Pinned helper replay only; no model, account, or live user files are read.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.322');
const binary = readFileSync(binaryPath);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = digest(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const sourceByteRanges = [
  { name: 'safe_prefix', start: 260461603, end: 260461746, sha256: 'aba65f0627f912d07129e66e5cfbf35b189cd4ce73b0edc7f490e1f6d72a5b8a' },
  { name: 'clip_marker', start: 270982032, end: 270982084, sha256: '57e772f112ad38e298a6c6925962bb582e0c20d9dbf50b1182cfae5d8fc95674' },
  { name: 'clip_start', start: 270982084, end: 270982379, sha256: '1f5eed2f915aa0a47107e56874ed18d552e345fdf79e95f4f9ade6d02d004ad9' },
  { name: 'nested_limit', start: 272301084, end: 272301199, sha256: 'e31d3e186aef090e6019955e04dfc90e2d590a05dbc2e9358fa8068201ccf55c' },
  { name: 'full_view', start: 266182509, end: 266182719, sha256: '656eddf0be279b3def2d664bc1d8a96968fddaa3a51472b83f8aa05c3b47c31e' },
  { name: 'fence_methods', start: 266183385, end: 266183555, sha256: '209234864a09d59f1097455f5329d85273d2c07d43cac9ea831e10f0883703c6' },
  { name: 'model_ratio', start: 264401421, end: 264401792, sha256: '3dd66de3c2006f1b333c44bec9df221d9f3784908e8ea6694e867500fbd67e0f' },
];
const source = Object.fromEntries(sourceByteRanges.map(range => {
  const bytes = binary.subarray(range.start, range.end);
  assert.equal(digest(bytes), range.sha256, `${range.name}: native bytes changed`);
  return [range.name, bytes.toString('utf8')];
}));
const context = vm.createContext({
  Buffer, path,
  t1e: value => Buffer.from(value, 'utf16le').toString('utf16le'),
  Ft: (value, separator) => value.split(separator).length - 1,
  Ge: 'Read', Wt: value => value, Ht: value => value.toLowerCase(), it: value => value,
  at: () => '', $p: () => [], og: () => context.contextTokens,
});
vm.runInContext(source.safe_prefix + source.clip_marker + source.clip_start
  + `var ${source.model_ratio}; var usr=.05,csr=40000,bSt=200000;`
  + source.nested_limit + source.full_view
  + `function a(value) { return path.normalize(value); }
     class NativeFence { cache=new Map; startOnly=new Set; ${source.fence_methods} }
     function probeFence(operations) {
       let state=new NativeFence; return operations.map(operation=>{
         if(operation.op==='mark')state.markStartOnly(operation.path);
         if(operation.op==='set')state.cache.set(a(operation.path), operation.entry);
         if(operation.op==='delete')state.cache.delete(a(operation.path));
         if(operation.op==='clear')state.cache.clear();
         if(operation.op==='clone') { let next=new NativeFence; for(let key of state.startOnlyPaths())next.markStartOnly(key); state=next; }
         return state.holdsOnlyStartOf(operation.path);
       });
     }`, context);
const units = value => Array.from({length: value.length}, (_, index) => value.charCodeAt(index));
const clipCases = [];
for (const content of ['', 'alpha\nbeta\ngamma', '\nalpha\nbeta', 'a😀b\nc', '\ud800x\udc00tail', '中😀文\n尾']) {
  for (const limit of [0, 1, 2, 3, 5, 10, 99]) {
    for (const filePath of [null, '/project/LINGXI.md', '/project/😀.md']) {
      clipCases.push({input: {contentUnits: units(content), path: filePath, limit},
        expectedUnits: units(context.mno(content, filePath ?? undefined, limit))});
    }
  }
}
const limitCases = [];
for (const model of ['', 'claude-sonnet-4-6', 'claude-opus-5', 'qwen-custom']) {
  for (const contextTokens of [null, 0, 1, 200000, 1000000, 2000000]) {
    context.contextTokens = contextTokens === null ? undefined : contextTokens;
    limitCases.push({input: {model, contextTokens}, expected: context.PXe(model)});
  }
}
const fenceCases = [
  [
    {op: 'mark', path: '/project/LINGXI.md'},
    {op: 'set', path: '/project/LINGXI.md', entry: {content: 'prefix', isPartialView: true}},
    {op: 'set', path: '/project/LINGXI.md', entry: {content: 'tail', offset: 2}},
    {op: 'set', path: '/project/LINGXI.md', entry: {content: 'one', limit: 1}},
    {op: 'set', path: '/project/LINGXI.md', entry: {content: 'one', limit: 2}},
    {op: 'delete', path: '/project/LINGXI.md'},
    {op: 'set', path: '/project/LINGXI.md', entry: {content: 'whole'}},
    {op: 'clear', path: '/project/LINGXI.md'},
    {op: 'clone', path: '/project/LINGXI.md'},
  ],
  [
    {op: 'mark', path: '/project/./LINGXI.md'},
    {op: 'set', path: '/project/LINGXI.md', entry: {content: 'host seed', contentNotInModelContext: true}},
    {op: 'set', path: '/project/LINGXI.md', entry: {content: '', limit: 2}},
    {op: 'set', path: '/project/LINGXI.md', entry: {content: ''}},
    {op: 'delete', path: '/project/LINGXI.md'},
  ],
].map(input => ({input, expected: Array.from(context.probeFence(input))}));
const fixture = {nativeVersion: '2.1.322', nativeSha256, sourceByteRanges,
  boundary: 'Pinned native helpers with controlled cache, model normalization and context-window providers; not native CLI acceptance.',
  clipCases, limitCases, fenceCases};
const fixturePath = new URL('./fixtures/file_memory_fence_322.json', import.meta.url);
const json = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) assert.equal(readFileSync(fixturePath, 'utf8'), json);
else writeFileSync(fixturePath, json);
console.log(JSON.stringify({status: 'passed', cases: clipCases.length + limitCases.length + fenceCases.reduce((sum, value) => sum + value.expected.length, 0), ranges: sourceByteRanges.length}));
