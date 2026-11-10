#!/usr/bin/env node
// Execute the bounded native notification producer, XML escaping and framing.
// No bootstrap, provider calls, credentials, or filesystem task state execute.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: node agent_task_notifications_319_oracle.mjs <official 2.1.319 binary> [--check]');
const binary = fs.readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const artifactSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(sha256(binary), artifactSha256, 'Use the pinned official darwin-arm64 artifact');
const sourceRanges = [];
function take(label, start, after, offset) {
  const first = binary.indexOf(start, offset);
  const endExclusive = binary.indexOf(after, first + start.length);
  assert(first >= offset && endExclusive > first && endExclusive - first < 10000, label);
  const source = binary.subarray(first, endExclusive);
  sourceRanges.push({label, start: first, endExclusive, sha256: sha256(source)});
  return source.toString('utf8');
}

const world = vm.createContext({});
vm.runInContext([
  `var ${take('task identity tags', 'ju=', ';var N9=', 259546000)};`,
  take('task notification field tags', 'var rqe=', 'function FFr(', 259547000),
  `${take('agent status and worktree tags', 'var bcn=', ',UFr=', 259547000)};`,
  `var Rn='SendMessage',${take('continue hint', 'Byo=', ',FAr=', 264050000)};`,
  take('native XML escaping Yt', 'function Yt(u){', 'function k6e(', 261475000),
  take('native notification framing Gi', 'function Gi({taskId:', '\nexport{', 268693000),
  take('native agent notification U9t', 'function U9t({taskId:', 'function B9t(', 274322000),
].join('\n'), world, {timeout: 2000});

// These stubs supply declared host facts at the producer's imported boundary:
// a claimed terminal task, a main-session recipient, no artifact provenance,
// no queued messages, no pending owned work, and no usage-limit refusal.
vm.runInContext(`
  var emitted, task;
  var $p='__artifact_provenance__',bv='__remote_provenance__',DAr='__waiting_marker__';
  var V0t='next';
  var joe=()=>({claimed:true,task}),B9t=()=>undefined,kur=()=>false;
  var qCi=()=>false,KCi=()=>false,HYe=()=>false,st=()=>undefined,Pr=value=>value;
  var zh=body=>body(),sur=()=>undefined,Vhe=()=>true,gMe=()=>false,Rre=()=>false,t=()=>{};
  var bjt=()=>false,ll=value=>{emitted=value.value};
  function render(input){
    emitted=undefined;
    task={type:'local_agent',status:input.status,outputFile:'/tmp/tasks/a12345678.output'};
    U9t({taskId:'a12345678',description:'audit',...input,
      taskRegistry:{update:(_id,update)=>{task=update(task)}}});
    return emitted;
  }
`, world, {timeout: 2000});

const inputs = [
  ['completed', {}],
  ['empty-result', {finalMessage: ''}],
  ['whitespace-result', {finalMessage: ' '}],
  ['escaped-result', {description: 'audit <repo>&', finalMessage: 'Found <auth>& "quoted"'}],
  ['failed-error', {status: 'failed', error: 'rate <limit>&'}],
  ['failed-empty-error', {status: 'failed', error: ''}],
  ['stopped-with-error', {status: 'killed', error: 'session <closed>&'}],
  ['stopped-empty-error', {status: 'killed', error: ''}],
  ['stopped-system-error', {status: 'killed', killedBy: 'system', error: 'worker unavailable'}],
  ['stopped-parent-error', {status: 'killed', killedBy: 'parent', error: 'worker unavailable'}],
  ['stopped-user-error', {status: 'killed', killedBy: 'user', error: 'worker unavailable'}],
  ['max-turns', {maxTurnsReached: 12}],
  ['zero-max-turns', {maxTurnsReached: 0}],
  ['worktree-escaped-branch', {worktreePath: '/tmp/wt/agent-a1', worktreeBranch: 'agent/<safe>&'}],
  ['empty-worktree-path', {worktreePath: '', worktreeBranch: 'agent/a1'}],
  ['empty-worktree-branch', {worktreePath: '/tmp/wt/agent-a1', worktreeBranch: ''}],
  ['usage-and-tool-id', {toolUseId: 'tool-123', usage: {totalTokens: 1234, toolUses: 7, durationMs: 4200}}],
];
const cases = inputs.map(([name, extra]) => {
  const input = {status: 'completed', ...extra};
  const output = world.render(input);
  assert.equal(typeof output, 'string', name);
  return {name, input, output};
});
const rawInput = {status: 'completed', descriptionUnits: [0x41, 0xd800, 0x26], resultUnits: [0xdc00, 0x3c, 0x42]};
const rawOutput = world.render({
  status: rawInput.status,
  description: String.fromCharCode(...rawInput.descriptionUnits),
  finalMessage: String.fromCharCode(...rawInput.resultUnits),
});
const rawCases = [{
  name: 'unpaired-surrogates', input: rawInput,
  outputUnits: Array.from({length: rawOutput.length}, (_, index) => rawOutput.charCodeAt(index)),
}];
const fixture = {
  version: '2.1.319', artifactSha256, sourceRanges,
  boundary: 'Native U9t, Gi and Yt execute in a VM; imported task registry, recipient, scheduling and provenance facts are controlled. No application bootstrap or model executes.',
  cases, rawCases,
};
const destination = fileURLToPath(new URL('./fixtures/agent_task_notifications_319.json', import.meta.url));
const serialized = JSON.stringify(fixture, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(fs.readFileSync(destination, 'utf8'), serialized, 'native notification fixture drift');
} else {
  fs.writeFileSync(destination, serialized);
}
console.log(`Claude Code 2.1.319 agent notifications: ${cases.length} scalar and ${rawCases.length} UTF-16 cases ${process.argv.includes('--check') ? 'matched' : 'written'}`);
