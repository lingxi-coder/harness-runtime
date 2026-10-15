#!/usr/bin/env node
import assert from 'node:assert/strict';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';
import {nativeBetaWorld} from './common/anthropic_native_288.mjs';

const {binary, hash, ranges, take, context} = nativeBetaWorld(process.argv[2]);
const canonical = take('function qI(e)', 'function ', 181784550, 'native bare-wire identity branch');
const date = take('function Bb(e)', 'function ', 181786200, 'native date identity suffix');
const contextTag = take('function kt(e){return e.replace', 'function ', 179326600, 'native context identity suffix');
const target = take('function P1e()', 'function Qpn(', 185908850, 'actual P1e live latch and $Fo query target');
const selection = take('class pe{', 'class fe{', 178985800, 'actual native model-selection state');
const modelPick = take('function oae()', 'function CB(', 179041300, 'actual model-pick latch removal');
take('async function pNe(', 'function ', 199633200, 'native approved model-pick caller');
vm.runInContext(`var Mee=['us','eu','apac','global'];
function sie(){return undefined;}function Iu(e){return {base:e};}function TW(){return false;}
${canonical}${date}${contextTag}
function Nm(e){return kt(qI(e.toLowerCase()));}
function CB(){return latch;}function Fc(){return mainOverride;}
${target}${selection}
function n(){return host;}${modelPick}`, context);

const models = ['claude-opus-4-7', 'claude-opus-4-7[1m]', 'claude-sonnet-4-6', 'other', 'OTHER'];
const choices = [null, ...models];
const identities = Object.fromEntries([...new Set([...models, ...models.map(model => `${model}[1m]`)])]
  .map(model => [model, context.Nm(model)]));
const cases = [];
for (const request_model of models) for (const user_model of models)
  for (const turn_override of choices) for (const chain_step_model of choices)
    for (const [latched_model, main_loop_override] of [
      [null, null], [request_model, request_model], [request_model, null],
      [request_model, user_model], [request_model, `${request_model}[1m]`],
    ]) {
      context.latch = latched_model === null ? undefined : {
        fallbackModel: latched_model, previousAppStateModel: user_model,
      };
      context.mainOverride = main_loop_override ?? undefined;
      const expected = context.$Fo({requestModel: request_model, userModel: user_model,
        turnRefusalOverride: turn_override ?? undefined, chainStepModel: chain_step_model ?? undefined});
      cases.push({request_model, user_model, turn_override, chain_step_model,
        latched_model, main_loop_override, expected});
    }
context.host = {modelSelection: vm.runInContext('new pe()', context)};
context.host.modelSelection.overrideMainLoopModel('fallback');
context.host.modelSelection.replaceRefusalFallbackModelLatch({fallbackModel: 'fallback'});
context.host.modelSelection.markRefusalFallbackOccurred();
context.oae();
const modelPickState = {
  targetAfterPick: context.host.modelSelection.refusalFallbackModelLatch() ?? null,
  routingStillLatched: context.host.modelSelection.refusalFallbackOccurred(),
  overrideAfterPick: context.host.modelSelection.mainLoopModelOverride(),
};
assert.deepEqual(modelPickState, {targetAfterPick: null, routingStillLatched: true, overrideAfterPick: 'fallback'});
const output = {version: '2.1.288', native_sha256: hash(binary), sourceByteRanges: ranges,
  scope: 'Actual $Fo and P1e with supplied query/latch host state. Native bare-wire identity executes qI/Bb/kt with served aliases, model overrides and transport parsing absent. This corpus proves the predicate, not full server header, session restore or model identity acquisition.',
  identities, cases, modelPickState};
const text = JSON.stringify(output, null, 2) + '\n';
const file = new URL('../../crates/core/tests/fixtures/fallback_target_2_1_288.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(file, 'utf8'), text);
else writeFileSync(file, text);
console.log(JSON.stringify({cases: cases.length, sourceRanges: ranges.length}));
