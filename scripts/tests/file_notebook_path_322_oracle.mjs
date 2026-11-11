#!/usr/bin/env node
// Execute bounded, digest-checked native helpers. No native program or network
// is run; fixtures retain only inputs, outputs, and source receipts.
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
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const sourceByteRanges = [];
function take(name, start, end, digest, first, next) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 4000);
  assert.equal(sha256(bytes), digest, name);
  assert(bytes.toString('utf8').startsWith(first));
  assert.equal(binary.subarray(end, end + Buffer.byteLength(next)).toString('utf8'), next);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}
const nfc = take('path NFC', 260574571, 260574612,
  '9fcd04e31340b326cfb10c35bcf77fa79183f51464cca2488de5349155ef0a04',
  'function pn(t){', 'function dr(');
const normalize = take('path normalization and validation', 261777606, 261778525,
  'c67bdac14f531f7345a61bd5601417db4acc752ea7afd7f540dbdd3f0b0bb5f0',
  'class d3 extends Error', 'function The(');
const notebookValidation = take('notebook and cell selector validation', 274283692, 274284312,
  '48f000978c73c70e517dbdccd94a7fe1084eed59b282e1963150bcb5d17de025',
  'function gMt(', 'function Cls(');
const mutate = take('notebook cell mutation and file serialization', 275888610, 275889388,
  'bd24accdcb9671920d8d0f9962018bd81e5917c977ad39abf14b41a1881c4e17',
  'let Ct=FXt(st.cells,s,y);', 'if(Buffer.byteLength(BUe(Dt,Ke)');
const mapper = take('notebook model result renderer', 275884972, 275885500,
  '4ce528e11e9bb5ffa7cc14b5069c34082d7958c5f8156e6f19b88892708e5222',
  'mapToolResultToToolResultBlockParam(', ',create(e){');
const validateInput = take('notebook input validation', 275885813, 275887504,
  'a064b72b6e277ba6119b30dbe89965c7b13f1221762ffbb848ab4a2d40a264a0',
  'async validateInput(', ',async call(');

const cwd = '/workspace/project', home = '/home/tester';
const pathContext = vm.createContext({
  P: undefined, wl: () => cwd, u: () => home, x: () => 'darwin',
  h: path.posix.normalize, v: path.posix.join, g: path.posix.isAbsolute,
  y: path.posix.resolve, kx: Error,
});
vm.runInContext(nfc + normalize, pathContext);
const paths = [
  '', '  ', '.', '..', 'a/../source.txt', '/../../tmp/source.txt',
  '/tmp/one//two/./', '/tmp/one/.', 'nested/', '/tmp/',
  '~', '~/notes/../file.txt', '~someone/file',
  '  file.txt\n', '\ufeffname.txt\u00a0', '\u0085name.txt\u0085',
  'cafe\u0301.txt', '/tmp/cafe\u0301.txt', '日本語/😀.txt',
  'a /.', 'a /b/..', 'a\0b',
].map(input => {
  pathContext.INPUT = input;
  try { return { input, expected: { path: vm.runInContext('X_(INPUT)', pathContext) } }; }
  catch (error) { return { input, expected: { error: error.message } }; }
});

const base = {
  cells: [
    { cell_type: 'code', id: 'c1', source: ['a\n', 'b'], metadata: {}, outputs: [{ output_type: 'stream', text: 'old' }], execution_count: 3 },
    { cell_type: 'markdown', id: 'c2', source: '# title', metadata: {} },
  ],
  metadata: { language_info: { name: 'julia' }, '10': 'ten', '2': 'two' },
  nbformat: 4, nbformat_minor: 5,
};
const scenarios = [
  ['replace array source', base, { cell_id: 'c1', new_source: 'new' }],
  ['delete array source', base, { cell_id: 'c1', edit_mode: 'delete', new_source: '' }],
  ['replace type', base, { cell_id: 'c1', cell_type: 'markdown', new_source: '# new' }],
  ['insert after anchor', base, { cell_id: 'c1', edit_mode: 'insert', cell_type: 'code', new_source: 'fresh' }],
  ['insert at start', base, { edit_mode: 'insert', cell_type: 'markdown', new_source: 'first' }],
  ['old format insert', { ...base, nbformat_minor: 4 }, { edit_mode: 'insert', cell_type: 'code', new_source: 'old format' }],
  ['old format replace keeps input id', { ...base, nbformat_minor: 4 }, { cell_id: 'cell-0', new_source: 'replaced' }],
  ['empty identifier insert', base, { cell_id: '', edit_mode: 'insert', cell_type: 'code', new_source: '' }],
  ['missing identifier', base, { new_source: 'x' }],
  ['bad identifier', base, { cell_id: 'missing', new_source: 'x' }],
  ['index selector', base, { cell_id: 'cell-01', new_source: 'index one' }],
  ['index outside notebook', base, { cell_id: 'cell-2', new_source: 'x' }],
  ['empty notebook insert', { cells: [], nbformat: 4, nbformat_minor: 5 }, { edit_mode: 'insert', cell_type: 'code', new_source: 'first' }],
  ['surrogate and numeric serialization', { ...base, extra: '\ud800', number: 1e21, small: 1e-7 }, { cell_id: 'c2', new_source: '\ud800😀' }],
  ['invalid cells', { cells: [{ source: [1] }] }, { cell_id: 'cell-0', new_source: 'x' }],
  ['missing cells', {}, { edit_mode: 'insert', cell_type: 'code', new_source: 'x' }],
];
const notebooks = scenarios.map(([name, notebook, supplied]) => {
  const input = { edit_mode: 'replace', new_source: '', ...supplied };
  const sandbox = vm.createContext({
    NOTEBOOK: JSON.stringify(notebook), INPUT: input,
    OYs: () => 'native123456', _: JSON.stringify, ha: value => value,
    FT: Error, gMt: undefined,
  });
  vm.runInContext(notebookValidation + `; var renderer={${mapper}};`, sandbox);
  sandbox.INVALID = 'Notebook file is not a valid Jupyter notebook (top-level "cells" must be an array of cell objects, each with a string or string-array "source").';
  const expected = vm.runInContext(`(() => {
    let st=JSON.parse(NOTEBOOK), {cell_id:s,cell_type:h,edit_mode:y,new_source:r}=INPUT;
    if(!gMt(st))return {error:INVALID};
    try { ${mutate}
      const data={new_source:r,old_source:It,cell_type:h??'code',language:_t,edit_mode:y,cell_id:wt||undefined,error:''};
      return {data,updatedFile:Dt,model:renderer.mapToolResultToToolResultBlockParam(data,'tool-1').content};
    } catch(error) { return {error:error.message}; }
  })()`, sandbox);
  return { name, notebookJson: JSON.stringify(notebook), input, expected: JSON.parse(JSON.stringify(expected)) };
});
// Replay the unchanged validation method with explicit path/policy/clock/I/O
// dependencies. This establishes hard JSON errors and extension sensitivity.
const validations = [];
for (const [name, configuration] of [
  ['uppercase extension', { file: '/w/NB.IPYNB' }],
  ['malformed JSON', { raw: '{' }],
  ['BOM notebook JSON', { raw: '\ufeff' + JSON.stringify(base) }],
  ...['null', 'false', '0', '""'].map(raw => ['falsy notebook JSON ' + raw, {raw}]),
  ['invalid cell source', { raw: '{"cells":[{"source":[1]}]}' }],
  ['mtime advanced', { timestamp: 1, mtime: 2 }],
  ['unread notebook', { read: false }],
]) {
  const config = { file: '/w/nb.ipynb', raw: JSON.stringify(base), timestamp: 1, mtime: 1, read: true, ...configuration };
  const sandbox = vm.createContext({
    e: { session: {}, readFileState: { get: () => config.read ? { timestamp: config.timestamp } : undefined } },
    UC:()=>undefined,V$:()=>undefined,Y$:()=>undefined,MY:()=>undefined,u2:()=>undefined,
    X_:value=>value,IYs:path.posix.extname,cl:()=>false,SKs:()=>false,
    $P:async()=>config.mtime,Pb:async()=>({content:config.raw}),Ze:value=>{try{return JSON.parse(value)}catch{return null}},
    XIe:'Notebook file is not a valid Jupyter notebook (top-level "cells" must be an array of cell objects, each with a string or string-array "source").',
    Zs:'NotebookEdit',lV:104857600,
  });
  vm.runInContext(notebookValidation + `; var validator={${validateInput}};`, sandbox);
  const expected = await sandbox.validator.validateInput({ notebook_path: config.file, cell_id: 'c1' }, {});
  validations.push({ name, configuration: config, expected: JSON.parse(JSON.stringify(expected)) });
}

const fixture = { schemaVersion: 1, claudeCodeVersion: '2.1.322', nativeSha256, sourceByteRanges, cwd, home, paths, notebooks, validations };
const output = new URL('./fixtures/file_notebook_path_322.json', import.meta.url);
const bytes = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) assert.equal(readFileSync(output, 'utf8'), bytes);
else writeFileSync(output, bytes);
process.stdout.write(`${paths.length} paths, ${notebooks.length} notebook mutations, ${validations.length} validations; ${sourceByteRanges.length} pinned ranges\n`);
