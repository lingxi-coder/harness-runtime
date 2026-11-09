#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import fs from 'node:fs';
import vm from 'node:vm';

const path = process.argv[2];
assert(path, 'Usage: node partial_stream_319_oracle.mjs <official 2.1.319 binary> [--check]');
const binary = fs.readFileSync(path);
const nativeSha256 = '1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd';
assert.equal(createHash('sha256').update(binary).digest('hex'), nativeSha256);
const sourceByteRanges = [];
function take(name, start, end) {
  const begin = binary.indexOf(start, 259144680);
  const finish = binary.indexOf(end, begin + Buffer.byteLength(start));
  assert(begin >= 259144680 && finish > begin, name);
  const bytes = binary.subarray(begin, finish);
  sourceByteRanges.push({name, start: begin, endExclusive: finish, sha256: createHash('sha256').update(bytes).digest('hex')});
  return bytes.toString('utf8');
}
const kinds = take('tool kinds', 'VLr=new Set(["tool_use"', 'async function*YLr(');
const initial = take('forwarded close initial state', 'GT=!1,kR=null,hv=!1,Ak=null,Vf=0;', 'function*cg(');
const close = take('native close generator', 'function*cg({responseUnfinished:Ru})', 'let zT=null');
const observe = take('forwarded event identity and open-block state', 'if(va.type==="message_start")GT=!0', 'yield{type:"stream_event",event:va');
const completed = take('first abandoned block index', 'Vf=Math.max(Vf,Number.isSafeInteger(va.index)', 'Hu.add(Uu)').replace(/,$/, ';');
const exportFrame = take('native public partial frame builder', 'function Cg(e,n){return{type:"stream_event"', 'function YS(e)');
const display = take('native thinking-display partial-frame gate', 'function Gbn(e,n)', 'var zbn=');
const api = vm.runInNewContext(`(()=>{
  var ${kinds}; ${exportFrame}; ${display};
  return {state(){let ${initial}${close}
    return {observe(va){${observe}},completed(index){const va={index};${completed}},
      close(unfinished){return [...cg({responseUnfinished:unfinished})]}};},
    exportFrame:Cg, display:Gbn};
})()`);
const start = id => ({type: 'message_start', message: {id}});
const block = (type, index=0) => ({type: 'content_block_start', index, content_block: {type}});
const stop = index => ({type: 'content_block_stop', index});
const cases = [
  ['no-message', [], true],
  ['message-start-only', [start('msg-1')], true],
  ['open-text', [start('msg-1'), block('text', 3)], true],
  ['open-thinking', [start('msg-1'), block('thinking', 2)], true],
  ['open-tool-use', [start('msg-1'), block('tool_use', 1)], true],
  ['open-server-tool', [start('msg-1'), block('server_tool_use', 1)], true],
  ['open-mcp-tool', [start('msg-1'), block('mcp_tool_use', 1)], true],
  ['closed-block-then-open-text', [start('msg-1'), block('text', 2), stop(2), block('text', 7)], true],
  ['complete-response-with-lost-message-stop', [start('msg-1'), block('text'), stop(0)], false],
  ['already-forwarded-message-stop', [start('msg-1'), {type: 'message_stop'}], true],
  ['empty-message-id', [start(''), block('text')], true],
  ['new-message-after-real-stop', [start('msg-1'), stop(5), {type: 'message_stop'}, start('msg-2'), block('text')], true],
].map(([name, events, unfinished]) => {
  const state = api.state();
  for (const event of events) {
    state.observe(event);
    if (event.type === 'content_block_stop') state.completed(event.index);
  }
  const closed = state.close(unfinished);
  assert.equal(state.close(unfinished).length, 0);
  return {name, events, unfinished, expected: JSON.parse(JSON.stringify(closed)),
    expectedPublicJson: closed.map(row => JSON.stringify(api.exportFrame(row, {session_id: 'session', uuid: 'frame'})))};
});
const rawEnvelopeCases = [
  // The complete message_start shape and usage presence/order match the
  // unmodified 2.1.319 loopback capture (live-malformed-r4).
  '{"type":"message_start","message":{"id":"msg_oracle_1","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}',
  '{ "type":"content_block_delta", "index":0, "delta":{"type":"text_delta","text":"A\\ud800B"}, "future":{"2":-0,"1":1e2,"z":1e400,"a":9007199254740993} }',
];
const envelopeCases = [
  {event: start('msg-1'), apiMessageId: 'msg-1', ttftMs: 17, thinkingDisplay: 'omitted'},
  {event: {type: 'message_stop'}, apiMessageId: 'msg-1', abandonedBlocks: {apiMessageId: 'msg-1', fromBlockIndex: 3}},
  {event: {type: 'content_block_delta', index: 0, delta: {type: 'text_delta', text: 'A\ud800B'}}, apiMessageId: 'msg-1', textRunsJoined: true},
  {event: {type: 'message_stop'}},
].map(input => ({input, expectedJson: JSON.stringify(api.exportFrame(input, {session_id: 'session', uuid: 'frame'}))}));
for (const eventSource of rawEnvelopeCases) {
  const input = {event: JSON.parse(eventSource), apiMessageId: 'msg_oracle_1'};
  envelopeCases.push({input, eventSource, expectedJson: JSON.stringify(api.exportFrame(input, {session_id: 'session', uuid: 'frame'}))});
}
const sourceIdentityCases = [
  ['lone-high-surrogate-id', 'msg-\ud800'],
  ['literal-replacement-id', 'msg-\ufffd'],
].map(([name, id]) => {
  const events = [start(id), block('text', 2)];
  const state = api.state();
  for (const event of events) state.observe(event);
  const closed = state.close(true);
  const abandoned = {apiMessageId: id, fromBlockIndex: 0};
  const frame = {event: {type: 'message_stop'}, apiMessageId: id, abandonedBlocks: abandoned};
  // Source strings keep the JSON escape, so no intermediate Value parser can
  // turn a lone code unit into the literal replacement character.
  envelopeCases.push({input: {}, eventSource: JSON.stringify(frame.event),
    apiMessageIdSource: JSON.stringify(id), abandonedBlocksSource: JSON.stringify(abandoned),
    expectedJson: JSON.stringify(api.exportFrame(frame, {session_id: 'session', uuid: 'frame'}))});
  return {name, eventsSource: JSON.stringify(events), expectedInternalJson: JSON.stringify(closed),
    expectedPublicJson: closed.map(row => JSON.stringify(api.exportFrame(row, {session_id: 'session', uuid: 'frame'})))};
});
assert.notEqual(sourceIdentityCases[0].expectedInternalJson, sourceIdentityCases[1].expectedInternalJson);
const displayCases = [start('msg-1'), block('thinking'), block('text'),
  {type: 'content_block_delta', index: 0, delta: {type: 'thinking_delta', thinking: 'reason'}},
  {type: 'content_block_delta', index: 0, delta: {type: 'text_delta', text: 'answer'}},
  {type: 'message_stop'},
].map(event => ({event, expected: api.display(event, 'omitted') ?? null}));
const result = {version: '2.1.319', native_sha256: nativeSha256, sourceByteRanges,
  scope: 'Verbatim native forwarded close state, close generator, accepted-block index update, public partial frame builder and thinking-display event classifier. Accepted content-block stops, host timing/display mode, ids and session envelope are controlled. This source evaluation does not execute transport/parser validation.',
  cases, envelopeCases, displayCases, sourceIdentityCases};
const fixture = new URL('../../crates/orchestrator/tests/fixtures/partial_stream_2_1_319.json', import.meta.url);
const encoded = JSON.stringify(result, null, 2) + '\n';
if (process.argv.includes('--check')) assert.equal(fs.readFileSync(fixture, 'utf8'), encoded);
else fs.writeFileSync(fixture, encoded);
console.log(`${process.argv.includes('--check') ? 'checked' : 'wrote'} ${cases.length} native close cases, ${envelopeCases.length} public envelopes and ${displayCases.length} display gates`);
