#!/usr/bin/env node
// Source-pinned Claude Code 2.1.289 $.agent.list / $.agent.spawn oracle.
// Usage: node scripts/tests/mods_agent_289_oracle.mjs <2.1.289-binary> [--check]
//
// The exact native reducers and wrapper are executed in a Node VM. Only host
// task rows, plugin maps and team-file contents are stubbed; no provider,
// network, or app startup is involved. The named-teammate call path is pinned
// and source-asserted because its surrounding launcher is not a pure helper.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const args = process.argv.slice(2);
const binaries = args.filter(arg => arg !== '--check');
assert.equal(
  binaries.length,
  1,
  'usage: node scripts/tests/mods_agent_289_oracle.mjs <2.1.289-binary> [--check]',
);
const binary = readFileSync(binaries[0]);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 macOS arm64 binary SHA-256',
);

const sourceWindows = [];
function take(label, start, end, expectedHash, startText) {
  assert(start >= 0 && end > start, `${label}: invalid source byte range`);
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: source range is short`);
  if (startText) {
    assert.equal(
      bytes.subarray(0, Buffer.byteLength(startText)).toString(),
      startText,
      `${label}: source start marker changed`,
    );
  }
  assert.equal(sha256(bytes), expectedHash, `${label}: source hash changed`);
  sourceWindows.push({label, start, end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

// Exact pinned native source windows, including their independent full hashes.
const terminalSource = take('terminal status predicate', 185139913, 185139979,
  '97b7f941c1c09455cb734a557cb33b37fcdd82297be2af30b4d77296dd6ccbcd', 'function Rs(');
const identitySource = take('agent task identity projection', 188340930, 188341386,
  '676c1b52ae40467ef77100eb7bd9601729b8e4a9617ee7c543f9f4ad619a1f44', 'function h_e(');
const activitySource = take('background Bash and monitor predicate', 188505877, 188506078,
  'b0cc613b8c0411e79e0d9470f30f95ed73402e90a6dfb1b477d0a75f55526120', 'var GTe=');
const idleReasonSource = take('idle keepalive reason', 188506130, 188506156,
  '56d4037ee4da4a91e82e03ab2289eb92f335720636fa323a924f19fa71bc3f38', 'var yG=');
const statusSource = take('PAt status projection and MAt membership', 188506311, 188507089,
  'd36381136a0e262d37de6838e1d3d623451a01899a02a2d78b43c6f512691a9e', 'function PAt(');
const listSource = take('ifo agent.list reducer', 188507089, 188507928,
  'e003640923c46f03788a530b61ae435e10be9c2ba9dbe9713195b5257d6cba15', 'async function ifo(');
const hookContextSource = take('hook caller and origin projection', 188508612, 188508693,
  '728dd2ce55ef1c33361b9377a85efb109048e09ca338f7258e2c689e0db4e4ee', 'var $At=');
const apiSpawnSource = take('agent wrapper and spawn helpers', 182843403, 182844999,
  '4f818cf4e1cd61e152a4503060f655e40c2c23d864427aa2ad027f668b3d6401', 'function af(');
const apiWrapperSource = take('rw agent.list/agent.spawn wrapper', 182844300, 182844999,
  '1f639d650c75229f9e9ebac6cf50ae7694bb4e6fea77fec2763158c8966b47a7', 'var rw=');
const spawnBridgeSource = take('VOt/YOt/c_o spawn result bridge', 188857631, 188858131,
  'f6999ad4718a1d17944ae61c5f02fc8d1f70eb16fe3b55f376fe7c4edb0c2e97', 'var VOt=');
const provenanceSource = take('CMr provenance writer', 188963827, 188963992,
  'b4a9186fc470ab1b5ad28b53ca6d5d8856ec7e8506c36d2bd8f562f18e339259', 'function CMr(');
const teamReaderSource = take('Py team-file reader, source-only', 186836941, 186837441,
  'de6f3328d7a7cbbcb620d50e432ef40c2af070bd6baf2f5fbe78814248f61ce6', 'async function Py(');
const spawnHookSource = take('bn agent.spawn hook dispatcher, source-only', 194820648, 194821115,
  '34306bd3e01fba34e855d3afee63788a83fd860c71c9f383cbca4beb3157f485', 'async function bn(');
const namedTeammateSource = take('named teammate call into bn, source-only', 194832917, 194834998,
  '5996c73dc5590ab0a8c2579029c04b31680eb434e399c45a4df621f5675b852e', 'let P=Ip(e),G=E??Io(b),S=await bn(');
const embeddedNameError = take('embedded missing-name-registry error resource', 78657088, 78657164,
  '059760913df7073fb0e0d12b842fa838978bbbefb9dc0dc0ab58142cdd0cf88a',
  "Cannot destructure property 'agentNameRegistry' from null or undefined value");
const embeddedProvenanceError = take('embedded missing-provenance error resource', 78657172, 78657246,
  'edda4f2c11efec10fe504625527248ef8eb90bae2d2154800f7fa49dae0e9207',
  "Cannot destructure property 'spawnProvenance' from null or undefined value");
const embeddedErrorResources = [embeddedNameError, embeddedProvenanceError];

assert.match(spawnHookSource, /Cc\("agent\.spawn"\)/);
assert.match(spawnHookSource, /core:\{\[?"agent\.spawn"/);
assert.match(namedTeammateSource, /isTeammate:!0/);
assert.match(namedTeammateSource, /await bn\(/);
assert.match(hookContextSource, /hookCaller:r\.plugin/);
assert.match(teamReaderSource, /async function Py\(/);
assert(apiSpawnSource.includes(apiWrapperSource), 'wrapper window must be part of executed source');

const rosterSource = activitySource + idleReasonSource + statusSource + listSource;
const spawnResolveSource = spawnBridgeSource;
const spawnProvenanceSource = provenanceSource;

const state = {
  taskContext: null,
  names: new Map(), // iteration shape is [display name, stable id]
  provenance: new Map(),
  teams: new Map(),
};
const context = vm.createContext({
  AbortController,
  Map, Set, Promise, Object, Array, String, Boolean, Error, TypeError,
  N: value => value !== null && typeof value === 'object' && !Array.isArray(value),
  Ny: value => value,
  Ie: Error,
  dQe: /^[A-Za-z0-9_-]{1,64}$/,
  te: value => [...value],
  L: values => values,
  ho: value => value,
  Hu: () => ({ toolContext: () => state.taskContext }),
  DQn: () => ({ namedAgents: () => ({ agentNameRegistry: state.names }) }),
  dt: () => ({ spawnProvenance: state.provenance }),
  Py: async teamName => state.teams.get(teamName),
  At: value => value,
});
vm.runInContext(
  terminalSource + identitySource + rosterSource + spawnResolveSource + spawnProvenanceSource + apiSpawnSource,
  context,
);

const local = (id, status, extra = {}) => ({
  type: 'local_agent', agentId: id, description: `${id} description`, agentType: 'Explore', status, ...extra,
});
const teammate = (agentId, status, extra = {}) => ({
  type: 'in_process_teammate', identity: { agentId, agentName: 'Teammate', teamName: 'team', ...extra.identity },
  spawnedWith: { description: 'team task', agentType: 'Explore', ...extra.spawnedWith },
  status, ...extra.task,
});
async function list(rows, { names = [], provenance = [], teams = [] } = {}) {
  state.names = new Map(names);
  state.provenance = new Map(provenance);
  state.teams = new Map(teams);
  state.taskContext = { taskRegistry: { all: () => Object.fromEntries(rows.map((row, i) => [`row-${i}`, row])) }, storageV5: {} };
  return await context.ifo('oracle-plugin');
}

// Native PAt truth table (the argument is whether a display name is available).
const pAtCases = [
  [local('a', 'running'), false, 'running'],
  [local('a', 'running', { isIdle: true }), false, 'waiting'],
  [teammate('t@team', 'running'), false, 'running'],
  [teammate('t@team', 'running', { task: { awaitingPlanApproval: true } }), false, 'waiting'],
  [teammate('t@team', 'running', { task: { isIdle: true } }), false, 'idle'],
  [teammate('t@team', 'running', { task: { isIdle: true, awaitingPlanApproval: true } }), false, 'waiting'],
  [local('a', 'completed', { finalizing: true, keepaliveReasons: new Set(['owned-child']) }), true, 'running'],
  [local('a', 'completed', { keepaliveReasons: new Set(['owned-child']) }), false, 'waiting'],
  [local('a', 'completed', { keepaliveReasons: new Set(['flag:idle-window']) }), false, 'idle'],
  [local('a', 'completed'), true, 'idle'],
  [local('a', 'completed'), false, 'completed'],
  [teammate('t@team', 'completed', { task: { finalizing: true, keepaliveReasons: new Set(['owned-child']) } }), true, 'completed'],
  [local('a', 'paused'), false, 'idle'],
  [local('a', 'failed'), false, 'failed'],
  [local('a', 'custom-status'), false, 'custom-status'],
];
for (const [row, hasName, expected] of pAtCases) {
  assert.equal(context.PAt(row, hasName), expected, `${row.type}/${row.status}/${expected}`);
}
console.log('PAt', JSON.stringify(pAtCases.map(([row, name, expected]) => ({
  type: row.type, input: row.status, hasName: name, expected,
}))));

// Agent-list ordering and duplicate winner: last nonterminal row wins; once
// any nonterminal row exists, later terminal duplicates cannot replace it.
// Terminal-only duplicates keep the last terminal row. Output order is first
// insertion of each stable id in Object.values(taskRegistry.all()).
const dedupRows = [
  local('agent-A', 'running', { description: 'A first active' }),
  local('agent-B', 'running', { description: 'B first-seen' }),
  local('agent-A', 'running', { description: 'A last active', parentAgentId: 'parent-A' }),
  local('agent-A', 'completed', { description: 'A later terminal' }),
  local('agent-D', 'completed', { description: 'D first terminal' }),
  local('agent-D', 'failed', { description: 'D last terminal' }),
];
const dedup = await list(dedupRows, {
  names: [['B mapped name', 'agent-B']],
  provenance: [['agent-A', { hookCaller: 'spawn-plugin', hookOrigin: ['spawn-plugin'] }]],
});
assert.deepEqual(JSON.parse(JSON.stringify(dedup)), [
  { id: 'agent-A', description: 'A last active', type: 'Explore', status: 'running', parentId: 'parent-A', spawnedBy: 'spawn-plugin' },
  { id: 'agent-B', description: 'B first-seen', type: 'Explore', status: 'running', name: 'B mapped name' },
  { id: 'agent-D', description: 'D last terminal', type: 'Explore', status: 'failed' },
]);
console.log('dedup_order_json', JSON.stringify(dedup));

// Identity/name/provenance/parent optionality on both task families.
const identities = await list([
  local('local-id', 'running', { parentAgentId: 'parent-id' }),
  teammate('teammate@team', 'running', {
    identity: { resumableAgentId: 'resumed-loop-id' },
    spawnedWith: { parentAgentId: 'parent-id', description: '' },
  }),
  local('local-null-parent', 'running', { parentAgentId: null }),
], {
  names: [['registered local name', 'local-id']],
  provenance: [['local-id', { hookCaller: 'plugin-caller' }], ['resumed-loop-id', { hookCaller: 'teammate-caller' }]],
});
assert.deepEqual(JSON.parse(JSON.stringify(identities)), [
  { id: 'local-id', description: 'local-id description', type: 'Explore', status: 'running', parentId: 'parent-id', spawnedBy: 'plugin-caller', name: 'registered local name' },
  { id: 'resumed-loop-id', teammateId: 'teammate@team', description: '', type: 'Explore', status: 'running', parentId: 'parent-id', spawnedBy: 'teammate-caller', name: 'Teammate' },
  { id: 'local-null-parent', description: 'local-null-parent description', type: 'Explore', status: 'running', parentId: null },
]);
console.log('identity_json', JSON.stringify(identities));

// Membership negative fact wins before PAt; only running teammates without a
// resumableAgentId are read from team storage. Owned running background Bash /
// monitor tasks then promote only idle rows to waiting.
const inactive = await list([
  teammate('worker@team', 'running'),
], { teams: [['team', { members: [{ agentId: 'worker@team', isActive: false }] }]] });
assert.equal(inactive[0].status, 'idle');
const inactiveWithBash = await list([
  teammate('worker@team', 'running'),
  { type: 'local_bash', status: 'running', isBackgrounded: true, agentId: 'worker@team' },
], { teams: [['team', { members: [{ agentId: 'worker@team', isActive: false }] }]] });
assert.equal(inactiveWithBash[0].status, 'waiting');
const inactiveWithForegroundBash = await list([
  teammate('worker@team', 'running'),
  { type: 'local_bash', status: 'running', isBackgrounded: false, agentId: 'worker@team' },
], { teams: [['team', { members: [{ agentId: 'worker@team', isActive: false }] }]] });
assert.equal(inactiveWithForegroundBash[0].status, 'idle');
const inactiveWithMonitor = await list([
  teammate('worker@team', 'running'),
  { type: 'monitor_mcp', status: 'running', agentId: 'worker@team' },
], { teams: [['team', { members: [{ agentId: 'worker@team', isActive: false }] }]] });
assert.equal(inactiveWithMonitor[0].status, 'waiting');
const resumableIgnoresMembership = await list([
  teammate('worker@team', 'running', { identity: { resumableAgentId: 'loop-id' } }),
], { teams: [['team', { members: [{ agentId: 'worker@team', isActive: false }] }]] });
assert.equal(resumableIgnoresMembership[0].status, 'running');
console.log('membership_background', JSON.stringify({
  inactive: inactive[0].status,
  inactiveBackgroundBash: inactiveWithBash[0].status,
  inactiveForegroundBash: inactiveWithForegroundBash[0].status,
  inactiveMonitor: inactiveWithMonitor[0].status,
  resumableInactive: resumableIgnoresMembership[0].status,
}));

// Exact two-phase agent.spawn hook return wrapper.
const calls = [];
let hostResponse = { result: { resolvedModel: 'resolved-model', agentId: 'loop-id', teammate_id: 'worker@team' } };
const api = context.rw('oracle-plugin', async (event, input) => { calls.push([event, input]); return hostResponse; });
const spawnResult = await api.spawn({
  prompt: 'do it', model: 'requested-model', name: 'worker', subagentType: 'Explore',
});
assert.deepEqual(JSON.parse(JSON.stringify(spawnResult)), {
  model: 'resolved-model', agentId: 'loop-id', teammateId: 'worker@team',
});
assert.deepEqual(JSON.parse(JSON.stringify(calls.at(-1))), [
  'agent.spawn', { tool: 'Agent', prompt: 'do it', description: 'do it', run_in_background: true, model: 'requested-model', subagent_type: 'Explore', name: 'worker' },
]);
hostResponse = { result: { resolvedModel: null, teammate_id: null, agentId: 9 } };
assert.deepEqual(JSON.parse(JSON.stringify(await api.spawn({ prompt: 'ok' }))), { model: 'inherit' });
assert.deepEqual(JSON.parse(JSON.stringify(await api.spawn({ prompt: 'alpha beta gamma delta epsilon zeta' }))), { model: 'inherit' });
assert.equal(calls.at(-1)[1].description, 'alpha beta gamma delta epsilon');
hostResponse = { result: { resolvedModel: null } };
assert.deepEqual(JSON.parse(JSON.stringify(await api.spawn({ prompt: 'ok', model: 'requested-model' }))), { model: 'requested-model' });
hostResponse = { result: { teammate_id: '', agentId: 'stable' } };
assert.deepEqual(JSON.parse(JSON.stringify(await api.spawn({ prompt: 'ok' }))), { model: 'inherit', agentId: 'stable', teammateId: '' });
hostResponse = { deny: 'denied' };
assert.deepEqual(JSON.parse(JSON.stringify(await api.spawn({ prompt: 'ok' }))), { deny: 'denied' });
hostResponse = { isError: true, text: 'host error' };
assert.deepEqual(JSON.parse(JSON.stringify(await api.spawn({ prompt: 'ok' }))), { deny: 'host error' });
 const spawnErrors = [];
 for (const input of [{ prompt: '  ' }, undefined]) {
   try { await api.spawn(input); } catch (error) { spawnErrors.push(error.message); }
 }
 assert.deepEqual(spawnErrors, [
   'oracle-plugin: $.agent.spawn takes { prompt, ... } (a non-empty prompt)',
   'oracle-plugin: $.agent.spawn takes { prompt, ... } (a non-empty prompt)',
 ]);
 console.log('spawn_errors', JSON.stringify(spawnErrors));
hostResponse = [{ id: 'a-id', description: 'task', type: 'Explore', status: 'running' }];
assert.deepEqual(JSON.parse(JSON.stringify(await api.list({ ignored: true }))), hostResponse);
assert.deepEqual(JSON.parse(JSON.stringify(calls.at(-1))), ['agent.list', {}]);
console.log('spawn_returns', JSON.stringify({ success: spawnResult, omitted: { model: 'inherit' }, emptyStringIsPresent: { model: 'inherit', agentId: 'stable', teammateId: '' }, deny: { deny: 'denied' } }));

// Native `agent.spawn` teammate bridge resolves the stable id from a task row:
// first nonterminal matching teammate wins; otherwise last matching row; if no
// row matches, the raw teammate address is the fallback agentId.
const oldTeam = teammate('worker@team', 'completed', { identity: { resumableAgentId: 'old-loop' } });
const liveTeam1 = teammate('worker@team', 'running', { identity: { resumableAgentId: 'live-loop-1' } });
const liveTeam2 = teammate('worker@team', 'running', { identity: { resumableAgentId: 'live-loop-2' } });
const bridgeContext = { taskRegistry: { all: () => ({ oldTeam, liveTeam1, liveTeam2 }) } };
context.Hu = () => ({ toolContext: () => bridgeContext });
const raw = { result: { status: 'teammate_spawned', teammate_id: 'worker@team', model: 'model-x' } };
const bridged = context.c_o(raw, 'oracle-plugin');
assert.equal(bridged.result.agentId, 'live-loop-1');
assert.equal(bridged.result.teammate_id, 'worker@team');
assert.equal(bridged.result.resolvedModel, 'model-x');
const fallback = context.c_o({ result: { status: 'teammate_spawned', teammate_id: 'orphan@team' } }, 'oracle-plugin');
assert.equal(fallback.result.agentId, 'orphan@team');
const terminalTeam2 = teammate('worker@team', 'failed', { identity: { resumableAgentId: 'last-terminal' } });
context.Hu = () => ({ toolContext: () => ({ taskRegistry: { all: () => ({ oldTeam, terminalTeam2 }) } }) });
const terminalOnly = context.c_o({ result: { status: 'teammate_spawned', teammate_id: 'worker@team' } }, 'oracle-plugin');
assert.equal(terminalOnly.result.agentId, 'last-terminal');
context.Hu = () => ({ toolContext: () => ({ taskRegistry: { all: () => ({ oldTeam }) } }) });
const lastTerminal = context.c_o({ result: { status: 'teammate_spawned', teammate_id: 'worker@team' } }, 'oracle-plugin');
assert.equal(lastTerminal.result.agentId, 'old-loop');
context.Hu = () => ({ toolContext: () => ({ taskRegistry: { all: () => ({}) } }) });
const noTeammateId = context.c_o({ result: { status: 'teammate_spawned', teammate_id: null } }, 'oracle-plugin');
assert.equal(noTeammateId.result.agentId, undefined);
const notTeammate = context.c_o({ result: { status: 'async_launched', agent_id: 'agent-x' } }, 'oracle-plugin');
assert.equal(notTeammate.result.agentId, undefined);
console.log('spawn_bridge', JSON.stringify({ firstLive: bridged.result, lastWhenOnlyTerminal: terminalOnly.result, rawIdFallback: fallback.result, nullTeammateIdIgnored: noTeammateId.result }));

// Provenance helper: only a defined hookCaller is retained; no caller removes
// prior identity state. hookOrigin defaults to [hookCaller], but list surfaces
// the scalar hookCaller, not the origin array.
const provenance = new Map();
context.dt = () => ({ spawnProvenance: provenance });
context.CMr('spawned-id', { hookCaller: 'plugin@marketplace' });
assert.deepEqual(JSON.parse(JSON.stringify(provenance.get('spawned-id'))), {
  hookOrigin: ['plugin@marketplace'], hookCaller: 'plugin@marketplace',
});
context.CMr('spawned-id', { hookCaller: undefined });
assert.equal(provenance.has('spawned-id'), false);
console.log('provenance', JSON.stringify({ stored: { hookOrigin: ['plugin@marketplace'], hookCaller: 'plugin@marketplace' }, cleared: true }));

// Missing session-owned listing context is an error, not an empty roster. The
// executable also embeds these runtime error strings for the two destructures.
context.DQn = () => ({ namedAgents: () => null });
const listErrors = [];
try { await context.ifo('oracle-plugin'); } catch (error) { listErrors.push(error.message); }
context.DQn = () => ({ namedAgents: () => ({ agentNameRegistry: new Map() }) });
context.dt = () => null;
try { await context.ifo('oracle-plugin'); } catch (error) { listErrors.push(error.message); }
assert.deepEqual(listErrors, [
  "Cannot destructure property 'agentNameRegistry' of 'DQn(...).namedAgents(...)' as it is null.",
  "Cannot destructure property 'spawnProvenance' of 'dt(...)' as it is null.",
]);
context.dt = () => ({ spawnProvenance: provenance });
console.log('missing_context_node_vm_errors', JSON.stringify(listErrors));
console.log('embedded_binary_error_resources', JSON.stringify(embeddedErrorResources.map(message => ({
  message,
  byte_offset: binary.indexOf(Buffer.from(message)),
}))));

// Preserve JavaScript undefined-vs-null spread semantics in list rows.
context.Hu = () => ({ toolContext: () => state.taskContext });
context.dt = () => ({ spawnProvenance: state.provenance });
context.DQn = () => ({ namedAgents: () => ({ agentNameRegistry: state.names }) });
const nullableRows = [
  teammate('ignored-address', 'running', {
    identity: { agentId: null, resumableAgentId: 'stable-null-address' },
    spawnedWith: { parentAgentId: null, description: null },
    task: { description: 'row fallback description' },
  }),
  teammate(undefined, 'running', {
    identity: { agentId: undefined, agentName: null, resumableAgentId: 'stable-missing-address' },
    spawnedWith: { parentAgentId: undefined, description: undefined },
  }),
];
const nullableFields = await list(nullableRows, {
  names: [['', 'stable-missing-address']],
  provenance: [['stable-null-address', { hookCaller: null }]],
});
assert.deepEqual(JSON.parse(JSON.stringify(nullableFields)), [
  { id: 'stable-null-address', teammateId: null, description: 'row fallback description', type: 'Explore', status: 'running', parentId: null, spawnedBy: null, name: 'Teammate' },
  { id: 'stable-missing-address', type: 'Explore', status: 'running', name: '' },
]);
console.log('nullable_optional_json', JSON.stringify(nullableFields));

// Terminal rows do not win after a nonterminal identity has appeared, even
// when that nonterminal row arrives after an earlier terminal row.
const terminalBeforeActive = await list([
  local('agent-C', 'completed', { description: 'old terminal' }),
  local('agent-C', 'paused', { description: 'paused nonterminal' }),
  local('agent-C', 'failed', { description: 'later terminal' }),
]);
assert.equal(terminalBeforeActive.length, 1);
assert.equal(terminalBeforeActive[0].description, 'paused nonterminal');
assert.equal(terminalBeforeActive[0].status, 'idle');
console.log('terminal_before_active_json', JSON.stringify(terminalBeforeActive));

// Team membership is an exact false override applied after PAt. Activity
// promotion applies after that, but only while the computed status is idle.
const membershipOverridesApproval = await list([
  teammate('worker@team', 'running', { task: { awaitingPlanApproval: true } }),
], { teams: [['team', { members: [{ agentId: 'worker@team', isActive: false }] }]] });
assert.equal(membershipOverridesApproval[0].status, 'idle');
const activeMembershipKeepsApproval = await list([
  teammate('worker@team', 'running', { task: { awaitingPlanApproval: true } }),
], { teams: [['team', { members: [{ agentId: 'worker@team', isActive: true }] }]] });
assert.equal(activeMembershipKeepsApproval[0].status, 'waiting');
const monitorDoesNotOverrideRunning = await list([
  teammate('worker@team', 'running'),
  { type: 'monitor_ws', status: 'running', agentId: 'worker@team' },
]);
assert.equal(monitorDoesNotOverrideRunning[0].status, 'running');
console.log('priority_edges', JSON.stringify({
  inactiveOverridesPlanApproval: membershipOverridesApproval[0].status,
  activePreservesPlanApproval: activeMembershipKeepsApproval[0].status,
  monitorOnlyPromotesIdle: monitorDoesNotOverrideRunning[0].status,
}));

const utf8ReturnCases = [
  JSON.stringify(dedup),
  JSON.stringify(nullableFields),
  JSON.stringify(spawnResult),
];
console.log('json_return_utf8_hex', JSON.stringify(utf8ReturnCases.map(value => ({
  json: value,
  bytes: Buffer.byteLength(value, 'utf8'),
  hex: Buffer.from(value, 'utf8').toString('hex'),
}))));

console.log('source_windows', JSON.stringify(sourceWindows));
console.log('PASS: extracted native helper source executed in VM; no provider/network calls');
