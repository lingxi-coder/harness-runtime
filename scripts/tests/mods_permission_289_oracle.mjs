#!/usr/bin/env node
// Hash-pinned 2.1.289 Mod permission oracle. Native binaries are inputs only;
// extracted source and generated fixture stay out of the repository.
// Usage: node scripts/tests/mods_permission_289_oracle.mjs <2.1.288-binary> <2.1.289-binary> [--check]
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const args = process.argv.slice(2);
const check = args.includes('--check');
const binaries = args.filter((arg) => arg !== '--check');
assert.equal(binaries.length, 2, 'usage: node scripts/tests/mods_permission_289_oracle.mjs <2.1.288-binary> <2.1.289-binary> [--check]');

const sha256 = (value) => createHash('sha256').update(value).digest('hex');
const nativeSpecs = [
  {version: '2.1.288', path: binaries[0], sha256: 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750'},
  {version: '2.1.289', path: binaries[1], sha256: '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69'},
];
const binariesByVersion = new Map(nativeSpecs.map((spec) => {
  const bytes = readFileSync(spec.path);
  assert.equal(sha256(bytes), spec.sha256, `unexpected Claude Code ${spec.version} SHA-256`);
  return [spec.version, bytes];
}));
const sourceByteRanges = [];

function take(version, label, start, end, expectedHash, startText, endText) {
  const binary = binariesByVersion.get(version);
  assert(start >= 0 && end > start, `${label}: invalid pinned byte range`);
  if (startText) assert.equal(binary.subarray(start, start + Buffer.byteLength(startText)).toString(), startText, `${label}: start marker moved`);
  if (endText) assert.equal(binary.subarray(end, end + Buffer.byteLength(endText)).toString(), endText, `${label}: end marker moved`);
  const source = binary.subarray(start, end);
  const digest = sha256(source);
  assert.equal(digest, expectedHash, `${label}: source range hash changed`);
  sourceByteRanges.push({version, label, start, end, sha256: digest});
  return source.toString('utf8');
}

const oldExtractor = take(
  '2.1.288', 'Vrt/Yrt direct-only subcommand rule lookup', 187327473, 187327796,
  '85fea4ee43fd9dfcbcbc1f55223cdab35f98e328f1d91be45673c6d4f401b203',
  'function Vrt(e,n){', 'function Xrt(e){',
);
const extractor289 = take(
  '2.1.289', 'wEe/Ast recursive subcommand rule lookup', 187628314, 187628661,
  'e1613f5a8ad7e8c0b33cee74af4b6a1646cc5a52c09ace9b8e511c8222e47191',
  'function wEe(e,n){', 'function Pst(e,n){',
);
const producer289 = take(
  '2.1.289', 'm6o compound Bash subcommand-result producer', 189692731, 189695182,
  '782f6076975014582b28413b34b6c6e91d9520cebd4353080b3dbe8f611017a2',
  'async function m6o(e,n,r,s,g,h,b,w){', 'async function g6o(',
);
const coreProjection289 = take(
  '2.1.289', 'Pst core decision projection', 187628661, 187628952,
  '1087d88f782be664074cd9229366e8c69bff407434f4d3fd3de82bf20f5e0a1d',
  'function Pst(e,n){', 'function Ost(e,n){',
);
const coreReasonAndMetadata289 = take(
  '2.1.289', 'Rst/rQ/Cst/xst tool.check core fields', 187627964, 187628314,
  '66d4d7970ef6499020d845496442e0f70b111ac52321c1b8352453601c3d1a24',
  'function Rst(e,n){', 'function wEe(e,n){',
);
const coreMst289 = take(
  '2.1.289', 'Mst core hook wrapper', 187628802, 187628858,
  '6520a71b656d6e2f9a9a75addba8b5038685c45a2e3dbf9abecc293cc47c29ca',
  'var Mst=(e)=>({name:"core",isCore:!0,budgetMs:0,run:e});', 'function Ist(',
);
const coreIst289 = take(
  '2.1.289', 'Ist hook decision reason', 187628858, 187628952,
  'b7e2b075009aca9fc4fa2a189f3a593fe5e58b1996a3cf4bd57d779ddb9c768c',
  'function Ist(e,n){', 'function Ost(e,n){',
);
const coreRst289 = take(
  '2.1.289', 'Ost same-behavior preservation and message projection', 187628952, 187629060,
  '78b22a5e2c89a4e8de2378af5b19be0490995a253e62718c443d645b62abb429',
  'function Ost(e,n){', 'function ELn(',
);
const coreELn289 = take(
  '2.1.289', 'ELn hook/core decision arbitration', 187629060, 187629730,
  'cd62acd92a1bde567e84ba97fb801bf85b51df59edf47e73d1004931d58d07eb',
  'function ELn({result:e,decided:n,tool:r,input:s,toolUseID:g,hooked:h}){', 'function Dst(',
);
const coreDst289 = take(
  '2.1.289', 'Dst retained core result', 187629730, 187629818,
  '5cd6ee7c77e515375a8b54315cfb6fd696a4e4af9a7291b0e12facb0c1f931bc',
  'function Dst(e,n){', 'var ALn=',
);
const mainToolCheck289 = take(
  '2.1.289', '$Ln tool.check decision caller', 187630030, 187630770,
  'c6f5e6c8bf158ed514906506da208fad09e6254dd6d5e6e0048f3652dd9d0167',
  '$Ln=({tool:e,input:n,context:r,toolUseID:s,evaluate:g})=>', 'FLn($Ln);',
);
const webFetchConstant289 = take(
  '2.1.289', '_r WebFetch tool name', 183190392, 183190459,
  'f73e84506163c032831f0b853bd2ed0ba92b5a10880827926f57f0c517a69f5e',
  'var _r="WebFetch",Rwn="url";', 'export{_r',
);
const webSearchConstant289 = take(
  '2.1.289', 'Pm WebSearch tool name', 183304712, 183304731,
  '191c8bb713b7a0e2b2a27fef9c3c7d2003e35eb736a850a6e86d0244ad06c5d9',
  'var Pm="WebSearch";', 'var bt=',
);
const projectsUoe289 = take(
  '2.1.289', 'Uoe Projects-session WebFetch/WebSearch predicate', 183805687, 183805858,
  '7507090d046e83385390644695e89b695b66ec7b919048cc1dda2e69e98fa3cb',
  'function Uoe(e){', 'function Yn(e){',
);
const ruleFormatterL289 = take(
  '2.1.289', 'permission rule display escaping helper', 179185572, 179185663,
  '62865469619d8f4a2c2cb5938e8d8716cea1a498098491787975accccf34d4d4',
  'function l(e){', 'function ive(',
);
const ruleFormatterPn289 = take(
  '2.1.289', 'permission rule display formatter', 179186704, 179186806,
  '072153e9efee2a236ba56a1b9112c379858dd0702ffdbf15cdcd4f04e0c6c04b',
  'function Pn(e){', 'var Hme=',
);
const securityDefault289 = take(
  '2.1.289', 'sec-default managed Mods tool.check consumer', 213011071, 213014666,
  '8d1bc3c4ea247bff06104cccf6389a242e626db59f26ba1105954f683838a2ae',
  'var h=(e,r)=>', 'var ge=()=>uS',
);

function serialize(value) {
  if (Object.prototype.toString.call(value) === '[object Map]') return {entries: [...value.entries()].map(([key, item]) => [key, serialize(item)])};
  if (Array.isArray(value)) return value.map(serialize);
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, serialize(item)]));
  return value;
}

function makeExtractor(source, name) {
  const context = vm.createContext({});
  vm.runInContext(`${source}\nthis.extract=(decision)=>${name}(decision);`, context);
  return {context, extract: (decision) => context.extract(decision)};
}
const old = makeExtractor(oldExtractor, 'Yrt');
const current = makeExtractor(extractor289, 'Ast');
const rule = {type: 'rule', rule: {ruleValue: {toolName: 'Bash', ruleContent: 'rm:*'}, source: 'managed'}};
const nested1 = {type: 'subcommandResults', reasons: new Map([['child', {behavior: 'ask', decisionReason: rule}]])};
const nested2 = {type: 'subcommandResults', reasons: new Map([['middle', {behavior: 'ask', decisionReason: nested1}]])};
const nested3 = {type: 'subcommandResults', reasons: new Map([['outer', {behavior: 'deny', decisionReason: nested2}]])};
const cases = [];
function recordCase(id, behavior, decisionReason, matchedAskRule = undefined) {
  const decision = {behavior, decisionReason, ...(matchedAskRule === undefined ? {} : {matchedAskRule})};
  const prior = old.extract(decision);
  const upgraded = current.extract(decision);
  cases.push({id, input: serialize(decision), expected288: serialize(prior), expected289: serialize(upgraded)});
  return {decision, upgraded};
}
recordCase('direct-ask-rule', 'ask', rule);
recordCase('one-subcommand-level-ask-rule', 'ask', nested1);
recordCase('two-subcommand-level-ask-rule', 'ask', nested2);
recordCase('three-subcommand-level-deny-rule', 'deny', nested3);
recordCase('mismatched-child-behavior', 'ask', {type: 'subcommandResults', reasons: new Map([['child', {behavior: 'deny', decisionReason: rule}]])});
recordCase('first-matching-rule-wins', 'ask', {type: 'subcommandResults', reasons: new Map([
  ['first', {behavior: 'ask', decisionReason: {type: 'rule', rule: {ruleValue: {toolName: 'Bash', ruleContent: 'first:*'}, source: 'managed'}}}],
  ['second', {behavior: 'ask', decisionReason: {type: 'rule', rule: {ruleValue: {toolName: 'Bash', ruleContent: 'second:*'}, source: 'managed'}}}],
])});
recordCase('root-rule-precedes-subcommands', 'ask', rule, {ruleValue: {toolName: 'Bash', ruleContent: 'matched-ask:*'}, source: 'managed'});
recordCase('matched-ask-rule-fallback', 'ask', {type: 'other', reason: 'ask'}, {ruleValue: {toolName: 'Bash', ruleContent: 'fallback:*'}, source: 'managed'});
recordCase('deny-does-not-use-matched-ask-rule', 'deny', {type: 'other', reason: 'deny'}, {ruleValue: {toolName: 'Bash', ruleContent: 'must-not-appear:*'}, source: 'managed'});
recordCase('allow-direct-rule', 'allow', rule);
recordCase('allow-without-rule', 'allow', {type: 'other', reason: 'allowed'});
recordCase('ask-without-rule', 'ask', {type: 'other', reason: 'ask'});

const producerContext = vm.createContext({
  Ed: (command) => [command],
  Oc: () => 'Native compound Bash decision',
  aL: () => undefined,
  li: {name: 'Bash'},
});
vm.runInContext(`${producer289}\nthis.produce=m6o;`, producerContext);
const childRule = {type: 'rule', rule: {ruleValue: {toolName: 'Bash', ruleContent: 'rm:*'}, source: 'managed'}};
const behavior = (name) => ({
  isNormalizedCdCommand: () => false,
  isNormalizedGitCommand: () => false,
});
const compoundAsk = await producerContext.produce(
  {command: 'rm -rf /tmp/target'}, ['rm -rf /tmp/target'], [],
  async () => ({behavior: 'ask', message: 'Native child ask', decisionReason: childRule}),
  behavior(), undefined, undefined, undefined,
);
const compoundDeny = await producerContext.produce(
  {command: 'rm -rf /tmp/target'}, ['rm -rf /tmp/target'], [],
  async () => ({behavior: 'deny', message: 'Native child deny', decisionReason: childRule}),
  behavior(), undefined, undefined, undefined,
);

const ruleDisplayContext = vm.createContext({});
vm.runInContext(`${ruleFormatterL289}\n${ruleFormatterPn289}\nthis.formatRule=Pn;`, ruleDisplayContext);
const coreContext = vm.createContext({
  Pn: ruleDisplayContext.formatRule,
  rQ: 'tool.check',
});
vm.runInContext(`${coreReasonAndMetadata289}\n${extractor289}\n${coreProjection289}\nthis.project=Pst;`, coreContext);
const coreAsk = coreContext.project(compoundAsk);
const coreDeny = coreContext.project(compoundDeny);
assert.deepEqual(serialize(coreAsk), {decision: 'ask', reason: 'Native compound Bash decision', rule: 'Bash(rm:*)'});
assert.deepEqual(serialize(coreDeny), {decision: 'deny', reason: 'Native child deny', rule: 'Bash(rm:*)'});

function makeProjectGate({projectsSession = false, explicitProjectState = false, policyValue = false, policySource = 'env', eligibleByUse = false} = {}) {
  const state = {policyValue, policySource, eligibleByUse};
  const context = vm.createContext({
    a: {CLAUDE_CODE_PROJECTS_SESSION: projectsSession},
    el: {value: explicitProjectState},
    Is: (value) => value,
    Sbt: () => false,
    SS: () => ({}),
    Tf: 'bot_protected_tengu_atomic_magpie',
    io: () => ({value: state.policyValue, source: state.policySource}),
    Use: () => state.eligibleByUse,
  });
  vm.runInContext(`${webFetchConstant289}\n${webSearchConstant289}\n${projectsUoe289}\nthis.eligible=Uoe;`, context);
  return {state, eligible: (tool) => context.eligible(tool)};
}
const projectGateCases = [];
for (const spec of [
  {id: 'Bash-is-not-project-eligible', tool: 'Bash', projectsSession: true, expected: false},
  {id: 'WebFetch-outside-project-session', tool: 'WebFetch', expected: false},
  {id: 'WebFetch-project-session', tool: 'WebFetch', projectsSession: true, expected: true},
  {id: 'WebSearch-project-session', tool: 'WebSearch', projectsSession: true, expected: true},
  {id: 'explicit-project-state', tool: 'WebFetch', explicitProjectState: true, expected: true},
  {id: 'payload-disabled-unless-eligible', tool: 'WebSearch', projectsSession: true, policyValue: true, policySource: 'payload', expected: false},
  {id: 'payload-eligible', tool: 'WebSearch', projectsSession: true, policyValue: true, policySource: 'payload', eligibleByUse: true, expected: true},
]) {
  const gate = makeProjectGate(spec);
  const actual = gate.eligible(spec.tool);
  assert.equal(actual, spec.expected, spec.id);
  projectGateCases.push({id: spec.id, tool: spec.tool, expected: actual});
}

function makeMainToolCheck({toolName, projectsSession = false, modDecision, permissionDecision, callCoreInsideMod = false, policyValue = false, policySource = 'env', eligibleByUse = false}) {
  const state = {permissionDecision, modDecision, callCoreInsideMod, coreRuns: 0};
  const context = vm.createContext({
    Pn: ruleDisplayContext.formatRule,
    rQ: 'tool.check',
    a: {CLAUDE_CODE_PROJECTS_SESSION: projectsSession},
    el: {value: false},
    Is: (value) => value,
    Sbt: () => false,
    SS: () => ({}),
    Tf: 'bot_protected_tengu_atomic_magpie',
    io: () => ({value: policyValue, source: policySource}),
    Use: () => eligibleByUse,
    Cc: () => true,
    Uoe: undefined,
    t: () => undefined,
    p8e: () => ['user-mod'],
    ya: (_event, envelope) => async () => {
      if (state.callCoreInsideMod) await envelope.core['tool.check'].run();
      return {decision: state.modDecision};
    },
  });
  vm.runInContext(`${webFetchConstant289}\n${webSearchConstant289}\n${projectsUoe289}\n${coreReasonAndMetadata289}\n${extractor289}\n${coreProjection289}\n${coreMst289}\n${coreIst289}\n${coreRst289}\n${coreELn289}\n${coreDst289}\n${mainToolCheck289}\nthis.invoke=$Ln;`, context);
  const evaluate = async () => { state.coreRuns++; return state.permissionDecision; };
  return {
    state,
    invoke: async () => context.invoke({
      tool: {name: toolName}, input: {command: 'fixture'}, toolUseID: 'fixture-tool-use',
      context: {abortController: {signal: undefined}, hookOrigin: undefined}, evaluate,
    }),
  };
}

const plainAsk = {behavior: 'ask', message: 'Native core ask', decisionReason: childRule};
const plainDeny = {behavior: 'deny', message: 'Native core deny', decisionReason: childRule};
const webFetchRule = {type: 'rule', rule: {ruleValue: {toolName: 'WebFetch', ruleContent: 'https://*'}, source: 'managed'}};
const webFetchAsk = {behavior: 'ask', message: 'Native WebFetch rule ask', decisionReason: webFetchRule};
const webFetchDeny = {behavior: 'deny', message: 'Native WebFetch rule deny', decisionReason: webFetchRule};
const plainAllow = {behavior: 'allow', updatedInput: {command: 'fixture'}, decisionReason: {type: 'other', reason: 'native allow'}};
const mainArbitrationCases = [];
async function runMainCase(spec) {
  const test = makeMainToolCheck(spec);
  const actual = serialize(await test.invoke());
  const record = {id: spec.id, tool: spec.toolName, projectsSession: spec.projectsSession ?? false, modDecision: spec.modDecision, callCoreInsideMod: spec.callCoreInsideMod ?? false, coreRuns: test.state.coreRuns, expected: actual};
  if (spec.wantDecision !== undefined) assert.equal(actual?.behavior, spec.wantDecision, spec.id);
  if (spec.wantCoreRuns !== undefined) assert.equal(test.state.coreRuns, spec.wantCoreRuns, spec.id);
  mainArbitrationCases.push(record);
}
await runMainCase({id: 'Projects-WebFetch-Mod-allow-core-ask-is-retained', toolName: 'WebFetch', projectsSession: true, modDecision: 'allow', permissionDecision: webFetchAsk, wantDecision: 'ask', wantCoreRuns: 1});
await runMainCase({id: 'Projects-WebFetch-Mod-allow-core-deny-is-retained', toolName: 'WebFetch', projectsSession: true, modDecision: 'allow', permissionDecision: webFetchDeny, wantDecision: 'deny', wantCoreRuns: 1});
await runMainCase({id: 'Projects-WebFetch-Mod-allow-core-allow-is-retained', toolName: 'WebFetch', projectsSession: true, modDecision: 'allow', permissionDecision: plainAllow, wantDecision: 'allow', wantCoreRuns: 1});
await runMainCase({id: 'Projects-WebFetch-same-ask-keeps-core-reason', toolName: 'WebFetch', projectsSession: true, modDecision: 'ask', permissionDecision: webFetchAsk, callCoreInsideMod: true, wantDecision: 'ask', wantCoreRuns: 1});
await runMainCase({id: 'WebFetch-without-project-gate-does-not-force-core', toolName: 'WebFetch', modDecision: 'allow', permissionDecision: plainAsk, wantDecision: 'allow', wantCoreRuns: 0});
await runMainCase({id: 'Bash-Mod-allow-does-not-force-core', toolName: 'Bash', modDecision: 'allow', permissionDecision: plainAsk, wantDecision: 'allow', wantCoreRuns: 0});
await runMainCase({id: 'Bash-already-evaluated-core-ask-versus-Mod-allow', toolName: 'Bash', modDecision: 'allow', permissionDecision: plainAsk, callCoreInsideMod: true, wantDecision: 'allow', wantCoreRuns: 1});
await runMainCase({id: 'Bash-same-ask-behavior-keeps-core-decision', toolName: 'Bash', modDecision: 'ask', permissionDecision: plainAsk, callCoreInsideMod: true, wantDecision: 'ask', wantCoreRuns: 1});

const secDefaultSource = vm.createContext({});
vm.runInContext(`${securityDefault289}\nthis.registerSecDefault=ue;`, secDefaultSource);
function captureSecDefaultHandler() {
  let handler;
  const register = (event, ...args) => {
    if (event === 'tool.check') {
      for (let i = args.length - 1; i >= 0; i--) if (typeof args[i] === 'function') { handler = args[i]; break; }
    }
    return {catch() { return this; }};
  };
  secDefaultSource.registerSecDefault(register);
  assert.equal(typeof handler, 'function', 'sec-default tool.check registration');
  return handler;
}
const userAllow = {decision: 'allow'};
const trace = [{tier: 'user', plugin: 'user-mod', returned: userAllow}];
const nestedDenyProjection = {decision: coreDeny.decision, rule: coreDeny.rule};
const nestedAskProjection = {decision: coreAsk.decision, rule: coreAsk.rule};
const noRuleDenyProjection = {decision: 'deny'};
const securityCases = [];
async function runSecurityCase(id, appendResult, {setting = false, readFailure = false, expected} = {}) {
  const handler = captureSecDefaultHandler();
  const plugin = {
    settings: {read: async () => {
      if (readFailure) throw new Error('fixture settings read failure');
      return {pluginConfigs: {'cc-plugin-sec-default@builtin': {options: {allowModsToOverrideDenyRules: setting}}}};
    }},
    ui: {log: () => undefined},
  };
  const next = async () => userAllow;
  next.trace = trace;
  next.to = async () => appendResult;
  const actual = serialize(await handler(plugin, {tool: 'Bash'}, next));
  assert.deepEqual(actual, expected, id);
  securityCases.push({id, appendedDecision: appendResult.decision, appendedRule: appendResult.rule, allowModsToOverrideDenyRules: setting, readFailure, expected: actual});
}
await runSecurityCase('managed-nested-deny-rule-is-held', nestedDenyProjection, {expected: nestedDenyProjection});
await runSecurityCase('managed-nested-ask-rule-does-not-trigger-deny-hold', nestedAskProjection, {expected: userAllow});
await runSecurityCase('managed-deny-without-rule-does-not-trigger-rule-hold', noRuleDenyProjection, {expected: userAllow});
await runSecurityCase('explicit-allowMods-override-option-skips-hold', nestedDenyProjection, {setting: true, expected: userAllow});
await runSecurityCase('settings-read-failure-fails-closed-for-deny-rule', nestedDenyProjection, {readFailure: true, expected: nestedDenyProjection});

const output = {
  versions: Object.fromEntries(nativeSpecs.map(({version, sha256: digest}) => [version, digest])),
  scope: 'Actual pinned rule helpers, compound Bash m6o producer, Pst core projection, $Ln/ELn permission arbitration and sec-default ue managed deny consumer. Parser callbacks, plugin aggregate decisions, config/session state, settings and filesystem are explicit injections; this is not a full installed-Mod or permission-pipeline launch.',
  sourceByteRanges,
  nestedRuleCases: cases,
  nativeCompoundProducer: {
    childInput: 'Bash(rm:*)',
    ask: {behavior: compoundAsk.behavior, decisionReason: serialize(compoundAsk.decisionReason), coreProjection: serialize(coreAsk)},
    deny: {behavior: compoundDeny.behavior, decisionReason: serialize(compoundDeny.decisionReason), coreProjection: serialize(coreDeny)},
  },
  projectsEligibilityCases: projectGateCases,
  mainToolCheckArbitration: mainArbitrationCases,
  secDefaultManagedConsumer: securityCases,
  boundaries: {
    proven: [
      'The native compound Bash producer packages matched child results under decisionReason.type=subcommandResults.',
      '2.1.289 Ast/wEe recursively extracts a rule from matching-behavior nested subcommandResults; 2.1.288 Yrt/Vrt stops after the direct child result.',
      'Pst projects a returned rule through Pn(ruleValue), preserving decision and message-derived reason.',
      'The $Ln force-evaluate/core-retention branch is only eligible for WebFetch or WebSearch in the Projects/session gate (plus the source-specific explicit/payload gate).',
      'The exact sec-default ue consumer only replaces a prior non-deny result with an appended deny that has a defined rule; ask+rule does not satisfy its isRuleDeny predicate.',
    ],
    open: [
      'The complete installed user-Mod lifecycle and real permission callback/config acquisition are not driven by this callback-injected oracle.',
      'This evidence does not establish a universal managed Bash ask-over-Mod-allow hold. In a callback-injected $Ln case where a Bash core ask has already been evaluated and the aggregate Mod result is allow, Bash is outside Uoe and the helper returns allow. The complete ya/plugin-runner result source remains outside the fixture.',
    ],
  },
};
const json = JSON.stringify(output, null, 2) + '\n';
const fixture = new URL('../../crates/permission/tests/fixtures/mods_nested_rule_2_1_289.json', import.meta.url);
if (check) assert.equal(readFileSync(fixture, 'utf8'), json, '289 permission oracle fixture differs');
else writeFileSync(fixture, json);
console.log(`${check ? 'checked' : 'wrote'} 2.1.289 Mods permission oracle: ${cases.length} nested-rule cases, ${projectGateCases.length} Projects gates, ${mainArbitrationCases.length} tool.check cases, ${securityCases.length} managed-consumer cases`);
