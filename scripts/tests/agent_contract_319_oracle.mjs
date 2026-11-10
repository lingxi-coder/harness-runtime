#!/usr/bin/env node
// Execute bounded Agent contract producers from the official native artifact.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import {dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: node agent_contract_319_oracle.mjs <official 2.1.319 binary> [--check]');
const binary = fs.readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const artifactSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(sha256(binary), artifactSha256, 'Use the pinned official darwin-arm64 artifact');
const sourceRanges = [];
function take(label, first, after, offset) {
  const start = binary.indexOf(first, offset);
  const endExclusive = binary.indexOf(after, start + first.length);
  assert(start >= offset && endExclusive > start && endExclusive - start < 20000, label);
  const source = binary.subarray(start, endExclusive);
  sourceRanges.push({label, start, endExclusive, sha256: sha256(source)});
  return source.toString('utf8');
}
const normalize = value => JSON.parse(JSON.stringify(value));

const resultWorld = vm.createContext({
  Rn: 'SendMessage', Ge: 'Read', Iee: () => false,
  // Unreachable for the captured successful result arms.
  m: () => { throw Error('Unexpected result status in oracle corpus'); },
});
vm.runInContext([
  `var ${take('one-shot native agent types', 'Ann=new Set(', ',cze=', 264050000)};`,
  `var ${take('shared-checkout warning', 'evi=', ',tvi=', 272814000)};`,
  `var mapper={${take('Agent result mapper', 'mapToolResultToToolResultBlockParam(n,e){', '});function kr(', 278389000)}};`,
].join('\n'), resultWorld, {timeout: 2000});
const mapResult = input => normalize(resultWorld.mapper.mapToolResultToToolResultBlockParam(input, 'tool-123'));
const completed = [];
const continuations = [['absent', {}], ['true', {canContinueAgent: true}], ['false', {canContinueAgent: false}]];
const contentCases = [
  ['empty', []],
  ['one', [{type: 'text', text: 'Inspection complete.'}]],
  ['multiple', [{type: 'text', text: 'First line'}, {type: 'text', text: ''}, {type: 'text', text: 'Last line\n<result>&'}]],
];
const worktrees = [
  ['none', {}],
  ['path-and-branch', {worktreePath: '/tmp/wt/agent-a1', worktreeBranch: 'agent/a1'}],
  ['path-only', {worktreePath: '/tmp/wt/agent-a1'}],
  ['empty-path', {worktreePath: '', worktreeBranch: 'agent/a1'}],
];
for (const agentType of ['general-purpose', 'Explore', 'Plan']) {
  for (const [continuation, canContinue] of continuations) {
    for (const [contentName, content] of contentCases) {
      for (const [worktreeName, worktree] of worktrees) {
        const input = {
          status: 'completed', agentId: 'agent-123', agentType, prompt: 'inspect',
          content, totalTokens: 1234, totalToolUseCount: 7, totalDurationMs: 4200,
          ...canContinue, ...worktree,
        };
        completed.push({name: `${agentType}-${continuation}-${contentName}-${worktreeName}`, input, output: mapResult(input)});
      }
    }
  }
}
const asyncCases = [];
for (const [continuation, canContinue] of continuations) {
  for (const [readName, canRead] of [['absent', {}], ['true', {canReadOutputFile: true}], ['false', {canReadOutputFile: false}]]) {
    for (const outputFile of ['', '/tmp/tasks/agent-123.output']) {
      const input = {
        status: 'async_launched', isAsync: true, agentId: 'agent-123', description: 'inspect', prompt: 'inspect',
        outputFile, ...canContinue, ...canRead,
      };
      asyncCases.push({name: `${continuation}-${readName}-${outputFile ? 'file' : 'empty-file'}`, input, output: mapResult(input)});
    }
  }
}
const rawInputs = [
  {name: 'completed-unpaired-surrogates', input: {status: 'completed', agentId: 'agent-123', agentType: 'general-purpose', totalTokens: 1, totalToolUseCount: 2, totalDurationMs: 3, canContinueAgent: false, contentUnits: [[65, 0xd800, 66], [0xdc00, 67]]}},
  {name: 'one-shot-unpaired-surrogates', input: {status: 'completed', agentId: 'agent-123', agentType: 'Explore', totalTokens: 1, totalToolUseCount: 2, totalDurationMs: 3, contentUnits: [[65, 0xd800, 66], [0xdc00, 67]]}},
];
const raw = rawInputs.map(({name, input}) => {
  const {contentUnits, ...rest} = input;
  const output = mapResult({...rest, content: contentUnits.map(units => ({type: 'text', text: String.fromCharCode(...units)}))});
  const text = output.content.map(block => block.text).join('\n');
  return {name, input, outputUnits: Array.from({length: text.length}, (_, index) => text.charCodeAt(index))};
});

const backgroundWorld = vm.createContext({
  Yn: () => undefined,
  Ek: agent => agent.__builtinWebFetch,
});
vm.runInContext(take('local background decision Gr', 'function Gr(n){', 'var Qn=', 278336000), backgroundWorld, {timeout: 2000});
const background = [];
const flags = ['definitionBackground', 'isBuiltinWebFetch', 'isCoordinator', 'forceAsync', 'callerIsInProcessTeammate', 'callerIsHeadlessSubagent', 'backgroundTasksDisabled'];
for (const wantsBackground of [undefined, false, true]) {
  for (let bits = 0; bits < 1 << flags.length; bits++) {
    const input = Object.fromEntries(flags.map((name, index) => [name, Boolean(bits & 1 << index)]));
    if (wantsBackground !== undefined) input.wantsBackground = wantsBackground;
    const native = {...input, agent: {background: input.definitionBackground, __builtinWebFetch: input.isBuiltinWebFetch}};
    background.push({input, expected: backgroundWorld.Gr(native).shouldRunAsync});
  }
}

// A metadata-only schema DSL records native .describe(), enum, optional, merge,
// extend and omit operations. It does not emulate validation or parsing.
function schema(type, extra = {}) {
  return {
    ...extra, type,
    optional() { return schema(type, {...this, optionalField: true}); },
    describe(description) { return schema(type, {...this, description}); },
    regex() { return this; },
    refine() { return this; },
    merge(other) { return schema(type, {shape: {...this.shape, ...other.shape}}); },
    extend(shape) { return schema(type, {shape: {...this.shape, ...shape}}); },
    omit(fields) { return schema(type, {shape: Object.fromEntries(Object.entries(this.shape).filter(([name]) => !fields[name]))}); },
  };
}
const metadataState = {fork: true, disabledBackground: false, effortOverride: undefined, modelForced: false, coordinator: false, relative: false};
const metadataWorld = vm.createContext({
  p: builder => builder,
  o: () => schema('string'), B: values => schema('string', {enum: [...values]}),
  I: () => schema('boolean'), u: shape => schema('object', {shape}),
  vZi: () => schema('string'),
  i7i: /^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/,
  Ag: 'main', oH: () => false,
  Rn: 'SendMessage',
  Da: () => metadataState.coordinator,
  Xge: () => metadataState.fork,
  wgt: () => metadataState.disabledBackground,
  yPt: () => metadataState.fork,
  nUn: () => true, Oje: () => undefined,
  bw: () => metadataState.effortOverride,
  i: {}, Ddt: () => metadataState.relative,
  D: value => value !== null && typeof value === 'object' && !Array.isArray(value),
});
vm.runInContext([
  `var ${take('native effort levels', '_i=["low"', ';function _tt(', 260114000)};`,
  `var ${take('agent input description constants', 'pVi=', ',oT=', 264050000)};`,
  `var ${take('Agent input schema and projections', 'ko=p(', 'var _o=', 278351000)}`,
  take('relative effort input coercion Pdr', 'function Pdr(e){', 'function pdr(', 270847000),
].join('\n'), metadataWorld, {timeout: 2000});
const metadata = [];
for (const fork of [false, true]) {
  for (const effortForced of [false, true]) {
    Object.assign(metadataState, {fork, effortOverride: effortForced ? 'high' : undefined});
    const native = metadataWorld.wjo();
    metadata.push({fork, effortForced, effort: native.shape.effort ? normalize(native.shape.effort) : null});
  }
}
const coercion = [];
for (const relativeEnabled of [false, true]) {
  metadataState.relative = relativeEnabled;
  for (const effort of [undefined, 'lower', 'higher', 'low', 'xhigh', 'max', 'invalid', null, 42]) {
    const input = {description: 'inspect', prompt: 'inspect', ...(effort !== undefined && {effort})};
    coercion.push({relativeEnabled, input, output: normalize(metadataWorld.Pdr(input))});
  }
}
const noteWorld = vm.createContext({Ann: resultWorld.Ann, Rn: 'SendMessage'});
vm.runInContext(`var ${take('max-turn note constants', 'cze=', ',$Ar=', 264050000)};`, noteWorld);
const noteSource = take('max-turn note producer', 'if(Re){let xe=Ann.has(O)||ne===!1?', 'if(j)ee.push(', 278214000);
const maxTurnsNotes = [];
for (const agentType of ['general-purpose', 'Explore', 'Plan']) for (const limit of [0, 3]) {
  for (const partial of [false, true]) for (const canContinue of [null, false, true]) {
    Object.assign(noteWorld, {Re:limit, O:agentType, Y:partial ? ['partial'] : [], ne:canContinue ?? undefined, ee:[]});
    vm.runInContext(noteSource, noteWorld, {timeout:1000});
    maxTurnsNotes.push({agentType, limit, partial, canContinue, output:noteWorld.ee[0]?.text ?? null});
  }
}
const fixture = {
  version: '2.1.319', artifactSha256, sourceRanges,
  boundary: 'Bounded native Agent result mapper, Gr local background decision and Pdr coercion execute in VMs. Imported isolation, tools, feature flags and note framing are controlled. Metadata DSL records schema-builder metadata only and does not establish validation equivalence. No bootstrap or provider executes. Remote results are excluded; provenance-on result framing is covered by the separate framing oracle.',
  completed, async: asyncCases, raw, background, effort: {metadata, coercion}, maxTurnsNotes,
};
const destination = fileURLToPath(new URL('../../crates/tools/agent/tests/fixtures/agent_contract_319.json', import.meta.url));
const serialized = JSON.stringify(fixture, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(fs.readFileSync(destination, 'utf8'), serialized, 'native Agent fixture drift');
} else {
  fs.mkdirSync(dirname(destination), {recursive: true});
  fs.writeFileSync(destination, serialized);
}
console.log(`Claude Code 2.1.319 Agent contract: ${completed.length} completed, ${asyncCases.length} async, ${raw.length} UTF-16, ${background.length} background and ${coercion.length} coercion and ${maxTurnsNotes.length} turn-limit cases ${process.argv.includes('--check') ? 'matched' : 'written'}`);
