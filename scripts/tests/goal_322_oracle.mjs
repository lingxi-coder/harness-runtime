#!/usr/bin/env bun
// Replay bounded, digest-checked Goal helpers from the shipped 2.1.322 binary.
// Native source is never copied into this repository or executed as an app.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import vm from 'node:vm';

if (typeof Bun === 'undefined') {
  const child = spawnSync(path.join(homedir(), '.bun/bin/bun'), [fileURLToPath(import.meta.url), ...process.argv.slice(2)], { stdio: 'inherit' });
  if (child.error) throw child.error;
  process.exit(child.status ?? 1);
}

const args = process.argv.slice(2);
assert(args.every(arg => !arg.startsWith('--') || arg === '--check'));
assert(args.filter(arg => !arg.startsWith('--')).length <= 1);
const binaryPath = args.find(arg => !arg.startsWith('--'))
  ?? path.join(homedir(), '.local/share/claude/versions/2.1.322');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
const nativeSha256 = sha256(binary);
assert.equal(nativeSha256, 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450');
const sourceByteRanges = [];
function take(name, start, end, digest) {
  const bytes = binary.subarray(start, end);
  assert(bytes.length > 0 && bytes.length < 10_000, `${name}: bounded source range`);
  assert.equal(sha256(bytes), digest, `${name}: exact source digest`);
  sourceByteRanges.push({ name, start, end, sha256: digest });
  return bytes.toString('utf8');
}

const source = {
  surrogates: take('well-formed UTF-16 filtering', 260462323, 260462884, 'b185fd1e8a6849ece80201f3443addd5daa90bf0363928bfd4b23b87ff43110e'),
  invisible: take('format, private-use and unassigned filtering', 260481569, 260481882, '56aee5cf75a1768566489bf8a86378ea497f601c7df0b595495446dd21f150c5'),
  width: take('native Bun display width', 261246167, 261246239, 'fd4d14f7bc144cbd387e86b39d2b92fcc8e78b35e0525ae72655d23995757a5a'),
  tabs: take('native eight-column tab expansion', 261907332, 261908631, '9b1b2df897d24f0597dc0f0b6d10bcfdff20b531e93eb0cfef35ece4dad93cd4'),
  controls: take('terminal control predicates', 261932754, 261932974, '857e711ae0c2e17769f228f4204f6bc1cee7402bab7a07e154f5d92a865f7833'),
  terminal: take('terminal-safe text and default-ignorable filtering', 261934897, 261935102, '6c58a9d1582360e6051b057557a35a35746d6bb423a2260cc17f462f872441a8'),
  canonical: take('proposal display canonicalization', 261935511, 261935551, '4c257eb43e078e35672a6a8957c21a11dc8bb8e48f33660f8faa805ffc25c9fe'),
  singleline: take('proposal single-line display', 295391641, 295391702, '994b9b951938dff843f35c062b7145ae9a26a9114260e62cbd974945fc9443f5'),
  state: take('goal state, gates, directive and transcript helpers', 265723694, 265727475, 'd141fea6b0d02b617e18c7038c9c0e039ce8b22653c27829475730461d727e14'),
  command: take('noninteractive goal command', 304740195, 304741029, 'bbfe3ab37bd66f961448271cabe197facc4815c0740bed39e600a7ed8d294d9f'),
  proposalPrompt: take('proposal prompt and description', 269967551, 269969141, '2ae5f01d16c0403a2049bf742733134278a65feea03672b8fb0ff289d86ecfe7'),
  proposal: take('proposal schemas, guards, decisions and result mapping', 295448013, 295453252, '413d411188d6e66702185ad9da1d57b06fc90ba0be01ee77d5abf3547899d58a'),
  flag: take('proposal admission feature flag', 287227110, 287227159, 'bf34e5a121adce3ccacb34642b40191dc838f1f592ed2b1ac248c8748caa34e7'),
  settings: take('trusted proposal setting and persistent-presence guard', 262192013, 262192229, 'ade92f68434231b35cf757c51f1706dcfbe49d50f11cf09395a223ad9e6c6bd7'),
  plural: take('iteration pluralization', 260461274, 260461315, '45f1e3bc4a287f79f38b9aaf60cebd55f609060f32b7ae9d04b8fb6976c97537'),
  firstPart: take('first delimiter-separated source segment', 260463715, 260463780, '7184cd31a9af8f0fac35de5b85a290da988105f5df932de856f663e2dc2427cd'),
  firstLine: take('first source line', 260464148, 260464180, '968dddda80d3ead12b9faf8432897056a27d17312c7abeae2c71647a2ca2765e'),
  interruption: take('goal interruption classification', 280512353, 280514445, 'cff9dd3d40481c5ec828ad677ef4b8de85e9ca3ff99a5ea7ed0b1cca2ad603ea'),
  interruptionTables: take('goal interruption notices and retry ladder', 280509366, 280510704, '9366de50c9296f9438f58a646cf01ec370cb4fd65fb569cc5b1e3d18cfd72337'),
  checkinInterval: take('goal check-in interval', 280412762, 280412869, 'ae8b083d07a2c87d44d316d935d74a1c6aa8851d8b91d77ab7604ae26b6109f5'),
  checkinConstants: take('goal check-in defaults', 280412465, 280412688, 'add0b8127097dd30686a99f4012bed5869e5cec34a0b1c52f9e60b95da59d6a0'),
  envStringParser: take('typed environment parser adapter', 260739069, 260739110, '6ef20a81a0d196d8d7b79bc4b4d63f0cfca19d6dab5e44378648dfb1ce1a815c'),
  envString: take('typed environment source string', 260739158, 260739207, '8326d38e4082258438fd2a666f2ce6d0bd6ef3762742515191455b12c1405441'),
  envIntParser: take('typed environment bounded integer', 260739692, 260739997, 'bf79a5fc80b8f82ba9dddd1af684d486355b3a2925a47835c557dca8c07dbcaf'),
  envInteger: take('native integer syntax parsing', 260733400, 260733745, 'bb51e30463cba298482c352c7a445705170d8db590d36bc6caa32bc14f99d5b1'),
  envCheckinType: take('typed goal interval configuration', 260904385, 260904426, 'f24cd945aaacea95d731c9d0d4d721edb2431f19aaf5efc3e5ee9d1f988113ce'),
};

// Host dependencies are explicit and inert. Canonicalization removes terminal
// escape introducers before the tab helper; its ANSI parser cannot be reached.
const canonicalContext = vm.createContext({ Bun, c4e: () => { throw Error('unexpected terminal parser dependency'); } });
const canonicalExports = vm.runInContext(`(function(){${source.surrogates}${source.invisible}${source.width}${source.tabs}${source.controls}${source.terminal}${source.canonical}${source.singleline};return{n3,H5e,canonical:value=>n3(H5e(value)).trim()}})()`, canonicalContext);
const canonical = canonicalExports.canonical;
const normalize = value => JSON.parse(JSON.stringify(value));
const flush = async () => { for (let index = 0; index < 8; index++) await Promise.resolve(); };

function environment({ state = null, gate = {}, relay = {}, mode = 'auto', nonInteractive = false, remote = false, thinClient = false, flag = true } = {}) {
  const host = {
    activeGoal: state === null ? undefined : { setAt: 1000, tokensAtStart: 1, origin: 'user', ...state },
    gate: { hooksRestricted: false, hooksHeld: false, trusted: true, nonInteractive, ...gate },
    relay: { dispatchedOverBridge: false, submissionVerifiedSlackHumanTurn: false, enabled: true, ...relay },
    mode, nonInteractive, remote, thinClient, flag,
    hooks: [], operations: [], queue: [], origins: [], telemetry: [], availabilityLogged: false,
    proposalState: { id: undefined, clear() { this.id = undefined; } },
  };
  if (host.activeGoal) host.hooks.push({ type: 'prompt', prompt: host.activeGoal.condition });
  const context = vm.createContext({
    Date: class extends Date { constructor(...parts) { super(...(parts.length ? parts : [10_000])); } static now() { return 10_000; } },
    G: () => 'session', AS: () => 100,
    ye: () => host.gate.nonInteractive || host.nonInteractive,
    Rn: () => host.remote, Vt: () => host.thinClient,
    Mn: () => false, kte: () => host.gate.hooksRestricted,
    oh: () => host.gate.hooksRestricted, u$t: () => false, oOs: () => false,
    yb: () => host.gate.hooksHeld, r$t: () => host.operations.push('read_hook_policy'),
    Vs: () => host.gate.trusted,
    l: (name, metadata) => host.telemetry.push({ name, metadata }),
    g: () => {}, c: (event, reason) => host.operations.push({ failed: event, reason }),
    f: value => value, ce: value => value, S: value => value,
    T5i: () => {}, d: () => 'fixture-goal-uuid',
    k: (name, fallback) => name === 'tengu_mellow_sunset' ? host.relay.enabled : name === 'tengu_propose_goal' ? host.flag : fallback,
    J9s: () => host.availabilityLogged, Q9s: () => { host.availabilityLogged = true; },
    xP: () => [host.mode === 'unset' ? undefined : host.mode],
    Xnt: async () => host.persistedSettingPresence ?? 'absent',
    H5e: canonicalExports.H5e,
    n3: canonicalExports.n3,
    N5e: () => { throw Error('native proposal renderer is outside this replay'); },
    O: () => 'fixture-proposal-uuid', nt: () => 'main', ice: Symbol('goal-dialog'), s5t: Symbol('proposal-state'),
    a: error => host.operations.push({ dialogError: String(error) }),
    T: class extends Error {},
    // The factory receipt records the native field metadata, not native Zod's
    // JSON Schema serializer or aggregate diagnostic formatting.
    p: build => { let cached; return () => cached ??= build(); },
    cn: value => value,
    o: () => field('string'), I: () => field('boolean'),
    Ne: fields => ({ type: 'object', strict: true, fields: schemaFields(fields) }),
    u: fields => ({ type: 'object', strict: false, fields: schemaFields(fields) }),
  });
  function field(type) {
    return {
      metadata: { type },
      min(value) { this.metadata.minLength = value; return this; },
      max(value) { this.metadata.maxLength = value; return this; },
      optional() { this.metadata.optional = true; return this; },
      describe(description) { this.metadata.description = description; return this; },
    };
  }
  function schemaFields(fields) {
    return Object.entries(fields).map(([name, schema]) => ({ name, ...schema.metadata }));
  }
  const goal = vm.runInContext(`(function(){${source.state};return{KPr,C1n,Yfe,Qon,e$t,BPs,qPr,EZe,k5i,jPs,T1n,v5i,t$t,n$t,tEo}})()`, context);
  Object.assign(context, goal);
  Object.assign(context, vm.runInContext(`(function(){${source.plural}${source.firstPart}${source.firstLine};return{P,Gn}})()`, context));
  const command = vm.runInContext(`(function(){${source.command};return p})()`, context);
  const proposalPrompt = vm.runInContext(`(function(){${source.proposalPrompt};return{JLt,pvr,PBi,OBi}})()`, context);
  Object.assign(context, proposalPrompt);
  vm.runInContext(source.settings, context);
  vm.runInContext(source.flag, context);
  const proposal = vm.runInContext(`(function(){${source.proposal};return X})()`, context);
  const commandHost = {
    options: {},
    getAppState: () => ({ activeGoal: host.activeGoal, queuedGoalOrigin: host.queuedGoalOrigin }),
    setAppState: update => {
      const next = update({ activeGoal: host.activeGoal, queuedGoalOrigin: host.queuedGoalOrigin });
      host.activeGoal = next.activeGoal; host.queuedGoalOrigin = next.queuedGoalOrigin;
    },
    applyMessageOp: operation => host.operations.push({ append: operation.messages.map(row => row.attachment) }),
    sessionHooksRegistry: {
      get: () => new Map([['Stop', [{ matcher: '', hooks: host.hooks }]]]),
      add: (_session, _event, _matcher, hook) => { host.hooks.push(hook); },
      remove: (_session, _event, hook) => { host.hooks = host.hooks.filter(value => value !== hook); },
    },
  };
  return { host, context, goal, command, commandHost, proposal, proposalPrompt };
}

const commands = [];
async function commandCase(name, args, options = {}) {
  const env = environment(options);
  const { host, commandHost } = env;
  commandHost.options.activeGoal = host.activeGoal;
  Object.assign(commandHost, host.relay);
  const expected = normalize(await env.command(args, commandHost));
  commands.push({
    name, args, state: options.state ?? null,
    gate: host.gate, relay: host.relay, expected,
    afterState: host.activeGoal ? normalize(host.activeGoal) : null,
    operations: normalize(host.operations),
  });
}
await commandCase('no active goal', '');
await commandCase('ECMAScript whitespace-only status', '\ufeff \t\r\n');
await commandCase('active, not evaluated', '', { state: { condition: 'ship it', iterations: 0 } });
await commandCase('active, one turn', '', { state: { condition: 'ship it', iterations: 1 } });
await commandCase('active, last reason first line', '', { state: { condition: 'ship it', iterations: 7, lastReason: ' \t first\nsecond \r\n ' } });
await commandCase('active, truthy whitespace-only reason', '', { state: { condition: 'ship it', iterations: 2, lastReason: ' \t ' } });
await commandCase('active, exact lone UTF-16 units', '', { state: { condition: 'a\ud800z', iterations: 3, lastReason: '\udc00 check\nnext' } });
for (const token of ['clear', 'STOP', 'Off', 'reset', 'none', 'CANCEL']) {
  await commandCase(`clear token ${token}`, ` \ufeff${token}\t`, { state: { condition: 'old goal', iterations: 4 } });
}
await commandCase('clear with nothing set', 'clear');
await commandCase('set a goal', ' ship it ');
await commandCase('set exact UTF-16 source', 'a\ud800\0\udc00z');
await commandCase('typed goal retains internal lines', 'first\nsecond\tthird');
await commandCase('NEL is not ECMAScript trim whitespace', '\u0085');
await commandCase('replace an active goal', 'new goal', { state: { condition: 'old goal', iterations: 9 } });
await commandCase('at the 4000-unit cap', 'x'.repeat(4000));
await commandCase('over the 4000-unit cap', 'x'.repeat(4001));
await commandCase('2000 emoji fit exactly', '😀'.repeat(2000));
await commandCase('2001 emoji exceed UTF-16 cap', '😀'.repeat(2001));
await commandCase('length precedes set gates', 'x'.repeat(4001), { gate: { hooksRestricted: true, hooksHeld: true, trusted: false } });
await commandCase('restricted hooks precede held policy and trust', 'goal', { gate: { hooksRestricted: true, hooksHeld: true, trusted: false } });
await commandCase('held policy precedes trust', 'goal', { gate: { hooksHeld: true, trusted: false } });
await commandCase('held policy in a trusted workspace', 'goal', { gate: { hooksHeld: true } });
await commandCase('trust gate', 'goal', { gate: { trusted: false } });
await commandCase('noninteractive trust bypass', 'goal', { gate: { trusted: false, nonInteractive: true } });
await commandCase('status bypasses set gates', '', { gate: { hooksRestricted: true, hooksHeld: true, trusted: false } });
await commandCase('clear bypasses set gates', 'clear', { state: { condition: 'clear me', iterations: 0 }, gate: { hooksRestricted: true, hooksHeld: true, trusted: false } });
for (const args of ['', 'clear', 'set this', 'x'.repeat(4001)]) {
  await commandCase(`verified relay blocks ${args.length > 50 ? 'overlong input' : args || 'status'}`, args, { state: { condition: 'keep', iterations: 1 }, relay: { dispatchedOverBridge: true, submissionVerifiedSlackHumanTurn: true } });
}
await commandCase('bridge without verified relayed-human fact', 'allowed', { relay: { dispatchedOverBridge: true } });
await commandCase('verified fact without bridge dispatch', 'allowed', { relay: { submissionVerifiedSlackHumanTurn: true } });
await commandCase('relay feature disabled', 'allowed', { relay: { dispatchedOverBridge: true, submissionVerifiedSlackHumanTurn: true, enabled: false } });

const normalizationInputs = [
  ['ASCII trim', '  ship it  '], ['line-break run', 'a\r\n\u2028\u2029b'],
  ['separated line breaks', 'a\n \rb'], ['one tab', 'a\tb'], ['two tabs', 'a\t\tb'],
  ['leading and trailing tabs', '\tship it\t'], ['CJK tab stop', '界\tready'],
  ['emoji tab stop', '😀\tready'], ['combining tab stop', 'e\u0301\tready'],
  ['ZWJ family then tab', '👨\u200d👩\u200d👧\tready'],
  ['format characters', 'a\u200b\u200f\u2066\ufeffb'],
  ['private-use characters', 'a\ue000\udb80\udc00b'],
  ['unassigned characters', 'a\u0378\uffffb'],
  ['default-ignorable combining marks', 'a\u034f\ufe0f\u180bb'],
  ['invisible-only', '\u200b\ue000\u034f'],
  ['C0 and C1 controls', 'a\0\x01\x08\x0b\x0c\x1b\x7f\x85\x9bb'],
  ['ANSI-looking text', '\x1b[31mred\x1b[0m'],
  ['unpaired high surrogate', 'a\ud800b'], ['unpaired low surrogate', 'a\udc00b'],
  ['paired surrogate emoji', 'a😀b'], ['fullwidth punctuation remains', 'Ａ：Ｂ'],
  ['Unicode spaces trim', '\u00a0\u2002word\u3000'],
  ['tabs exceed canonical cap', `a${'\t'.repeat(63)}x`],
];
const normalization = normalizationInputs.map(([name, input]) => ({ name, input, expected: canonical(input) }));

const proposalCases = [];
async function proposalCase(name, input, facts = {}) {
  const env = environment({ ...facts, mode: facts.setting ?? 'auto' });
  const { host } = env;
  let decide;
  const runtime = {
    agentId: facts.agent ? 'child' : undefined,
    permissions: () => ({ mode: facts.planMode ? 'plan' : 'default' }),
    storageV5: {}, toolState: { get: () => host.proposalState },
    recordQueuedGoalOrigin: (condition, origin) => host.origins.push({ condition, origin }),
    messageQueue: { enqueue: message => host.queue.push(message) },
    requestDialog: facts.dialogAvailable === false ? undefined : (_component, options) => {
      host.operations.push({ dialog: options });
      return new Promise(resolve => { decide = resolve; });
    },
  };
  if (facts.pending) host.proposalState.id = 'previous-proposal';
  let expected;
  try { expected = { data: normalize((await env.proposal.create(runtime).call(input)).data) }; }
  catch (error) { expected = { error: error.message }; }
  if (facts.decision) {
    if (facts.disabledAfterApproval) host.mode = 'disabled';
    if (facts.planAfterApproval) runtime.permissions = () => ({ mode: 'plan' });
    if (facts.staleAfterApproval) host.proposalState.id = 'replacement-proposal';
    assert(decide, `${name}: approval promise exists`);
    decide(facts.decision);
    await flush();
  }
  proposalCases.push({ name, input, facts, expected, queue: normalize(host.queue), origins: normalize(host.origins), pending: host.proposalState.id ?? null });
}
await proposalCase('default approval remains nonblocking', { condition: 'tests pass' });
await proposalCase('direct kickoff', { condition: 'tests pass', ask_user: false });
await proposalCase('alwaysAsk overrides direct request', { condition: 'tests pass', ask_user: false }, { setting: 'alwaysAsk' });
await proposalCase('approved kickoff', { condition: 'tests pass' }, { decision: { approved: true } });
await proposalCase('explicit decline stays silent', { condition: 'tests pass' }, { decision: { approved: false, explicit: true } });
await proposalCase('unanswered proposal stays silent', { condition: 'tests pass' }, { decision: { approved: false } });
await proposalCase('disabled before approval resolves', { condition: 'tests pass' }, { decision: { approved: true }, disabledAfterApproval: true });
await proposalCase('plan mode before approval resolves', { condition: 'tests pass' }, { decision: { approved: true }, planAfterApproval: true });
await proposalCase('stale approval cannot replace a new proposal', { condition: 'tests pass' }, { decision: { approved: true }, staleAfterApproval: true });
await proposalCase('agent context first', { condition: '' }, { agent: true });
await proposalCase('noninteractive session first', { condition: '' }, { nonInteractive: true });
await proposalCase('remote session', { condition: 'tests pass' }, { remote: true });
await proposalCase('thin client session', { condition: 'tests pass' }, { thinClient: true });
await proposalCase('empty visible condition', { condition: '\u200b\ue000' });
await proposalCase('canonical display cap', { condition: `a${'\t'.repeat(63)}x` });
await proposalCase('clear token rejected', { condition: ' \tSTOP\n' });
await proposalCase('hook restriction precedes plan and setting', { condition: 'tests pass' }, { gate: { hooksRestricted: true }, planMode: true, setting: 'disabled' });
await proposalCase('held hook policy', { condition: 'tests pass' }, { gate: { hooksHeld: true } });
await proposalCase('untrusted workspace', { condition: 'tests pass' }, { gate: { trusted: false } });
await proposalCase('plan before disabled setting', { condition: 'tests pass' }, { planMode: true, setting: 'disabled' });
await proposalCase('disabled setting', { condition: 'tests pass' }, { setting: 'disabled' });
await proposalCase('dialog required even for a direct request', { condition: 'tests pass', ask_user: false }, { dialogAvailable: false });
await proposalCase('pending decision blocks another proposal', { condition: 'tests pass', ask_user: false }, { pending: true });
await proposalCase('canonical condition reaches queue', { condition: ' \t界\tall\r\ntests\u200b pass ', ask_user: false });

const proposalAvailability = [];
for (const facts of [{}, { flag: false }, { nonInteractive: true }, { remote: true }, { thinClient: true }, { mode: 'alwaysAsk' }, { mode: 'disabled' }]) {
  const env = environment(facts);
  proposalAvailability.push({ facts, expected: env.proposal.isEnabled() });
}
const settingCases = [];
for (const setting of ['auto', 'alwaysAsk', 'disabled', 'unset']) {
  for (const presence of ['absent', 'present', 'unknown']) {
    const env = environment({ mode: setting });
    env.host.persistedSettingPresence = presence;
    settingCases.push({ setting, presence, sync: env.context.fWt(), async: await env.context.TWs({}) });
  }
}
const interruptionContext = vm.createContext({
  i: {}, EA: () => false, Rd: () => false, Whe: () => null, iAe: () => false,
});
const interruptionNative = vm.runInContext(`(function(){${source.interruptionTables}${source.interruption};return{Xf,Jf,backoffs:$i,jitter:Vf,maxRetries:Qi,retryCauses:Yf,pauseMessages:zf}})()`, interruptionContext);
const interruptionCases = [];
function interruptionCase(name, input, context = {}) {
  interruptionContext.i.CLAUDE_CODE_REMOTE = context.envRemote ?? false;
  interruptionContext.EA = () => context.quotaExhausted ?? false;
  interruptionContext.Rd = () => context.remoteHost ?? false;
  interruptionContext.Whe = () => context.workspaceHost ? {} : null;
  interruptionContext.iAe = () => context.inProcessRemote ?? false;
  const result = interruptionNative.Xf(input);
  assert(result !== undefined, `${name}: known native reason`);
  interruptionCases.push({ name, input, context, expected: normalize(result) });
}
for (const reason of ['aborted_streaming', 'aborted_tools', 'max_turns', 'tool_deferred', 'background_requested', 'stop_hook_prevented', 'hook_stopped', 'blocking_limit', 'prompt_too_long', 'rapid_refill_breaker', 'image_error', 'model_error', 'malformed_tool_use_exhausted']) {
  interruptionCase(reason, { reason });
}
for (const goalPass of ['error', 'cancelled', 'not_met', 'met', 'impossible', 'deferred', undefined]) {
  interruptionCase(`completed ${goalPass ?? 'without a goal pass'}`, { reason: 'completed', ...(goalPass === undefined ? {} : { goalPass }) });
}
for (const errorKind of ['rate_limit', 'max_output_tokens', 'cloud_credential_error', 'overloaded', 'server_error', 'unknown', 'invalid_request', 'authentication_failed', 'oauth_org_not_allowed', 'account_on_hold', 'verification_required', 'billing_error', 'model_not_found', undefined]) {
  interruptionCase(`API ${errorKind ?? 'unclassified'}`, { reason: 'api_error', ...(errorKind === undefined ? {} : { errorKind }) });
}
interruptionCase('transient invalid request retries', { reason: 'api_error', errorKind: 'invalid_request', isTransient: true });
interruptionCase('rate limiting precedes transient retry', { reason: 'api_error', errorKind: 'rate_limit', isTransient: true });
interruptionCase('usage limit exhaustion', { reason: 'api_error', errorKind: 'rate_limit', quotaLimits: {} }, { quotaExhausted: true });
interruptionCase('daily cap precedes quota exhaustion and transient retry', { reason: 'api_error', errorKind: 'rate_limit', isTransient: true, quotaLimits: { dailyCapPaused: true } }, { quotaExhausted: true });
interruptionCase('daily cap does not affect a different API error', { reason: 'api_error', errorKind: 'server_error', quotaLimits: { dailyCapPaused: true } });
for (const context of [{ envRemote: true }, { remoteHost: true }, { workspaceHost: true }, { inProcessRemote: true }]) {
  interruptionCase(`remote auth ${Object.keys(context)[0]}`, { reason: 'api_error', errorKind: 'authentication_failed' }, context);
}
interruptionCase('OAuth refresh disk-full stop', { reason: 'api_error', errorKind: 'unknown', oauthRefreshDiskFull: true });
interruptionCase('remote OAuth refresh disk-full retry', { reason: 'api_error', errorKind: 'unknown', oauthRefreshDiskFull: true }, { envRemote: true });
const interruption = {
  backoffs: normalize(interruptionNative.backoffs), jitter: interruptionNative.jitter, maxRetries: interruptionNative.maxRetries,
  retryCauses: normalize(interruptionNative.retryCauses), pauseMessages: normalize(interruptionNative.pauseMessages),
  cases: interruptionCases,
};
const envContext = vm.createContext({});
const typedInt = vm.runInContext(`(function(){${source.envStringParser}${source.envString}${source.envInteger}${source.envIntParser};var H={int:t};var ${source.envCheckinType};return Ai})()`, envContext);
const intervalContext = vm.createContext({ i: {}, k: () => true });
const interval = vm.runInContext(`(function(){${source.checkinConstants}${source.checkinInterval};return{jr,defaultMinutes:Np,idlePausedSuffix:Hp,idlePausedInstruction:Gp,concurrentCheckLimit:Bp,consecutiveCheckinLimit:qp,idleTaskInterval:jp}})()`, intervalContext);
const intervalCases = [];
for (const raw of [undefined, '0', '-0', '+0', '1', '30', '+30', '10080', '10081', '-1', '0.5', '', ' ', '1e1', '1_000', '\ufeff30', '\u008530', `${'0'.repeat(300)}1`, '9999999999999999999999999999999999999999']) {
  const parsed = typedInt.parse(raw);
  intervalContext.i.CLAUDE_CODE_GOAL_CHECKIN_MINUTES = parsed;
  intervalCases.push({ name: raw === undefined ? 'absent' : JSON.stringify(raw), ...(raw === undefined ? {} : { raw }), flag: true, parsed: parsed ?? null, expected: interval.jr() });
}
intervalContext.k = () => false;
intervalCases.push({ name: 'feature disabled', flag: false, raw: '30', parsed: 30, expected: interval.jr() });
const checkin = { defaultMinutes: interval.defaultMinutes, idlePausedSuffix: interval.idlePausedSuffix, idlePausedInstruction: interval.idlePausedInstruction, concurrentCheckLimit: interval.concurrentCheckLimit, consecutiveCheckinLimit: interval.consecutiveCheckinLimit, idleTaskInterval: interval.idleTaskInterval, intervalCases };
const sample = environment();
const fixture = {
  native: { version: '2.1.322', sha256: nativeSha256, runtime: `Bun ${Bun.version}` },
  sourceByteRanges,
  directive: sample.goal.qPr('{condition}'),
  gates: { trust: commands.find(row => row.name === 'trust gate').expected.value, held: commands.find(row => row.name === 'held policy in a trusted workspace').expected.value, restricted: commands.find(row => row.name === 'restricted hooks precede held policy and trust').expected.value, relay: sample.goal.k5i },
  commands, normalization, interruption, checkin,
  proposal: {
    name: sample.proposal.name, description: await sample.proposal.description(), prompt: await sample.proposal.prompt(),
    searchHint: sample.proposal.searchHint, shouldDefer: sample.proposal.shouldDefer, backgrounding: sample.proposal.backgrounding,
    maxResultSizeChars: sample.proposal.maxResultSizeChars, concurrencySafe: sample.proposal.isConcurrencySafe(), readOnly: sample.proposal.isReadOnly(),
    inputSchema: normalize(sample.proposal.inputSchema), outputSchema: normalize(sample.proposal.outputSchema),
    results: { approval: sample.proposal.mapToolResultToToolResultBlockParam({ askUser: true }, 'tool').content, direct: sample.proposal.mapToolResultToToolResultBlockParam({ askUser: false }, 'tool').content },
    availability: proposalAvailability, settings: settingCases, cases: proposalCases,
    classifierInputs: [{ condition: 'tests pass' }, { condition: 'tests pass', ask_user: false }, { condition: 'a\ud800z', ask_user: true }].map(input => ({ input, expected: sample.proposal.toAutoClassifierInput(input) })),
  },
  boundaries: [
    'Unchanged bounded native helpers replay with explicit inert host dependencies; no assistant session, credentials or network request is used.',
    'Native Bun.stringWidth delegates to the installed pinned Bun runtime; future runtime or Unicode-table changes require a reviewed fixture refresh.',
    'Schema factories record ordered native field metadata, strictness, requiredness and limits; native Zod JSON Schema serializer bytes and aggregate diagnostic formatting are not asserted.',
    'Approval tests use controlled promises and exact queue operations; they do not establish native terminal rendering or cross-platform UI acceptance.',
  ],
};
const fixturePath = fileURLToPath(new URL('../../crates/command-api/tests/fixtures/goal_2_1_322.json', import.meta.url));
const serialized = `${JSON.stringify(fixture, null, 2)}\n`;
if (args.includes('--check')) assert.equal(readFileSync(fixturePath, 'utf8'), serialized, 'native Goal fixture replay');
else writeFileSync(fixturePath, serialized);
console.log(`Goal 2.1.322 native replay passed: ${commands.length} commands, ${normalization.length} canonicalizations, ${proposalCases.length} proposals, ${interruptionCases.length} interruptions, ${intervalCases.length} intervals, ${sourceByteRanges.length} pinned ranges`);
