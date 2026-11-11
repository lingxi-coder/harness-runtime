#!/usr/bin/env node
// Execute the shipped Zod modules and output factories. No parser/serializer
// constructors are replaced. Only host configuration and factory memoization
// are supplied; extracted proprietary code stays in memory.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import vm from 'node:vm';

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
assert(args.filter(arg => !arg.startsWith('--')).length <= 1);
const binary = readFileSync(args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.322'));
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = hash(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const sourceByteRanges = [];
function take(name, start, end, sha256) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 100_000, `${name}: bound`);
  assert.equal(hash(bytes), sha256, name);
  sourceByteRanges.push({ name, start, end, sha256 });
  return bytes.toString('utf8');
}
const util = take('native Zod utility module', 260913877, 260925569,
  'b5a04d327a1f168024f6bb78466824994dcbadcdf984986ed08f4dcdb76ad9d9');
const zod = take('native Zod core and classic module', 260925570, 261020487,
  'd010acd51e312f3506c5c811c020d0cf7b970a48524aaf2a8691861061e900ad');
const staged = take('staged result wording factory', 272341539, 272341872,
  '6f6e672aedbe0eb9879ffefebb8f2a563bf958d3aaf83c4da64c896a3d965dae');
const edit = take('Edit output factory and nested shapes', 272387043, 272388322,
  'a585dc10f041197f7f16d43f87c5da9ff23a059d43c5e83b2b89ca742c77ece6');
const write = take('Write output factory', 272389573, 272390687,
  '848433aba128c136e7c4cad5f2e4c5115049171acc738e98fed1a9f446daaeab');
const notebook = take('NotebookEdit output factory', 275882048, 275883558,
  'f9659ae0555838d56b858af982e101f388ca586ec20e1e2e4284e729c4e2d4c9');
const glob = take('Glob output factory', 272419354, 272420656,
  '135ec795a02a3eb53ebcf419147b84834f5444d1156c9300784dbc7e2b2255b9');
const grep = take('Grep output factory', 272427522, 272427837,
  '9573c419a5ec8aa1dd067bd5536a23aa90993574841e8e2d27d8d9a2cddbcb7f');
function moduleBody(source) {
  assert.equal((source.match(/export\{/g) ?? []).length, 1);
  return source.replace(/import\{[^}]+\}from"[^"\n]+";/g, '')
    .replace(/export\{([^}]+)\};\s*$/, ';return {$1};');
}
const world = vm.createContext({});
Object.assign(world, vm.runInContext(`(()=>{${moduleBody(util)}})()`, world, { timeout: 10_000 }));
world.zod = vm.runInContext(`(()=>{${moduleBody(zod)}})()`, world, { timeout: 10_000 });
world.memoFactory = factory => factory;
const schemas = vm.runInContext(`(()=>{
  const {o,v,I,A,u,Ne,B}=zod;
  const p=memoFactory,uv=value=>value,v7=()=>({});
  ${staged};${edit};${write};${notebook}${glob}${grep};
  return {Edit:dJe(),Write:Aln(),NotebookEdit:LYs(),Glob:Far(),Grep:qar()};
})()`, world, { timeout: 10_000 });

const base = {
  Edit: { filePath: '/file', oldString: 'a', newString: 'b', originalFile: 'a',
    structuredPatch: [], userModified: false, replaceAll: false },
  Write: { type: 'update', filePath: '/file', content: 'b', structuredPatch: [], originalFile: 'a' },
  NotebookEdit: { new_source: 'b', cell_type: 'code', language: 'python', edit_mode: 'replace',
    notebook_path: '/book.ipynb', original_file: '{}', updated_file: '{}' },
  Glob: { durationMs: 1, numFiles: 1, filenames: ['/file'], truncated: false },
  Grep: { mode: 'files_with_matches', numFiles: 1, filenames: ['/file'] },
};
const cases = [];
function record(name, tool, value, rawJson) {
  // Reparse in the same realm as native Zod so Object prototype identity and
  // JavaScript number parsing match the actual source's JSON boundary.
  const inputJson = rawJson ?? JSON.stringify(value);
  world.inputJson = inputJson;
  world.schema = schemas[tool];
  const result = vm.runInContext('schema.safeParse(JSON.parse(inputJson))', world, { timeout: 10_000 });
  const outcome = result.success
    ? { outputJson: JSON.stringify(result.data) }
    : { issuesJson: JSON.stringify(result.error.issues, null, 2) };
  cases.push({ name, tool, inputJson, ...outcome });
}
for (const [tool, value] of Object.entries(base)) {
  record('valid', tool, value);
  record('missing-all', tool, {});
  record('wrong-root', tool, []);
  record('nullable-root', tool, null);
  const field = tool === 'NotebookEdit' ? 'new_source' : tool === 'Write' ? 'content' : 'newString';
  record('unknown-properties-and-exact-utf16', tool,
    ['Glob', 'Grep'].includes(tool)
      ? { extra: '\ud800', ...value, filenames: ['\ud800x\udfff'] }
      : { extra: '\ud800', ...value, [field]: '\ud800x\udfff' });
}
for (const tool of ['Edit', 'Write']) {
  record('nested-aggregate', tool, { ...base[tool], filePath: 7,
    structuredPatch: [
      { oldStart: 'x', oldLines: null, newStart: false, lines: [1, 'ok', {}] },
      false,
      { oldStart: 1, oldLines: 1, newStart: 1, newLines: 1, lines: [false, null] },
    ], gitDiff: { filename: 0, status: 'deleted', additions: 'x', changes: false, patch: 9 },
  });
  record('nested-stripping-and-optional-catch', tool, { ...base[tool], stagedWording: 42,
    structuredPatch: [{ oldStart: 1, oldLines: 1, newStart: 1, newLines: 1,
      lines: ['\ud800', '-a', '+b'], extra: true }],
    gitDiff: { filename: '/file', status: 'modified', additions: 1, deletions: 1,
      changes: 2, patch: '\udfff', repository: null, extra: true },
  });
  for (const word of ['review', 'card', 'cloud', 'linked', 'policy', 'invalid', null]) {
    record(`staged-wording-${word}`, tool, { ...base[tool], stagedWording: word });
  }
}
record('enum-aggregate', 'Write', { ...base.Write, type: 'delete', filePath: false, originalFile: 3 });
record('enum-non-string', 'Write', { ...base.Write, type: 3 });
record('enum-aggregate', 'NotebookEdit', { ...base.NotebookEdit, cell_type: 'raw', language: null,
  original_file: true, updated_file: 7 });
record('enum-non-string', 'NotebookEdit', { ...base.NotebookEdit, cell_type: 3 });
record('array-and-scalar-aggregate', 'Glob', { ...base.Glob,
  durationMs: null, filenames: [1, '\ud800', false, {}], numFiles: 'x', truncated: 7,
  totalMatches: 'x', countIsComplete: 1 });
record('array-enum-and-scalar-aggregate', 'Grep', { ...base.Grep,
  mode: 'invalid', numFiles: null, filenames: [1, '\ud800', false, {}], content: 7,
  numLines: 'x', timedOut: 1 });
record('non-finite-numbers', 'Glob', null,
  '{"durationMs":1e400,"numFiles":-1e400,"filenames":[],"truncated":false}');
const schemaSerializations = Object.entries(schemas).map(([tool, schema]) => ({
  tool, json: JSON.stringify(schema.toJSONSchema()),
}));
const fixture = { schemaVersion: 1, claudeCodeVersion: '2.1.322', nativeSha256, sourceByteRanges,
  controls: {
    parserAndSerializer: 'Actual shipped Zod modules and native output factories execute in an isolated Node VM; no type, enum, array, object, error, or JSON-schema implementation is substituted.',
    memoization: 'Factory p is an identity wrapper. Attached-machine input fields are unavailable; only output factories are used.',
    serializer: 'schema.toJSONSchema() with native defaults. This does not claim a different caller-selected draft or transport schema transform.',
  }, schemaSerializations, cases };
const fixturePath = new URL('./fixtures/file_output_contract_322.json', import.meta.url);
const output = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) assert.equal(readFileSync(fixturePath, 'utf8'), output);
else writeFileSync(fixturePath, output);
process.stdout.write(`${cases.length} native output cases, ${schemaSerializations.length} native schema serializations; ${sourceByteRanges.length} pinned ranges verified\n`);
