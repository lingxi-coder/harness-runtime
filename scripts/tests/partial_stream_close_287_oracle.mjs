#!/usr/bin/env node
// Execute the exact native partial-message close state and completion predicates.
// Usage: node scripts/tests/partial_stream_close_287_oracle.mjs /path/to/2.1.287 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const digest = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = digest(binary);
assert.equal(nativeSha256, '6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges = [];
const sourceBase = 185762130;
function takeBytes(start, end, label) {
  assert(start >= sourceBase && end > start && end <= binary.length, `Invalid native ${label} span`);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({ label, start, end, sha256: digest(bytes) });
  return bytes.toString('utf8');
}
function take(startMarker, endMarker, label) {
  const start = binary.indexOf(startMarker, sourceBase);
  const end = binary.indexOf(endMarker, start + startMarker.length);
  assert(start >= sourceBase && end > start, `Missing native ${label}`);
  return takeBytes(start, end, label);
}
const takeAt = (start, end, label) => takeBytes(sourceBase + start, sourceBase + end, label);
const toolKinds = take('DNo=new Set([', 'async function*LNo(', 'native tool kinds');
const initial = take('nf=!1,Ih=null,L_=!1;', 'function*Um(){', 'native initial state');
const close = take('function*Um(){', 'let Ud=null', 'native close generator');
const observe = take('if(Fs.type==="message_start")nf=!0', 'yield{type:"stream_event",event:Fs', 'native forwarded-event state update');
const start = { type: 'message_start' };
const block = (type, index = 0) => ({ type: 'content_block_start', index, content_block: { type } });
const stop = index => ({ type: 'content_block_stop', index });
const tests = [
  ['no-forwarded-message', []],
  ['message-start-only', [start]],
  ['open-text', [start, block('text', 3)]],
  ['open-thinking', [start, block('thinking', 2)]],
  ['open-redacted-thinking', [start, block('redacted_thinking', 4)]],
  ['open-tool-use', [start, block('tool_use', 1)]],
  ['open-server-tool-use', [start, block('server_tool_use', 2)]],
  ['open-mcp-tool-use', [start, block('mcp_tool_use', 3)]],
  ['completed-text-block', [start, block('text', 1), stop(1)]],
  ['completed-thinking-then-open-text', [start, block('thinking', 0), stop(0), block('text', 1)]],
  ['stop-reason-before-lost-stop', [start, block('text', 0), stop(0), { type: 'message_delta', delta: { stop_reason: 'end_turn' } }]],
  ['already-forwarded-message-stop', [start, block('text', 0), stop(0), { type: 'message_stop' }]],
  ['block-without-message-start', [block('text', 0)]],
  ['new-message-after-real-stop', [start, { type: 'message_stop' }, start, block('text', 7)]],
];
const cases = tests.map(([name, events]) => {
  const native = vm.runInNewContext(`(()=>{var ${toolKinds};let ${initial}${close}
    return {observe(Fs){${observe}},close(){return [...Um()]}};})()`);
  for (const event of events) native.observe(event);
  const first = JSON.parse(JSON.stringify(native.close()));
  const repeated = JSON.parse(JSON.stringify(native.close()));
  assert.deepEqual(repeated, [], `Close is not idempotent: ${name}`);
  return { name, events, expected: { first, repeated,
    eventJson: first.map(row => JSON.stringify(row.event)) } };
});
assert.equal(cases[2].expected.eventJson[0], '{"type":"content_block_stop","index":3}');
assert.deepEqual(cases[5].expected.eventJson, ['{"type":"message_stop"}']);
assert.equal(cases.length, 14);

// These are half-open byte ranges in src_185762130.js, not JS string offsets.
const terminalUpdate = takeAt(2145054, 2145151, 'native final-delta terminal state update');
const cleanPredicates = takeAt(2150387, 2151109, 'native clean-EOF completion predicates');
const errorComplete = takeAt(2157575, 2157980, 'native error close-after-complete branch');
const messageStartState = takeAt(2138360, 2138379, 'native message-start completion state');
const blockStartReset = takeAt(2139125, 2139130, 'native block-start terminal reset');
const openBlockState = takeAt(2140244, 2140255, 'native open-block state');
const blockDeltaReset = takeAt(2140398, 2140663, 'native block-delta terminal reset and guards');
const blockStopReset = takeAt(2142365, 2142756, 'native block-stop terminal reset and guards');
const closedBlockState = takeAt(2142619, 2142626, 'native closed-block state');
const completedBlockState = takeAt(2143571, 2143618, 'native completed-block state');
const messageStopState = takeAt(2149377, 2149382, 'native message-stop completion state');
const errorStopReasonFlag = takeAt(2157313, 2157330, 'native error stop-reason presence flag');
assert(blockStopReset.includes(closedBlockState), 'Native block stop must execute the pinned wa=null assignment');

// Payload validation/conversion is outside this bounded state/predicate VM.
// Native guards, terminal changes, completed-block count changes and predicates
// are executed unchanged. Stubs provide bookkeeping objects and branch inputs.
const completionSource = [
  '(()=>{var ' + toolKinds + 'let ' + initial + close,
  'let Al=false,ef,Au=null,iS=false,wa=null,Bm=[],Ya=new WeakSet(),tp=[],Vv=0,Gb=new Map();',
  'const t=()=>{},i=()=>{},S=x=>x,c=x=>x,Et=x=>x,Hf=x=>x,ye=x=>x,Yvn=x=>x,Nee=()=>({}),pae=Fs=>Fs.index;',
  'const z={model:"fixture-model"},Zi=null,al={},K={monotonicNow:()=>1},Wy=0,vC=null;',
  'class btr extends Error{};class ZGt extends Error{};class F8 extends Error{};',
  'let errorAccepted=false;const uh=kind=>{errorAccepted=kind==="stream_completed"};',
  'return {observe(Fs){switch(Fs.type){',
  'case "message_start":{' + messageStartState + ';break;}',
  'case "content_block_start":{' + blockStartReset + ';Bm[Fs.index]={...Fs.content_block};' + openBlockState + ';break;}',
  'case "content_block_delta":{' + blockDeltaReset + 'break;}',
  'case "content_block_stop":{' + blockStopReset + 'const mp={};' + completedBlockState + ';break;}',
  'case "message_delta":{' + terminalUpdate + 'break;}',
  'case "message_stop":{' + messageStopState + ';break;}',
  'default:throw Error("Uncovered completion event "+Fs.type);}' + observe + '},',
  'snapshot(){return {messageStarted:!!ef,terminalDelta:iS,stopReason:Au,openBlock:wa,completedBlocks:tp.length};},',
  'clean(){try{' + cleanPredicates + 'return {accepted:true,failure:null};}catch(error){',
  'if(error instanceof btr)return {accepted:false,failure:"dropped_connection"};',
  'if(error instanceof ZGt)return {accepted:false,failure:"non_streaming_fallback"};throw error;}},',
  'error(){' + errorStopReasonFlag + ';const Ug=false,ld={code:"StreamTruncated"},p6e="controlled_eligible_connection_error";',
  'errorAccepted=false;function*run(){e:for(let once=0;once<1;once++){' + errorComplete + '}}',
  'const events=[...run()];return {accepted:errorAccepted,eventJson:events.map(row=>JSON.stringify(row.event))};}',
  '};})()',
].join('\n');
const completionStart = { type: 'message_start', message: { id: 'fixture-message' } };
const finalDelta = reason => ({ type: 'message_delta', delta: { stop_reason: reason } });
const usageOnlyDelta = { type: 'message_delta', delta: {}, usage: { output_tokens: 2 } };
const textDelta = index => ({ type: 'content_block_delta', index, delta: { type: 'text_delta', text: 'partial' } });
const inputDelta = index => ({ type: 'content_block_delta', index, delta: { type: 'input_json_delta', partial_json: '{"path":' } });
const closedText = [completionStart, block('text'), textDelta(0), stop(0)];
const completionTests = [
  ['missing-message-start', []],
  ['message-start-without-final-block-or-stop-reason', [completionStart]],
  ['zero-output-final-string-delta', [completionStart, finalDelta('end_turn')]],
  ['missing-message-start-with-final-delta', [finalDelta('end_turn')]],
  ['closed-block-without-final-delta', closedText],
  ['closed-block-with-final-delta', [...closedText, finalDelta('end_turn')]],
  ['open-text-with-final-delta', [completionStart, block('text'), textDelta(0), finalDelta('end_turn')]],
  ['open-tool-block-before-final-delta', [completionStart, block('tool_use'), finalDelta('tool_use')]],
  ['final-delta-before-tool-block-start', [completionStart, finalDelta('tool_use'), block('tool_use')]],
  ['open-tool-input-with-final-delta', [completionStart, block('tool_use'), inputDelta(0), finalDelta('tool_use')]],
  ['open-server-input-with-final-delta', [completionStart, block('server_tool_use'), inputDelta(0), finalDelta('tool_use')]],
  ['open-mcp-block-with-final-delta', [completionStart, block('mcp_tool_use'), finalDelta('tool_use')]],
  ['later-block-start-resets-terminal', [...closedText, finalDelta('end_turn'), block('text', 1)]],
  ['later-block-start-and-stop-reset-terminal', [...closedText, finalDelta('end_turn'), block('text', 1), stop(1)]],
  ['later-block-delta-resets-terminal', [completionStart, block('text'), finalDelta('end_turn'), textDelta(0)]],
  ['later-block-stop-resets-terminal', [completionStart, block('text'), finalDelta('end_turn'), stop(0)]],
  ['usage-only-delta-keeps-terminal', [...closedText, finalDelta('end_turn'), usageOnlyDelta]],
  ['usage-only-delta-does-not-set-terminal', [...closedText, usageOnlyDelta]],
  ['null-stop-reason-does-not-set-terminal', [completionStart, finalDelta(null)]],
  ['number-stop-reason-does-not-set-terminal', [completionStart, finalDelta(7)]],
  ['zero-output-empty-string-final-delta', [completionStart, finalDelta('')]],
  ['closed-block-empty-string-final-delta', [...closedText, finalDelta('')]],
  ['already-forwarded-message-stop-after-final-delta', [...closedText, finalDelta('end_turn'), { type: 'message_stop' }]],
  ['already-forwarded-message-stop-without-final-delta', [...closedText, { type: 'message_stop' }]],
];
const completionCases = completionTests.map(([name, events]) => {
  const native = vm.runInNewContext(completionSource);
  for (const event of events) native.observe(event);
  const state = JSON.parse(JSON.stringify(native.snapshot()));
  const cleanResult = native.clean();
  const errorResult = JSON.parse(JSON.stringify(native.error()));
  assert.deepEqual(JSON.parse(JSON.stringify(native.error())).eventJson, [], 'Duplicate native error close: ' + name);
  return { name, events, expected: {
    complete: cleanResult.accepted && errorResult.accepted,
    cleanAccepted: cleanResult.accepted,
    errorAccepted: errorResult.accepted,
    ...state,
    cleanFailure: cleanResult.failure,
    errorEventJson: errorResult.eventJson,
  } };
});
const completionByName = Object.fromEntries(completionCases.map(row => [row.name, row.expected]));
assert.equal(completionByName['zero-output-final-string-delta'].complete, true);
assert.equal(completionByName['missing-message-start-with-final-delta'].complete, false);
assert.equal(completionByName['open-text-with-final-delta'].complete, false);
assert.equal(completionByName['open-tool-block-before-final-delta'].terminalDelta, true);
assert.equal(completionByName['open-tool-block-before-final-delta'].openBlock, 0);
assert.equal(completionByName['open-tool-block-before-final-delta'].cleanAccepted, false);
assert.equal(completionByName['open-tool-block-before-final-delta'].errorAccepted, false);
assert.equal(completionByName['final-delta-before-tool-block-start'].terminalDelta, false);
assert.equal(completionByName['final-delta-before-tool-block-start'].openBlock, 0);
assert.equal(completionByName['final-delta-before-tool-block-start'].complete, false);
assert.equal(completionByName['later-block-start-and-stop-reset-terminal'].terminalDelta, false);
assert.equal(completionByName['later-block-delta-resets-terminal'].terminalDelta, false);
assert.equal(completionByName['later-block-stop-resets-terminal'].terminalDelta, false);
assert.equal(completionByName['usage-only-delta-keeps-terminal'].complete, true);
assert.equal(completionByName['zero-output-empty-string-final-delta'].cleanAccepted, false);
assert.equal(completionByName['zero-output-empty-string-final-delta'].errorAccepted, true);
assert.deepEqual(completionByName['already-forwarded-message-stop-after-final-delta'].errorEventJson, []);

const classifyStreamFailure = takeAt(2038750, 2039123, 'native DUt stream-failure classification');
const classifyCause = takeAt(2039123, 2039476, 'native HLo stream-failure cause classification');
const classification = vm.runInNewContext([
  '(()=>{', classifyStreamFailure, classifyCause,
  'class xt extends Error{};class Jd extends xt{};class Dd extends xt{};',
  'const RU=error=>error.overloaded===true;',
  'return {classify(input){const classes={xt,Jd,Dd};',
  'const error=input.errorClass==="overloaded"?{overloaded:true}:classes[input.errorClass]?new classes[input.errorClass]():new Error();',
  'const e={...input,error};return {cause:HLo(e),failure:DUt(e)??null};}};})()',
].join('\n'));
const classifierInput = changes => ({
  errorClass: 'unknown', denied: false, isServerError: false, stalled: false,
  isConnectionError: false, truncated: false, malformed: false,
  anyEvent: true, anyBlockFinished: false, anyOutputShown: false,
  stopReasonReceived: true, complete: true, ...changes,
});
const errorEligibilityTests = [
  ['complete-connection-lost-with-zero-output', { isConnectionError: true }],
  ['complete-truncated-with-zero-output', { isConnectionError: true, truncated: true }],
  ['complete-stalled-with-zero-output', { stalled: true }],
  ['complete-overloaded-with-output', { errorClass: 'overloaded', anyBlockFinished: true, anyOutputShown: true }],
  ['complete-server-error-with-output', { isServerError: true, anyBlockFinished: true, anyOutputShown: true }],
  ['complete-overloaded-without-output', { errorClass: 'overloaded' }],
  ['complete-overloaded-with-thinking-only', { errorClass: 'overloaded', anyBlockFinished: true }],
  ['complete-overloaded-with-partial-output', { errorClass: 'overloaded', anyOutputShown: true }],
  ['complete-server-error-without-output', { isServerError: true }],
  ['complete-server-error-with-partial-output', { isServerError: true, anyOutputShown: true }],
  ['complete-malformed-error', { malformed: true }],
  ['complete-bad-request-error', { errorClass: 'xt' }],
  ['complete-dd-error-class', { errorClass: 'Dd' }],
  ['complete-unknown-error', {}],
  ['incomplete-connection-lost', { isConnectionError: true, complete: false }],
  ['denied-error-overrides-connection-lost', { denied: true, isConnectionError: true }],
];
const errorEligibilityCases = errorEligibilityTests.map(([name, changes]) => {
  const input = classifierInput(changes);
  const result = JSON.parse(JSON.stringify(classification.classify(input)));
  return { name, input, expected: {
    bypassesStreamFailurePolicy: result.failure === null,
    ...result,
  } };
});
assert.deepEqual(errorEligibilityCases.filter(row => row.expected.bypassesStreamFailurePolicy).map(row => row.name),
  errorEligibilityTests.slice(0, 5).map(([name]) => name));
const output = {
  version: '2.1.287', native_sha256: nativeSha256, sourceByteRanges,
  scope: 'Exact native Um generator, forwarded-event state update, initial state and DNo tool-kind set. Input forwarded events and close timing are controlled. This proves the helper state and JSON bytes, not native CLI, abort/retraction or every caller branch acceptance.',
  completionScope: 'Exact extracted native terminal updates, reset/guard statements and clean-EOF/error-close-after-complete predicates executed in a VM. Content-block payload validation/conversion, bookkeeping objects, logging, clocks and an eligible connection-error context are controlled. Native wa open/closed assignments execute separately from the forwarded Ih state. complete is the intersection of cleanAccepted and errorAccepted; errorAccepted means only that the selected close-after-complete necessary-condition branch matched. This VM does not execute DUt/HLo or onStreamFailed error eligibility, abort/retraction or native CLI acceptance.',
  errorEligibilityScope: 'Exact native DUt and HLo classifiers executed separately with controlled input fields, RU overload checks and xt/Jd/Dd error-class prototypes. bypassesStreamFailurePolicy means DUt returned undefined: complete connectionLost/truncated/stalled, or complete overloaded/serverError with output progress (both anyBlockFinished and anyOutputShown). Classification is separate from completionCases and does not execute M_.onStreamFailed, native transports, abort/retraction or native CLI acceptance.',
  cases,
  completionCases,
  errorEligibilityCases,
};
const fixture = fileURLToPath(new URL('../../crates/orchestrator/tests/fixtures/partial_stream_close_2_1_287.json', import.meta.url));
const json = JSON.stringify(output, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(readFileSync(fixture, 'utf8'), json);
else writeFileSync(fixture, json);
process.stdout.write(`${process.argv.includes('--check') ? 'checked' : 'wrote'} native 2.1.287 partial-stream close oracle: ${cases.length} close cases, ${completionCases.length} completion cases, ${errorEligibilityCases.length} error eligibility cases\n`);
