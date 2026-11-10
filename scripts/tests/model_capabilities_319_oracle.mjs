#!/usr/bin/env node
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';

const nativePath = process.argv[2];
assert(nativePath, 'Usage: model_capabilities_319_oracle.mjs <official binary> [--check]');
const fd = fs.openSync(nativePath, 'r');
const digest = createHash('sha256');
const block = Buffer.alloc(1024 * 1024);
for (let at = 0, size; (size = fs.readSync(fd, block, 0, block.length, at)); at += size) {
  digest.update(block.subarray(0, size));
}
const nativeSha256 = digest.digest('hex');
assert.equal(nativeSha256, '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd');
const ranges = [];
function take(start, endExclusive, label) {
  const bytes = Buffer.alloc(endExclusive - start);
  assert.equal(fs.readSync(fd, bytes, 0, bytes.length, start), bytes.length);
  ranges.push({ label, start, endExclusive, sha256: createHash('sha256').update(bytes).digest('hex') });
  return bytes.toString('utf8');
}
const catalogSource = take(260742015, 260762295, 'kAo baked model catalog');
const lookupSource = take(260766296, 260767012, 'Rw/E/HNs native capability lookup');
const featureSource = take(264443412, 264444006, 'jx prompt feature fallback');
const leanSource = take(264447673, 264448314, '_t/TX/b0/ce native lean prompt selection');
const rewriteSource = take(264445493, 264445771, 'ie native temporal_hare rewrite selection');
const familySource = take(264152516, 264152702, 'd2/nze native family gates');
const writingSource = take(264447424, 264447630, '_2i native writing guidance selection');
const systemSource = take(263106427, 263106494, 'u7 request system-turn capability');
const systemGateSource = take(263106811, 263107107, 'w$/rIr system-turn defaults and exclusion');
const opus48Source = take(264444594, 264444643, 'sht Opus 4.8 exclusion');
const systemExplanationSource = take(271755332, 271755383, 'kPe system-turn prompt selection');
const modelOrderSource = take(263101251, 263101552, 'FA/pyt native chronological model threshold');
const modelSuffixSource = take(263092158, 263092205, 'vU context suffix removal');
fs.closeSync(fd);

const catalog = vm.runInNewContext(`${catalogSource};kAo`);
const models = catalog.models.map(({ id, capabilities, provider_ids }) => ({ id, capabilities, provider_ids }));
const state = { environment: undefined, served: undefined, client: undefined, clientCapability: 'temporal_hare', compiled: false, cohort: false, flag: false, simple: undefined, configured: false, lean: undefined, force: false, hipaa: false, systemConfigured: undefined };
const context = vm.createContext({
  state,
  i: {},
  B: 'tengu_willow_tern',
  uH: () => ({ servedCapabilityLookup: () => state.served }),
  sd: id => models.find(model => model.id === id),
  rt: id => id?.replace(/\[1m\]/gi, ''),
  djn: () => state.client === undefined ? undefined : { data: { [state.clientCapability]: state.client } },
  tte: () => state.compiled,
  syt: () => state.cohort,
  rn: (_key, fallback) => state.flag || fallback,
  k: (_key, fallback) => fallback,
  Z: () => {},
  vFr: () => false,
  rks: () => undefined,
  Pe: value => typeof value === 'string' && ['1', 'true', 'yes'].includes(value.toLowerCase()),
  rs: value => typeof value === 'string' && ['0', 'false', 'no'].includes(value.toLowerCase()),
  pe: () => state.configured,
  dl: (_field, _model, load) => load(),
  us: () => state.hipaa,
  mVe: () => state.systemConfigured,
  $t: value => value,
});
vm.runInContext(`${lookupSource}${featureSource}${leanSource}${rewriteSource}${familySource}${writingSource}${systemSource}${systemGateSource}${opus48Source}${systemExplanationSource}${modelOrderSource}${modelSuffixSource}
function C(){return {leanPrompt: e => state.lean ?? ce(e), leanPromptCompiledOnly: e => state.lean ?? ce(e)}}
function CCr(e){return Rw(e,'opus_5_prompt_bundle',e)===true}
function L(e){return Rw(e,'opus_5_5_prompt_bundle',e)===true}
`, context);

function reset(options = {}) {
  Object.assign(state, { environment: undefined, served: undefined, client: undefined, clientCapability: 'temporal_hare', compiled: false, cohort: false, flag: false, simple: undefined, configured: false, lean: undefined, force: false, hipaa: false, systemConfigured: undefined }, options);
  context.i.CLAUDE_CODE_MODEL_CAPABILITIES = state.environment;
  context.i.CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT = state.simple;
  context.i.CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM = state.force;
}
function run(expression) { return vm.runInContext(expression, context); }
function expr(fn, ...args) { return `${fn}(${args.map(arg => JSON.stringify(arg)).join(',')})`; }

const promptTraits = models.map(({ id }) => {
  reset();
  return {
    model: id,
    lean: run(expr('ce', id)),
    rewrite: run(expr('ie', id)),
    turnUpdates: run(expr('jx', 'turn_updates', id, id)),
    fableMitigations: run(expr('d2', id, id)),
    deliveringWork: run(expr('nze', id)) || run(expr('jx', 'bison_cairn', id, id)),
    corrections: run(expr('jx', 'larch_cistern', id, id)),
    silentTurn: run(expr('jx', 'silent_turn_reminder', id, id)),
    originalGames: run(expr('jx', 'original_games_guidance', id, id)),
    earlyStopping: run(expr('jx', 'haiku_5_5_early_stopping_guidance', id, id)),
    writingForUser: run(expr('_2i', id)),
    systemTurns: run(expr('kPe', id)),
  };
});

const observations = [];
const leanCases = [];
for (const model of [...models.map(row => row.id), 'claude-unknown', 'claude-sonnet-4-6-eap']) {
  for (const support of [null, false, true]) for (const served of [null, false, true]) {
    const environment = support === null ? undefined : support ? 'lean_prompt' : '-lean_prompt';
    reset({ environment, served: served ?? undefined });
    observations.push({ model, support, served, observed: run(expr('Rw', model, 'lean_prompt', model)) ?? null });
    for (const simple of [null, '0', '1']) for (const configured of [false, true]) {
      reset({ environment, served: served ?? undefined, simple: simple ?? undefined, configured });
      leanCases.push({ model, support, served, simple, configured, lean: run(expr('ce', model)) });
    }
  }
}

const rewriteCases = [];
for (const support of [null, false, true]) for (const served of [null, false, true]) {
  for (const client of [null, false, true]) for (const compiled of [false, true]) {
    for (const lean of [false, true]) for (const cohort of [false, true]) {
      const environment = support === null ? undefined : support ? 'temporal_hare' : '-temporal_hare';
      reset({ environment, served: served ?? undefined, client: client ?? undefined, compiled, lean, cohort, flag: true });
      rewriteCases.push({ support, served, client, compiled, lean, cohort, flag: true, rewrite: run(expr('ie', 'claude-opus-5')) });
    }
  }
}

const featureCases = [];
for (const model of ['claude-opus-5', 'claude-opus-5-5', 'claude-fable-5-1', 'claude-mythos-5', 'claude-haiku-5-5', 'claude-sonnet-5-5']) {
  for (const capability of ['turn_updates', 'silent_turn_reminder', 'bison_cairn', 'larch_cistern', 'temporal_hare', 'total_tokens_reminder']) {
    for (const support of [null, false, true]) for (const served of [null, false, true]) for (const client of [null, false, true]) {
      const environment = support === null ? undefined : (support ? '' : '-') + capability;
      reset({ environment, served: served ?? undefined, client: client ?? undefined, clientCapability: capability });
      featureCases.push({ model, capability, support, served, client, enabled: run(expr('jx', capability, model, model)) });
    }
  }
}

const systemCases = [];
for (const model of [...models.map(row => row.id), 'claude-unknown']) {
  for (const support of [null, false, true]) for (const served of [null, false, true]) {
    for (const force of [false, true]) for (const configured of [null, false, true]) for (const hipaa of [false, true]) {
      const environment = support === null ? undefined : support ? 'mid_conv_system' : '-mid_conv_system';
      reset({ environment, served: served ?? undefined, force, hipaa, systemConfigured: configured ?? undefined });
      systemCases.push({ model, support, served, force, configured, hipaa, explainsSystemTurns: run(expr('kPe', model)) });
    }
  }
}

const output = {
  version: '2.1.319', native_sha256: nativeSha256, sourceByteRanges: ranges,
  models, promptTraits, observations, leanCases, rewriteCases, featureCases, systemCases,
  scope: 'Exact baked capability arrays and actual native Rw, _t/ce, ie and jx results. Canonical identity and explicit served/client/cohort/flag observations are controlled inputs; no account lookup, provider request or remote rollout is executed.',
};
const json = JSON.stringify(output, null, 2) + '\n';
const file = new URL('../../crates/core/tests/fixtures/model_capabilities_2_1_319.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(file, 'utf8'), json);
else fs.writeFileSync(file, json);
console.log(`${process.argv.includes('--check') ? 'checked' : 'wrote'} ${models.length} model arrays, ${leanCases.length} lean cases and ${rewriteCases.length} rewrite cases`);
