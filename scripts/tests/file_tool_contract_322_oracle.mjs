#!/usr/bin/env node
// Native prompt bytes and ordered schema-factory metadata; constructors and
// unavailable attached-machine support are controlled, not native Zod emission.
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
function take(name, start, end, digest, first, next) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 5000, `${name}: bound`);
  assert.equal(hash(bytes), digest, name);
  assert(bytes.toString('utf8').startsWith(first), `${name}: first`);
  assert.equal(binary.subarray(end, end + Buffer.byteLength(next)).toString('utf8'), next);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}
const globPrompt = take('Glob prompt', 265921819, 265922452,
  '60991c600b02111e26461d2ae0f75d2cd82756a3aada758d9f2abfe036129adf', 'function tPs(', ';var $t=');
const grepPrompt = take('Grep prompt', 265926023, 265927567,
  '5945c4e3eba85200bd625f4e2eef59101666f600e99975c0bbcb1765129eeafd', 'function rPs(', 'function xNe(');
const writePrompt = take('Write prompt', 269286943, 269287904,
  '836f9720362981304e981fb25d82cd64b00d131d1a1c038d23d98320b9b14b48', 'function o(){return`\n- If this is an existing file', 'var P_=');
const editPrompt = take('Edit prompt', 275864785, 275866894,
  'dc83d091da45eff08c40bab1ffa5ca0802efc9cab4d4b97c92834ed8c4f339e4', 'function EYs(', 'var ivt=');
const notebookPrompt = take('Notebook prompt and description', 275881305, 275882047,
  '5c6fc8b476bc57003d5dfa91cbe0e6c947daf24ca80f047a680d3971dcaf087d', 'var G_o=', ';var DYs=');
const grepRewrite = take('Grep rewrite prompt', 265915131, 265915660,
  'b547cffe13ec07d01ed61ecaa2e5078e91cb379bd663ffa1bfb04b4eda6807c0', 'we=`Content search', ',p3i=');
const editSchemas = take('Edit input and output schema factories', 272387043, 272388322,
  'a585dc10f041197f7f16d43f87c5da9ff23a059d43c5e83b2b89ca742c77ece6', 'var dCe=', ';var bln=');
const writeSchemas = take('Write input and output schema factories', 272389573, 272390687,
  '848433aba128c136e7c4cad5f2e4c5115049171acc738e98fed1a9f446daaeab', 'var vln=', ',Xir=');
const globSchemas = take('Glob input and output schema factories', 272419354, 272420656,
  '135ec795a02a3eb53ebcf419147b84834f5444d1156c9300784dbc7e2b2255b9', 'var Nar=', 'function $ar(');
const grepInput = take('Grep input schema factory', 272424756, 272427188,
  '4429fde78f69f39b68097cc3e5b2880182bb0cfc94bd98c47d56841a63d0ad54', 'var War=', ',ddn=');
const grepOutput = take('Grep output schema factory', 272427522, 272427837,
  '9573c419a5ec8aa1dd067bd5536a23aa90993574841e8e2d27d8d9a2cddbcb7f', 'var qar=', ',ZN=');
const notebookSchemas = take('Notebook input and output schema factories', 275882048, 275883558,
  'f9659ae0555838d56b858af982e101f388ca586ec20e1e2e4284e729c4e2d4c9', 'var DYs=', 'function q_o(');
const staged = take('staged result wording factory', 272341539, 272341872,
  '6f6e672aedbe0eb9879ffefebb8f2a563bf958d3aaf83c4da64c896a3d965dae', 'var Jsr=', ',Jve=');
const writeDescription = take('Write description', 272391587, 272391653,
  'd2827bdd3df262de8f23a49003e4a0f13b0a3502822abaa3e4b6570567eac8c3', 'async description()', ',');
const editDescription = take('Edit description', 275868650, 275868703,
  '4f96faec20775fa8e9a026b24345df883c0f466e698fe51e045ef728850e919b', 'async description()', ',');
const currentSuffix = take('current file state suffix', 265923018, 265923095,
  '2deef089fa750c7d6dc3e78e6abc61744304453ce7bad6f124a134b0137269e1', 'Ewo=', ',Wt=');
const editResultMapper = take('Edit context-sensitive result mapper', 275874560, 275875386,
  'b41a50433da86fce5c63d7212a1b4d9203a96cb00e4f4499059437915be874bc', 'mapToolResultToToolResultBlockParam(e,n)', '});async function xYs');

class Schema {
  constructor(value) { this.value = value; this.isOptional = false; this.catchesUndefined = false; }
  describe(description) { this.value.description = description; return this; }
  optional() { this.isOptional = true; return this; }
  nullable() { this.value.type = [this.value.type, 'null']; return this; }
  default(value) { this.value.default = value; return this; }
  catch(value) { assert.equal(value, undefined); this.catchesUndefined = true; return this; }
}
function shape(fields) {
  return new Schema({ type: 'object', additionalProperties: false,
    required: Object.entries(fields).filter(([, field]) => !field.isOptional).map(([name]) => name),
    properties: Object.fromEntries(Object.entries(fields).map(([name, field]) => [name, field.value])),
    catchUndefinedFields: Object.entries(fields).filter(([, field]) => field.catchesUndefined).map(([name]) => name),
  });
}
const world = vm.createContext({
  J0: ({ leanPrompt }) => leanPrompt, u9: ({ leanRewrite }) => leanRewrite,
  f6: () => world.style, B_t: () => world.tabSeparator, k: () => world.minimalAnchor,
  Ge: 'Read', lt: 'Edit', mr: 'Grep', Oe: 'Bash', ft: 'Agent',
  p: factory => factory, Ne: shape, u: shape,
  o: () => new Schema({ type: 'string' }), v: () => new Schema({ type: 'number' }),
  I: () => new Schema({ type: 'boolean' }),
  B: values => new Schema({ type: 'string', enum: [...values] }),
  A: items => new Schema({ type: 'array', items: items instanceof Schema ? items.value : items }),
  uv: schema => schema, CU: schema => schema, v7: () => ({}),
});
vm.runInContext(globPrompt + ';' + grepPrompt + writePrompt + editPrompt + notebookPrompt
  + ';var ' + grepRewrite + ';' + staged + ';' + editSchemas + ';' + writeSchemas
  + ';' + globSchemas + grepInput + ';' + grepOutput + ';' + notebookSchemas
  + `var writeDescription = ({${writeDescription}}).description;`
  + `var editDescription = ({${editDescription}}).description;`, world);
const cases = [];
for (const lean of [false, true]) {
  for (const style of ['default', 'other']) {
    Object.assign(world, { style, tabSeparator: false, minimalAnchor: false });
    for (const [tool, call] of [
      ['Glob', 'tPs("claude-opus-5-5",LEAN)'], ['Grep', 'rPs("claude-opus-5-5",LEAN,false)'],
      ['Write', 'HBi("claude-opus-5-5",LEAN)'], ['Edit', 'N_o("claude-opus-5-5",LEAN)'],
      ['NotebookEdit', 'z_o'],
    ]) {
      const output = vm.runInContext(call.replace('LEAN', String(lean)), world);
      cases.push({ tool, lean, style, output, sha256: hash(output), bytes: Buffer.byteLength(output) });
    }
  }
}
const variants = [];
for (const tabSeparator of [false, true]) {
  for (const minimalAnchor of [false, true]) {
    Object.assign(world, { tabSeparator, minimalAnchor });
    for (const lean of [false, true]) {
      const output = vm.runInContext(`N_o("claude-opus-5-5",${lean})`, world);
      variants.push({ tool: 'Edit', lean, tabSeparator, minimalAnchor, output });
    }
  }
}
variants.push({ tool: 'Grep', lean: true, rewrite: true,
  output: vm.runInContext('rPs("claude-opus-5-5",true,true)', world) });
Object.assign(world, { style: 'default', tabSeparator: false, minimalAnchor: false });
const descriptions = [
  { tool: 'Edit', output: await world.editDescription() },
  { tool: 'Write', output: await world.writeDescription() },
  { tool: 'Glob', output: vm.runInContext('tPs(undefined,undefined)', world) },
  { tool: 'Grep', output: vm.runInContext('rPs(undefined,undefined,false)', world) },
  { tool: 'NotebookEdit', output: world.G_o },
];
// The independently bundled Write prompt calls its pre-read helper `o`; the
// schema chunks use the same minified name for the string constructor.
world.o = () => new Schema({ type: 'string' });
const schemas = [];
for (const [tool, input, output] of [
  ['Edit', 'dCe()', 'dJe()'], ['Write', 'vln()', 'Aln()'],
  ['Glob', 'Nar()', 'Far()'], ['Grep', 'War()', 'qar()'], ['NotebookEdit', 'DYs()', 'LYs()'],
]) schemas.push({ tool, input: JSON.parse(JSON.stringify(vm.runInContext(input, world).value)),
  output: JSON.parse(JSON.stringify(vm.runInContext(output, world).value)) });
world.eCe = () => '';
vm.runInContext('var ' + currentSuffix + ';var editResultMapper = ({' + editResultMapper + '}).mapToolResultToToolResultBlockParam;', world);
const editResultMessages = [];
for (const replaceAll of [false, true]) for (const staleRecovered of [false, true])
  for (const contentNotInModelContext of [false, true]) {
    const input = { filePath: '/oracle/file', replaceAll, staleRecovered, contentNotInModelContext };
    editResultMessages.push({ input, output: world.editResultMapper(input, 'tool-1').content });
  }

const prior = JSON.parse(readFileSync(new URL('./fixtures/tool_prompts_319.json', import.meta.url)));
const comparisonTo319 = [];
for (const row of cases.filter(row => row.style === 'default')) {
  const previous = row.tool === 'NotebookEdit'
    ? prior.otherToolCases.find(candidate => candidate.tool === row.tool)
    : prior.cases.find(candidate => candidate.tool === row.tool && candidate.lean === row.lean);
  assert(previous, row.tool);
  comparisonTo319.push({ tool: row.tool, lean: row.lean, equal: row.output === previous.output });
}
assert(comparisonTo319.every(row => row.equal), 'A default prompt changed from319; update production template');
const fixture = { schemaVersion: 1, claudeCodeVersion: '2.1.322', nativeSha256, sourceByteRanges,
  controls: { attachedMachines: 'unavailable; native v7() contributes no _host input field',
    schemaMetadata: 'Native schema factories execute through recording scalar/array/object constructors. Their ordered descriptors are verified; native Zod JSON-schema serialization is not executed.',
    coercion: 'uv/CU retain primitive field metadata; coercion execution is outside these descriptor checks',
    promptSelection: 'explicit lean flag; default tab separator and minimal-anchor feature are false; style controls are explicit' },
  cases, variants, descriptions, schemas, comparisonTo319, editResultMessages };
const fixturePath = new URL('./fixtures/file_tool_contract_322.json', import.meta.url);
const output = JSON.stringify(fixture, null, 2) + '\n';
if (args.includes('--check')) assert.equal(readFileSync(fixturePath, 'utf8'), output);
else writeFileSync(fixturePath, output);
process.stdout.write(`${cases.length} prompts, ${variants.length} feature variants, ${descriptions.length} descriptions, ${schemas.length * 2} schema factories, ${editResultMessages.length} Edit result messages; ${sourceByteRanges.length} pinned ranges verified\n`);
