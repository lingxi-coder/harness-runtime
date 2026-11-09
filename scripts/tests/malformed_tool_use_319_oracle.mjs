#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'Usage: node malformed_tool_use_319_oracle.mjs <official 2.1.319 binary> [--check]');
const nativeSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
const binary = fs.readFileSync(binaryPath);
assert.equal(createHash('sha256').update(binary).digest('hex'), nativeSha256, 'The upstream executable changed');
const sourceByteRanges = [];
function take(name, start, end, after = 259144680) {
  const begin = binary.indexOf(start, after);
  assert(begin >= 0, `Missing ${name} start`);
  const finish = binary.indexOf(end, begin + Buffer.byteLength(start));
  assert(finish > begin, `Missing ${name} end`);
  const bytes = binary.subarray(begin, finish);
  sourceByteRanges.push({name, start: begin, endExclusive: finish, sha256: createHash('sha256').update(bytes).digest('hex')});
  return bytes.toString('utf8');
}
const allowance = take('malformed retry allowance and outcome classifier', 'var gg=2;function Vi(e)', 'function fc(e)');
const guards = take('initial recovery guards', 'var ss={maxOutputTokensRecoveryCount:0', 'function is()');
const messageBuilders = take('native assistant message builders', 'function Lpo({content:', 'function Yct(e)');
const userBuilder = take('native user message builder', 'function Ie({content:', 'function zYt(e)');
const nudgeDeclaration = take('malformed retry nudge', 'gWn="The previous response failed', ',oLs=');
const malformedBranch = take('complete malformed tool-use branch', 'if((R?.message.stop_reason??Jn)==="tool_use"&&so.length===0', 'let Le=R?.message.stop_reason??Jn;');
const maxTokensGuard = take('output-limit transition preserves malformed count', 'guards:{...Ft,maxOutputTokensRecoveryCount:xe}', ',messages:He===void 0').slice('guards:'.length);
const thinkingGuard = take('thinking-only transition preserves malformed count', 'guards:{...Ft,thinkingOnlyNudged:!0}', ',messages:[...D,je]').slice('guards:'.length);
const nextTurnGuard = take('next tool round resets malformed count', 'guards:{...Ft,...Wi,mcpEndTurnOverrideSpent:', ',messages:[...D,...re,...ot]').slice('guards:'.length);
const stopHookGuard = take('stop-hook continuation resets malformed count', 'guards:{...Ft,...ss,malformedToolUseRetryCount:0,stopHookActive:', ',messages:[...D,...re,...ie,...me.blockingErrors]').slice('guards:'.length);
const context = vm.createContext({
  // Only host facts, analytics, ancillary classifier metadata and ids/clocks
  // are controlled. The retry branch and both message builders execute verbatim.
  Mi: '<synthetic>', If: '(no content)', hmo: () => false,
  l() {}, Lt: value => value, f: value => value, pc: () => false, J8e() {}, Eo() {},
});
const api = vm.runInContext(`(()=>{
  ${allowance}; ${guards}; ${messageBuilders}; ${userBuilder}; var ${nudgeDeclaration};
  return {
    run(input){
      let id=0;const O={now:()=>"2000-01-01T00:00:00.000Z",uuid:()=>"fixture-"+(++id)};
      const R=input.response,re=R===undefined?[]:[R],Jn=input.stopReason,so=Array(input.toolCount).fill({});
      const Hn=input.retryCount,Ft={...Wi,malformedToolUseRetryCount:Hn,maxOutputTokensRecoveryCount:2};
      const Fe="fixture-model",D=[{type:"user",uuid:"prompt",message:{role:"user",content:"work"}}],C={},gt=undefined,M="sdk",ho="continue";
      let B;function* run(){${malformedBranch}return "no_retry";}
      const iterator=run(),events=[];let outcome;
      while(true){const step=iterator.next();if(step.done){outcome=step.value;break;}events.push(step.value);}
      return {events,state:B,outcome};
    },
    transitions(retryCount){
      const Ft={...Wi,malformedToolUseRetryCount:retryCount},xe=1,na=false,Mn=undefined,Er=2,Be=1,J=2;
      return {max_tokens:(${maxTokensGuard}).malformedToolUseRetryCount,
        thinking_only:(${thinkingGuard}).malformedToolUseRetryCount,
        next_turn:(${nextTurnGuard}).malformedToolUseRetryCount,
        stop_hook_blocking:(${stopHookGuard}).malformedToolUseRetryCount};
    },
    retryAllowed:Vi,outcome:mc
  };
})()`, context);
const response = (stopReason='tool_use', isApiErrorMessage=false) => ({
  type: 'assistant', uuid: 'malformed-attempt', message: {stop_reason: stopReason, content: [{type: 'text', text: 'I will call the tool'}]},
  ...(isApiErrorMessage ? {isApiErrorMessage: true} : {}),
});
const cases = [0, 1, 2, 3].map(retryCount => ({name: `malformed-retry-count-${retryCount}`, input: {retryCount, toolCount: 0, response: response()}}));
cases.push(
  {name: 'valid-tool-does-not-enter-malformed-branch', input: {retryCount: 1, toolCount: 1, response: response()}},
  {name: 'api-error-does-not-enter-malformed-branch', input: {retryCount: 1, toolCount: 0, response: response('tool_use', true)}},
  {name: 'end-turn-does-not-enter-malformed-branch', input: {retryCount: 1, toolCount: 0, response: response('end_turn')}},
  {name: 'fallback-stop-reason-with-no-assistant-row', input: {retryCount: 1, toolCount: 0, stopReason: 'tool_use'}},
);
for (const row of cases) row.expected = JSON.parse(JSON.stringify(api.run(row.input)));
const transitions = [0, 1, 2, 3].map(retryCount => ({retryCount, expected: JSON.parse(JSON.stringify(api.transitions(retryCount)))}));
const outcomes = [0, 1, 2, 3].map(retryCount => ({retryCount, retryAllowed: api.retryAllowed(retryCount),
  malformed: api.outcome(0, 'tool_use', false, retryCount), success: api.outcome(1, 'tool_use', false, retryCount)}));
assert.deepEqual(outcomes.map(row => row.retryAllowed), [true, true, false, false]);
for (const row of cases.slice(0, 2)) {
  assert.equal(row.expected.events[0].type, 'tombstone');
  assert.equal(row.expected.events[1].turnCompanion, true);
  assert.equal(row.expected.state.guards.malformedToolUseRetryCount, row.input.retryCount + 1);
}
for (const row of cases.slice(2, 4)) {
  const error = row.expected.events[0];
  assert.equal(error.apiError, 'malformed_tool_use_exhausted');
  assert.equal(error.error, undefined);
  assert.equal(error.message.stop_reason, 'stop_sequence');
  assert.deepEqual(row.expected.outcome, {reason: 'malformed_tool_use_exhausted'});
}
const output = {version: '2.1.319', native_sha256: nativeSha256, sourceByteRanges,
  scope: 'Verbatim retry allowance, malformed-tool-use generator branch, recovery guard expressions and native user/assistant message builders. Analytics, host context, ancillary classifier metadata and deterministic ids/clocks are controlled. This is source evaluation, not a live CLI/API execution.',
  cases, transitions, outcomes};
const fixture = new URL('../../crates/orchestrator/tests/fixtures/malformed_tool_use_2_1_319.json', import.meta.url);
const encoded = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(fixture, 'utf8'), encoded);
else fs.writeFileSync(fixture, encoded);
console.log(`${process.argv.includes('--check') ? 'checked' : 'wrote'} ${cases.length} malformed-tool cases and ${transitions.length} guard-transition cases from native 2.1.319`);
