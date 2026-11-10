import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import {fileURLToPath} from 'node:url';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: node task_contract_319_oracle.mjs <official 2.1.319 binary> [--check]');
const binary = fs.readFileSync(binaryPath);
const hash = bytes => createHash('sha256').update(bytes).digest('hex');
const artifactSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(hash(binary), artifactSha256, 'Use the pinned official darwin-arm64 executable');
const ranges = [];
function take(label, first, after, from) {
  const start = binary.indexOf(first, from);
  const endExclusive = binary.indexOf(after, start + first.length);
  assert(start >= from && endExclusive > start && endExclusive - start < 20000, label);
  const bytes = binary.subarray(start, endExclusive);
  ranges.push({label, start, endExclusive, sha256: hash(bytes)});
  return bytes.toString('utf8');
}

// The schema adapters observe declarations only; they do not implement Zod
// parsing. Calls, coercers, dependency/metadata helpers and result mappers below
// execute the shipped JavaScript with explicit in-memory host facts.
class Schema {
  constructor(spec = {}, optional = false) { this.spec = spec; this.isOptional = optional; }
  optional() { return new Schema(this.spec, true); }
  describe(description) { return new Schema({...this.spec, description}, this.isOptional); }
  nullable() { return new Schema({anyOf: [this.spec, {type: 'null'}]}, this.isOptional); }
  catch() { return this; }
  or(other) { return new Schema({type: this.spec.type, enum: [...this.spec.enum, ...other.spec.enum]}); }
  extend() { return this; }
}
function object(fields, strict) {
  const required = Object.keys(fields).filter(key => !fields[key].isOptional);
  return new Schema({type: 'object', properties: Object.fromEntries(Object.entries(fields).map(([key, value]) => [key, value.spec])), ...(required.length ? {required} : {}), ...(strict ? {additionalProperties: false} : {})});
}
const clone = value => JSON.parse(JSON.stringify(value));
const state = {swarms: false, teammate: false, agentName: undefined, rows: new Map(), nextId: 1, mailboxCount: 0};
const world = vm.createContext({
  p: callback => callback, dn: definition => definition,
  o: () => new Schema({type: 'string'}), I: () => new Schema({type: 'boolean'}),
  oe: () => new Schema(), me: () => new Schema({type: 'object'}),
  T: item => new Schema({type: 'array', items: item.spec}),
  u: fields => object(fields, false), Fe: fields => object(fields, true),
  B: values => new Schema({type: 'string', enum: values}), C: value => new Schema({type: 'string', enum: [value]}),
  UCe: () => new Schema({type: 'string', enum: ['pending', 'in_progress', 'completed']}),
  D: value => value !== null && typeof value === 'object' && !Array.isArray(value),
  Nge: () => true, Zo: () => state.swarms, Q0: () => state.teammate,
  Tv: () => state.agentName, Ec: () => undefined, PO: () => undefined,
  Hi: () => ({taskList: {}}), Yv: async () => { state.mailboxCount++; }, YC: () => 'fixture',
  _: JSON.stringify, Rn: 'Create a new task in the task list',
  p_: 'TaskCreate', jZ: 'TaskGet', CS: 'TaskUpdate', XP: 'TaskList', ya: 'TaskStop',
  bSr: ['KillShell', 'KillBash'], BY: () => false, jDi: '', OX: () => ({}),
  kro: 'TaskCreated', K8t: 'TaskCompleted',
  aHi: async (_list, task) => { const id = String(state.nextId++); state.rows.set(id, {...clone(task), id}); return id; },
  ZY: async (_list, id) => state.rows.has(id) ? clone(state.rows.get(id)) : undefined,
  LI: async () => [...state.rows.values()].map(clone),
  BCe: async (_list, id, patch) => { const old = state.rows.get(id); if (old) state.rows.set(id, {...old, ...clone(patch)}); },
  cQt: async (_list, id) => {
    const success = state.rows.delete(id);
    for (const task of state.rows.values()) { task.blocks = task.blocks.filter(value => value !== id); task.blockedBy = task.blockedBy.filter(value => value !== id); }
    return success;
  },
  bce: () => undefined,
  rCt: async () => state.stopResult,
});
for (const source of [
  take('first-line helper tt', 'function tt(t,n){', 'function j9(', 259203000),
  take('first-line helper Wn', 'function Wn(t){', 'function Dt(', 259203600),
  take('TaskCreate and TaskUpdate input coercion and steering', 'var Uir=new Set(', 'var Xir=', 271275000),
  take('metadata merge and dependency helper', 'function U0t(e,n){', 'async function Dt(', 268896600),
  take('TaskCreate definition', 'function En(){', ';var An=', 278717900),
  take('TaskGet definition', 'var An=', ';var Dn=', 278722000),
  take('TaskUpdate definition', 'var Dn=', ';var Mn=', 278724000),
  take('TaskList definition', 'var Mn=', ';function jn(', 278730000),
  take('TaskStop definition', 'var Jo=p(', ';function Qo(', 278604000),
]) vm.runInContext(source, world, {timeout: 1000});
const api = vm.runInContext('({TaskCreate: In, TaskGet: Pn, TaskUpdate: Ln, TaskList: Bn, TaskStop: Qe, coerceCreate: f9t, steerCreate: mbi, coerceUpdate: vPt})', world);
const units = text => Array.from({length: text.length}, (_, index) => text.charCodeAt(index));
const coercionInputs = [
  null, [], {}, {subject: 'Only subject'}, {description: '  First line\nSecond line  '},
  {description: '\nSecond line'}, {description: '\ufeffShip it\ufeff'},
  {description: '\u0085Ship it\u0085'}, {subject: '\ufeff', priority: 1}, {subject: '\u0085'},
  {description: `${'😀'.repeat(30)} ${'a'.repeat(80)}`}, {description: 'a'.repeat(81)},
  {description: ` ${'x'.repeat(65)} more words beyond eighty `},
  {description: '\ud800First line\nSecond line\udfff'},
  {task: 'Nested task'}, {task: {title: 'Nested title', content: 'Nested body'}},
  {title: 'Alias title', content: 'Alias body', active_form: 'Working'},
  {subject: 'Ship it', description: 'Do the work', priority: 1, other: true},
  {subject: 's', description: 'd', activeForm: 2, metadata: []},
  {subject: 's', description: 'd', tasks: []}, {tasks: []}, {todos: []},
  {prompt: 'Delegate', subagent_type: 'Explore'}, {task: {prompt: 'Delegate'}},
  {subject: 's', description: 'd', prompt: 'Discard'},
  {id: '1', active_form: 'Working'}, {task_id: '2'}, {id: '\ufeff'}, {id: '\u0085'},
];
const coercionCases = coercionInputs.map(input => {
  const create = api.coerceCreate(input), update = api.coerceUpdate(input);
  return {inputJson: JSON.stringify(input), createJson: JSON.stringify(create), updateJson: JSON.stringify(update), steer: api.steerCreate(input)};
});
const task = (id, subject, extra = {}) => ({id, subject, description: 'Task details', status: 'pending', blocks: [], blockedBy: [], ...extra});
const executionCases = [];
async function execute(name, tool, input, seed = [], options = {}) {
  Object.assign(state, {swarms: options.swarms ?? false, teammate: options.teammate ?? false, agentName: options.agentName, rows: new Map(seed.map(row => [row.id, clone(row)])), nextId: seed.length + 1, mailboxCount: 0});
  const host = {runTaskHooks: async function* () {}};
  const output = await api[tool].create(host).call(input, {signal: new AbortController().signal});
  const rendered = api[tool].mapToolResultToToolResultBlockParam(output.data, 'fixture-use');
  executionCases.push({name, tool, inputJson: JSON.stringify(input), seedJson: JSON.stringify(seed), options, dataJson: JSON.stringify(output.data), modelUnits: units(rendered.content), mailboxCount: state.mailboxCount});
}
await execute('create-retains-saved-description', 'TaskCreate', {subject: 'Ship it', description: 'Saved task details'});
await execute('create-utf16', 'TaskCreate', {subject: 'Ship\ud800😀', description: 'Body\udfff\nsecond line'});
await execute('get-missing', 'TaskGet', {taskId: '404'});
await execute('get-dependencies', 'TaskGet', {taskId: '1'}, [task('1', 'First', {blocks: ['2'], blockedBy: ['3', '4'], owner: 'Not rendered', activeForm: 'Working', metadata: {private: true}})]);
await execute('get-utf16', 'TaskGet', {taskId: '1'}, [task('1', '\ud800😀', {description: 'Body\udfff'})]);
await execute('list-empty', 'TaskList', {});
await execute('list-internal-and-resolved-blocker', 'TaskList', {}, [task('1', 'First', {owner: 'alice', blockedBy: ['2', '3']}), task('2', 'Done', {status: 'completed'}), task('3', 'Internal', {metadata: {_internal: true}})]);
await execute('list-empty-owner-utf16', 'TaskList', {}, [task('1', '\ud800', {owner: '', blockedBy: ['404']})]);
await execute('list-internal-js-truthiness', 'TaskList', {}, [task('1', 'Visible', {metadata: {_internal: 0}}), task('2', 'Hidden', {metadata: {_internal: []}})]);
await execute('update-missing', 'TaskUpdate', {taskId: '404'});
await execute('update-noop', 'TaskUpdate', {taskId: '1', subject: 'First', status: 'pending'}, [task('1', 'First')]);
await execute('update-all-fields', 'TaskUpdate', {taskId: '1', subject: 'Changed', description: 'New body', activeForm: 'Changing', owner: 'alice', metadata: {drop: null, add: 1}, status: 'in_progress', addBlocks: ['2'], addBlockedBy: ['3']}, [task('1', 'First', {metadata: {drop: true}}), task('2', 'Second'), task('3', 'Third')]);
await execute('update-dependency-dedup', 'TaskUpdate', {taskId: '1', addBlocks: ['2', '2'], addBlockedBy: ['3', '3']}, [task('1', 'First', {blocks: ['2'], blockedBy: ['3']}), task('2', 'Second'), task('3', 'Third')]);
await execute('update-delete', 'TaskUpdate', {taskId: '1', status: 'deleted', subject: 'Ignored'}, [task('1', 'First')]);
await execute('update-complete', 'TaskUpdate', {taskId: '1', status: 'completed'}, [task('1', 'First')]);
await execute('update-teammate-complete', 'TaskUpdate', {taskId: '1', status: 'completed'}, [task('1', 'First')], {swarms: true, teammate: true, agentName: 'alice'});
await execute('update-teammate-already-complete', 'TaskUpdate', {taskId: '1', status: 'completed'}, [task('1', 'First', {status: 'completed'})], {swarms: true, teammate: true, agentName: 'alice'});
await execute('update-auto-owner', 'TaskUpdate', {taskId: '1', status: 'in_progress'}, [task('1', 'First')], {swarms: true, teammate: true, agentName: 'alice'});
await execute('update-clear-owner', 'TaskUpdate', {taskId: '1', owner: ''}, [task('1', 'First', {owner: 'alice'})], {swarms: true});
const replacementCases = [
  {name: 'create-edited', tool: 'TaskCreate', data: {task: {id: '42', subject: 'Edited subject', description: 'Edited body'}}},
  {name: 'create-edited-utf16', tool: 'TaskCreate', data: {task: {id: 'id\ud800', subject: 'Edited\udfff😀', description: 'Body'}}},
  {name: 'get-replaced-with-missing', tool: 'TaskGet', data: {task: null}},
  {name: 'get-edited-utf16', tool: 'TaskGet', data: {task: task('id\ud800', 'Edited\udfff😀', {description: 'Body\ud800\nnext', status: 'in_progress', blockedBy: ['2\udfff', '3'], blocks: ['4\ud800']})}},
  {name: 'list-replaced-with-empty', tool: 'TaskList', data: {tasks: []}},
  {name: 'list-edited-utf16', tool: 'TaskList', data: {tasks: [
    {id: 'id\ud800', subject: 'Edited\udfff😀', status: 'in_progress', owner: 'owner\ud800', blockedBy: ['2\udfff', '3']},
    {id: '2', subject: 'Empty owner', status: 'pending', owner: '', blockedBy: []},
    {id: '3', subject: 'No owner', status: 'completed', blockedBy: []},
  ]}},
].map(({name, tool, data}) => ({name, tool, dataJson: JSON.stringify(data), modelUnits: units(api[tool].mapToolResultToToolResultBlockParam(data, 'replacement-use').content)}));
const stopCases = [];
for (const scenario of [
  {name: 'running', taskId: 'b12345678', taskType: 'local_bash', command: 'sleep 5'},
  {name: 'utf16', taskId: 'a12345678', taskType: 'local_agent', command: 'Agent\ud800😀'},
  {name: 'ended-live-loop', taskId: 'a12345678', taskType: 'local_agent', command: 'Agent work', note: 'had already ended (completed) but its loop had not exited; re-signalled it and killed 0 process group(s). The record remains listed while the loop is still live.'},
]) {
  state.stopResult = scenario;
  const output = await api.TaskStop.create({}).call({task_id: scenario.taskId}, {});
  stopCases.push({name: scenario.name, dataJson: JSON.stringify(output.data), modelUnits: units(api.TaskStop.mapToolResultToToolResultBlockParam(output.data, 'use').content)});
}
const surfaces = Object.fromEntries(await Promise.all(['TaskCreate', 'TaskGet', 'TaskList', 'TaskUpdate', 'TaskStop'].map(async name => [name, {description: await api[name].description(), searchHint: api[name].searchHint, inputSchema: api[name].inputSchema.spec, concurrencySafe: api[name].isConcurrencySafe()}])));
const fixture = {version: '2.1.319', artifactSha256, authority: 'shipped JavaScript evaluated with explicit schema-declaration adapters and in-memory host facts; no application bootstrap, native Zod parser, filesystem, real registry or provider', ranges, surfaces, coercionCases, executionCases, replacementCases, stopCases};
const outputPath = path.join(path.dirname(fileURLToPath(import.meta.url)), 'fixtures', 'task_contract_319.json');
const output = JSON.stringify(fixture, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(outputPath, 'utf8'), output, 'task contract fixture drift');
else fs.writeFileSync(outputPath, output);
console.log(`${process.argv.includes('--check') ? 'Checked' : 'Wrote'} ${coercionCases.length} coercions, ${executionCases.length} task calls, ${replacementCases.length} replacements and ${stopCases.length} stop projections`);
