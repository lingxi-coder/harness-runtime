#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import vm from 'node:vm';

const manifest = JSON.parse(fs.readFileSync(new URL('./fixtures/main_body_319_sources.json', import.meta.url), 'utf8'));
const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: node main_body_319_oracle.mjs <official 2.1.319 binary> [--check]');
const fd = fs.openSync(binaryPath, 'r');
const hash = createHash('sha256');
const chunk = Buffer.alloc(1024 * 1024);
for (let count; (count = fs.readSync(fd, chunk, 0, chunk.length, null)) > 0;) hash.update(chunk.subarray(0, count));
assert.equal(hash.digest('hex'), manifest.native_sha256, 'The upstream executable changed');
const sources = manifest.ranges.map(range => {
  const bytes = Buffer.alloc(range.endExclusive - range.start);
  assert.equal(fs.readSync(fd, bytes, 0, bytes.length, range.start), bytes.length);
  assert.equal(createHash('sha256').update(bytes).digest('hex'), range.sha256, range.name);
  return `${range.kind === 'declaration' ? 'var ' : ''}${bytes.toString('utf8')};`;
});
fs.closeSync(fd);
const context = vm.createContext({
  Me: 'Bash', Et: 'PowerShell', Ge: 'Read', ct: 'Edit', Xt: 'Write', Zr: 'Glob', Sr: 'Grep',
  p_: 'TaskCreate', nE: 'TodoWrite', ft: 'Agent', $r: 'Skill', MDt: 3, Vx: {agentType: 'Explore'},
  // Pasted-content, remote-client, REPL and server-supplied slots are not
  // advertised by these controlled local-host cases.
});
vm.runInContext(sources.join('\n'), context);
const api = vm.runInContext('({eCr,iCr,aCr,dCr,pCr,TCr,A_n,kvr,Svr,kCr,Avr,FCr,Ivr,zCr,ACr,xCr,PCr,P_n,qvr,Bvr,Wvr,Xvr,PWi,OWi,IWi,GWi,JWi,kWi,vWi,XWi,qWi,YWi})', context);
const tools = ['Agent', 'Bash', 'Read', 'Edit', 'Write', 'Glob', 'Grep', 'Skill', 'TaskCreate'];
const baseGuidance = {offersShellEscape: false, isFollowedInApp: false, agentGuidance: 'none', namesShellSearch: false, suggestsExploreAgent: false, skills: 'skill-tool', explainsUltrareview: false};
const parts = {
  intro: api.eCr('default'), intro_style: api.eCr('output-style'),
  system_tags: api.iCr(false, false), system_turns: api.iCr(true, false),
  doing_tasks: api.aCr(false, 'report the issue at https://github.com/anthropics/claude-code/issues'),
  doing_tasks_verified: api.aCr(true, 'report the issue at https://github.com/anthropics/claude-code/issues'),
  actions: api.dCr(), tools: api.pCr(new Set(tools), false, false), tools_embedded_search: api.pCr(new Set(tools), false, true), tone: api.TCr(),
  lean_tags: api.A_n({opening: 'default', explainsSystemTurns: false, namesPastedContentTag: false, leanRewrite: false}),
  lean_turns: api.A_n({opening: 'default', explainsSystemTurns: true, namesPastedContentTag: false, leanRewrite: false}),
  lean_style: api.A_n({opening: 'output-style', explainsSystemTurns: true, namesPastedContentTag: false, leanRewrite: false}),
  communication_standard: api.kvr('standard'), communication_readable: api.kvr('readable'),
  communication_final: api.kvr('readable-final-message'), communication_lean: api.kvr('none'),
  turn_updates: api.kvr('turn-updates'), action_caution: api.Svr(true, false), pronouns: api.Ivr,
  fable_identity_5: api.Avr('5'), fable_identity_5_1: api.Avr('5.1'),
  context_management: api.zCr, act_dont_rederive: api.ACr, delivering_work: api.xCr,
  corrections: api.PCr, reduced_delegation: api.P_n, original_games: api.qvr,
  early_stopping: api.Bvr, writing_for_user: api.Wvr, autonomy: api.Xvr(true),
  lean_rewrite: api.A_n({opening: 'default', explainsSystemTurns: true, namesPastedContentTag: false, leanRewrite: true}),
  lean_rewrite_style: api.A_n({opening: 'output-style', explainsSystemTurns: true, namesPastedContentTag: false, leanRewrite: true}),
  communication_rewrite: api.kvr('rewrite'), communication_rewrite_final: api.kvr('rewrite-final-message'),
  action_caution_rewrite: api.Svr(true, true), pronouns_rewrite: api.PWi,
  shell_escape: api.kCr({...baseGuidance, skills: 'none', offersShellEscape: true}, false),
  shell_escape_rewrite: api.OWi, skill_rewrite: api.IWi,
  context_management_rewrite: api.GWi, act_dont_rederive_rewrite: api.JWi,
  delivering_work_rewrite: api.kWi, corrections_rewrite: api.vWi,
  system_messages_turns: api.XWi(api.qWi), system_messages_tags: api.XWi(api.YWi),
};
const guidanceCases = [];
for (const agentGuidance of ['none', 'fork', 'fork-background', 'with-nudge', 'plain']) {
  for (const offersShellEscape of [false, true]) {
    const input = {...baseGuidance, agentGuidance, offersShellEscape, suggestsExploreAgent: agentGuidance === 'with-nudge'};
    guidanceCases.push({input, text: api.kCr(input, false)});
  }
}
const nativeTraits = JSON.parse(fs.readFileSync(new URL('../../crates/core/tests/fixtures/model_capabilities_2_1_319.json', import.meta.url), 'utf8')).promptTraits;
const bodyTools = tools.filter(name => name !== 'Agent');
const ignored = {isReplayed: true, text: null};
const observed = decision => ({isReplayed: false, decision});
const assemblies = nativeTraits.map(traits => {
  const session = Object.fromEntries(['communication', 'pronouns', 'actionCaution', 'continuesAgreedTasks', 'fableIdentity', 'hasJsonParameterRule', 'guidance', 'latestModels', 'backgroundJob', 'contextManagement', 'hasBrief', 'hasFocusMode', 'actsWithoutRederiving', 'deliversWork', 'hasCorrections', 'countersDelegation', 'reducesDelegation', 'hasOriginalGamesGuidance', 'served', 'servedForEffort', 'writesForUser', 'hasAutonomy'].map(name => [name, ignored]));
  Object.assign(session, {
    communication: observed(traits.turnUpdates ? 'turn-updates' : traits.fableMitigations ? 'readable-final-message' : traits.lean ? 'none' : 'standard'),
    pronouns: observed(null), actionCaution: observed(null),
    fableIdentity: observed(traits.model === 'claude-fable-5-1' ? '5.1' : traits.model.startsWith('claude-fable-') ? '5' : null),
    guidance: observed(baseGuidance), actsWithoutRederiving: observed(true),
    deliversWork: observed(traits.deliveringWork), hasCorrections: observed(traits.corrections),
    reducesDelegation: observed(traits.model === 'claude-opus-5'),
    hasOriginalGamesGuidance: observed(traits.originalGames),
    // The actual native dispatch below selects the baked early-stopping branch.
    served: observed(traits.earlyStopping ? {kind: 'early-stopping'} : {kind: 'none'}),
    writesForUser: observed(traits.writingForUser), hasAutonomy: observed(traits.fableMitigations),
  });
  const input = {kind: 'full', leanRewrite: false, opening: 'default', explainsSystemTurns: traits.systemTurns, namesPastedContentTag: false,
    separatesVerifiedFromAssumed: false, feedback: 'report the issue at https://github.com/anthropics/claude-code/issues',
    isReplMode: false, hasEmbeddedSearch: false, session, totalTokens: null, closesOnWhoPays: false};
  const facts = {model: traits.model, traits: traits.lean ? ['lean'] : [], tools: bodyTools, outputStyle: null};
  const sections = api.FCr(facts, input).filter(section => section.text !== null && section.text !== undefined && section.text !== '');
  return {model: traits.model, traits, tools: bodyTools, sections};
});
const rewriteAssemblies = assemblies.filter(value => ['claude-opus-5', 'claude-fable-5-1'].includes(value.model)).flatMap(value => [false, true].map(style => {
  const traits = value.traits;
  const session = Object.fromEntries(['communication', 'pronouns', 'actionCaution', 'continuesAgreedTasks', 'fableIdentity', 'hasJsonParameterRule', 'guidance', 'latestModels', 'backgroundJob', 'contextManagement', 'hasBrief', 'hasFocusMode', 'actsWithoutRederiving', 'deliversWork', 'hasCorrections', 'countersDelegation', 'reducesDelegation', 'hasOriginalGamesGuidance', 'served', 'servedForEffort', 'writesForUser', 'hasAutonomy'].map(name => [name, ignored]));
  Object.assign(session, {
    communication: observed(traits.fableMitigations ? 'rewrite-final-message' : 'rewrite'),
    pronouns: observed(null), actionCaution: observed(null),
    fableIdentity: observed(value.model === 'claude-fable-5-1' ? '5.1' : null),
    guidance: observed(baseGuidance), actsWithoutRederiving: observed(true),
    deliversWork: observed(traits.deliveringWork), hasCorrections: observed(traits.corrections),
    reducesDelegation: observed(value.model === 'claude-opus-5'), hasAutonomy: observed(traits.fableMitigations),
  });
  const input = {kind: 'full', leanRewrite: true, opening: style ? 'output-style' : 'default', explainsSystemTurns: traits.systemTurns, namesPastedContentTag: false,
    separatesVerifiedFromAssumed: false, feedback: 'report the issue at https://github.com/anthropics/claude-code/issues',
    isReplMode: false, hasEmbeddedSearch: false, session, totalTokens: null, closesOnWhoPays: false};
  const facts = {model: value.model, traits: ['lean'], tools: bodyTools, outputStyle: style ? {name: 'fixture', isKeepingCodingInstructions: true} : null};
  const sections = api.FCr(facts, input).filter(section => section.text !== null && section.text !== undefined && section.text !== '');
  return {model: value.model, traits, style, tools: bodyTools, sections};
}));
const output = {version: manifest.version, native_sha256: manifest.native_sha256, sourceByteRanges: manifest.ranges, parts, guidanceCases, assemblies, rewriteAssemblies,
  scope: 'Actual 2.1.319 local-host non-rewrite prompt renderers and controlled guidance combinations. Environment, memory, background jobs and dynamically served/cohort features have separate oracles. Names and brand normalization are performed only by the Rust comparison.'};
const target = new URL('../../crates/orchestrator/tests/fixtures/claude_2_1_319_main_body.json', import.meta.url);
const encoded = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(target, 'utf8'), encoded);
else fs.writeFileSync(target, encoded);
console.log(`${process.argv.includes('--check') ? 'checked' : 'wrote'} ${Object.keys(parts).length} native body fragments and ${guidanceCases.length} guidance cases`);
