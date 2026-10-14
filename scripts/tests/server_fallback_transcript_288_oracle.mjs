#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node server_fallback_transcript_288_oracle.mjs <2.1.288 native binary> [--check]');
const binary = readFileSync(binaryPath);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');

const sourceByteRanges = [];
function take(first, last, offset, label, maxLength = 8000) {
  const start = binary.indexOf(Buffer.from(first), offset);
  const end = binary.indexOf(Buffer.from(last), start + Buffer.byteLength(first));
  assert(start >= offset && end > start && end - start < maxLength, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha256(bytes)});
  return bytes.toString();
}

const contentBlockStop = take(
  'case"content_block_stop":{', 'case"message_delta":{', 189063000,
  'completed assistant block row construction and immediate yield', 1800,
);
const assistantRowLiteral = take(
  'gd={message:{...pf,content:el}', ';if(sPe(qe,gd)', 189063800,
  'Native completed block row literal', 800,
);
const messageDeltaRows = take(
  'let Uc=czt(si.delta)??czt(si);', 'if(Fu!==null&&wE!=="credited")', 189066000,
  'message_delta in-place terminal metadata update for retained rows', 500,
);
const messageDeltaStopState = take(
  'let Lc=typeof si.delta.stop_reason==="string"?si.delta.stop_reason:null;',
  'if(Fu!==null&&wE!=="credited")', 189065000,
  'Native sticky stop_reason and conditional stop_details assignment', 1000,
);
const admittedFallbackHop = take(
  'if(V.serverRefusalFallback===void 0)continue;', 'continue}if(Bu)', 189057000,
  'accepted stream hop and completed row retention/rewrite', 2000,
);
const eventProjection = take(
  'function*Dle(e,s,n){', 'function*swo(', 194386000,
  'assistant event projection from completed transcript row', 3000,
);
const acceptedFallbackEvents = take(
  'if(M.midStream){W=new Set(K.slice(te)', '}continue}if(M.type==="refusal_no_fallback")', 195142000,
  'accepted fallback output events and ordering', 1200,
);
const acceptedAssistantMerge = take(
  'let{apiBlockIndex:tn,apiBlockIndices:to,...ke}=X,We={...ke,message:{...X.message,content:it}}',
  'let bt=!1;', 195150000,
  'Native accepted assistant J merge and conditional supersedes field', 2800,
);
const acceptedAssistantYieldAndRetainedTombstones = take(
  'if(Me!==M&&Me.type==="assistant"&&M.type==="assistant"&&eo===void 0)',
  'yield*Tn()', 195153000,
  'accepted J assistant row yield followed by original-row tombstones', 900,
);
const messageCallback = take(
  'function m9e(e,n){', 'function zde(', 191051000,
  'message callback adapter used by deferred query events', 2200,
);
const transcriptApply = take(
  '_applyMessage=(h)=>', ';applyEvent=', 209335000,
  'assistant append and continuation UUID removal trigger', 3200,
);
const deferredApply = take(
  'Rn=await FZe({engine:this._engine', 'finally{let wn=Rn?.terminal_reason', 209349000,
  'query events are applied through deferred frame callback', 1200,
);
const writerCursor = take(
  'function QLt(e,n,r){', 'function l_n(', 191162000,
  'writer holds rows without terminal stop reason while query is active', 500,
);
const liveWriter = take(
  'async function LE(e,n,r,s,g){', 'async function Hut(', 191215000,
  'live writer parent hint from preceding persisted transcript row', 800,
);
const removeByUuid = take(
  'async performRemoveByUuid(', 'async removeByUuidV5(', 191194000,
  'physical JSONL UUID removal bounds and rewrite behavior', 1400,
);
const transcriptNormalizeAndProject = take(
  'function Oqn(e,n=new Set){', 'async function Wps(', 191330000,
  'Native persisted transcript filter and parent projection', 2200,
);
const insertMessageChain = take(
  'async insertMessageChain(', 'async insertFileHistorySnapshot(', 191203000,
  'Native live JSONL entry envelope construction and insertion order', 3000,
);
const jsonlWriter = take(
  'async function hHe(e,n,r){', 'function Ggs(', 180885000,
  'Native JSONL serializer', 1000,
);
const resumeLut = take(
  'function Lut(', 'function XMr(', 191246000,
  'resume parent-chain reconstruction', 600,
);
const resumeNearestParent = take(
  'var yRr=5000;function _Rr(', 'function Ukn(', 191248000,
  'timestamp fallback candidate selector and 5000ms cutoff', 450,
);

assert(contentBlockStop.includes('message:{...pf,content:el}'));
assert(contentBlockStop.includes('apiBlockIndex:si.index'));
assert(contentBlockStop.includes('type:"assistant",uuid:KY(),timestamp:new Date().toISOString()'));
assert(contentBlockStop.includes('Pp.push(gd)'));
assert(contentBlockStop.includes('Tw.set(si.index,gd)'));
assert(contentBlockStop.includes('yield gd'));
assert(messageDeltaRows.includes('Nl.message.usage=ul'));
assert(messageDeltaRows.includes('Nl.message.stop_reason=Fu'));
assert(messageDeltaRows.includes('Nl.message.stop_details=si.delta.stop_details??null'));
assert(messageDeltaStopState.includes('if(Lc!==null)Fu=Lc,wS=!0'));
assert(messageDeltaStopState.includes('Nl.message.stop_reason=Fu,Lc!==null||Fu===null'));
assert(messageDeltaStopState.includes('Nl.message.stop_details=si.delta.stop_details??null'));
assert(admittedFallbackHop.includes('pf={...pf,model:nd.model}'));
assert(admittedFallbackHop.includes('el.message.model=nd.model'));
assert(eventProjection.includes('message:g.message'));
assert(eventProjection.includes('uuid:g.uuid'));
assert(!eventProjection.includes('parentUuid:'));
assert(acceptedFallbackEvents.indexOf('type:"query_model_change"') < acceptedFallbackEvents.indexOf('type:"tombstone"'));
assert(acceptedFallbackEvents.indexOf('type:"tombstone"') < acceptedFallbackEvents.indexOf('phase:"begin"'));
assert(acceptedFallbackEvents.includes('message:ke,displayOnly:!0'));
assert(acceptedFallbackEvents.includes('replacesUuids:J.originals.map((ke)=>ke.uuid)'));
assert(acceptedAssistantMerge.includes('We={...ke,message:{...X.message,content:it}}'));
assert(acceptedAssistantMerge.includes('We.supersedesUuids=At.flatMap((Xe)=>Ff([Xe]).filter(pre).map((Qt)=>Qt.uuid))'));
assert(acceptedAssistantMerge.includes('Je)We.supersedesUuids='));
assert(acceptedAssistantYieldAndRetainedTombstones.indexOf('yield Me') < acceptedAssistantYieldAndRetainedTombstones.indexOf('type:"tombstone"'));
assert(acceptedAssistantYieldAndRetainedTombstones.includes('for(let xe of X)yield{type:"tombstone",message:xe,displayOnly:!0}'));
assert(messageCallback.includes('s?.(e.message)'));
assert(!messageCallback.includes('displayOnly'));
assert(transcriptApply.includes('remove-by-uuid'));
assert(transcriptApply.includes('this.stream.transcriptRetractedUuids.add(pt)'));
assert(transcriptApply.includes('Me({type:"append",messages:[Pe]}'));
assert(deferredApply.includes('this._frameNotifier.deferred(()=>this.applyEvent(wn,Tr))'));
assert(writerCursor.includes('g.type==="assistant"&&g.message.stop_reason===null'));
assert(liveWriter.includes('if(!ye&&ore(Se)&&!dH(Se.uuid))he=Se.uuid'));
assert(liveWriter.includes('insertMessageChain(V,!1,void 0,he'));
assert(removeByUuid.includes('QS=') || binary.includes(Buffer.from('var QS=65536')));
assert(removeByUuid.includes('h>jYe'));
assert(removeByUuid.includes('Skipping tombstone removal'));
assert(transcriptNormalizeAndProject.includes('function DHo(e,n){let r=null;return u9e(e.slice()).map((s)=>{let g={...s'));
assert(transcriptNormalizeAndProject.includes('function u9e(e,n=e){let r=e.filter(RS);if(uqn()==="ant")return r'));
assert(insertMessageChain.includes('let qe={parentUuid:Ge?null:Ze,logicalParentUuid:Ge?ke:void 0,isSidechain:n,teamName:g?.teamName,agentName:g?.agentName,promptId:Ne.type==="user"?zve()??void 0:void 0,agentId:r,...Ne,...dqn(Te),version:lkn(Ne),gitBranch:ve,slug:xe}'));
assert(jsonlWriter.includes('JSON.stringify(a)+`\n`'));
assert(resumeLut.includes('if(!M||g.has(M.uuid)){if(M=_Rr(e,h,g),M)i("tengu_chain_timestamp_fallback",{})}'));
assert(resumeNearestParent.includes('var yRr=5000;'));
assert(resumeNearestParent.includes('b.isSidechain!==n.isSidechain'));
assert(resumeNearestParent.includes('M>=0&&M<=yRr&&M<h'));

// Execute the exact completed-row literal with fixed UUID/time injection.
const fixedNow = 1700000000000;
const at = ms => new Date(ms).toISOString();
const FixedDate = class extends Date {
  constructor(...args) { super(...(args.length > 0 ? args : [fixedNow])); }
};
const producerVm = vm.createContext({
  pf: {id: 'provider-inner-id', model: 'initial-model', role: 'assistant'},
  el: [{type: 'text', text: 'hello'}],
  Ba: undefined, Lc: [], Uc: undefined, Jd: undefined, Jm: undefined,
  TOe: value => value, si: {index: 7}, ra: 'request-7', C3: 'request-ref-7',
  lQ: () => ({}),
  V: {querySource: 'repl_main_thread', spawnedBySkill: undefined, activeSkill: undefined, activeMcpServer: undefined, activeMcpTool: undefined},
  KY: () => 'outer-row-uuid-7', rn: undefined, q_: undefined, kE: undefined, FM: undefined,
  Date: FixedDate,
});
vm.runInContext(assistantRowLiteral, producerVm);
const producedRow = JSON.parse(JSON.stringify(producerVm.gd));
assert.equal(producedRow.uuid, 'outer-row-uuid-7');
assert.equal(producedRow.timestamp, new Date(fixedNow).toISOString());
assert.equal(producedRow.message.id, 'provider-inner-id');
assert.equal(producedRow.message.model, 'initial-model');
assert.equal(producedRow.apiBlockIndex, 7);
assert.equal(Object.hasOwn(producedRow, 'parentUuid'), false);
assert.deepEqual(producedRow.message.content, [{type: 'text', text: 'hello'}]);

// Execute the exact MessageDelta loop against multiple completed rows.
const deltaUsage = {input_tokens: 31, output_tokens: 4};
const deltaVm = vm.createContext({
  czt: () => undefined,
  Pp: [{message: {}}, {message: {}}, {message: {model: 'row-model'}}],
  ul: deltaUsage,
  Fu: 'end_turn',
  Lc: [],
  si: {delta: {stop_details: 'provider-stop-details'}},
  Tye: false,
  aF: undefined,
});
vm.runInContext(messageDeltaRows, deltaVm);
const deltaRows = JSON.parse(JSON.stringify(deltaVm.Pp));
for (const row of deltaRows) {
  assert.deepEqual(row.message.usage, deltaUsage);
  assert.equal(row.message.stop_reason, 'end_turn');
  assert.equal(row.message.stop_details, 'provider-stop-details');
}

// Execute Native's exact MessageDelta terminal-field fragment across raw
// absent/null/string stop-reason cases. Fu is sticky unless a string arrives;
// details clear only with a string terminal reason or while Fu remains null.
function runStopState(previousStopReason, delta) {
  const stateVm = vm.createContext({
    Fu: previousStopReason,
    wS: false,
    c1: undefined,
    si: {delta},
    czt: () => undefined,
    ul: {input_tokens: 5, output_tokens: 1},
    Pp: [{message: {stop_details: previousStopReason === 'refusal' ? {category: 'cyber'} : null}}],
    Tye: false,
    aF: undefined,
  });
  vm.runInContext(messageDeltaStopState, stateVm);
  return JSON.parse(JSON.stringify({
    Fu: stateVm.Fu,
    stopReason: stateVm.Pp[0].message.stop_reason,
    stopDetails: stateVm.Pp[0].message.stop_details,
  }));
}
const stickyNoReason = runStopState('refusal', {stop_reason: null});
assert.equal(stickyNoReason.Fu, 'refusal');
assert.equal(stickyNoReason.stopReason, 'refusal');
assert.deepEqual(stickyNoReason.stopDetails, {category: 'cyber'});
const preTerminalDetails = runStopState(null, {stop_details: {category: 'bio'}});
assert.equal(preTerminalDetails.Fu, null);
assert.equal(preTerminalDetails.stopReason, null);
assert.deepEqual(preTerminalDetails.stopDetails, {category: 'bio'});
const newTerminalClearsDetails = runStopState('refusal', {stop_reason: 'end_turn'});
assert.equal(newTerminalClearsDetails.Fu, 'end_turn');
assert.equal(newTerminalClearsDetails.stopReason, 'end_turn');
assert.equal(newTerminalClearsDetails.stopDetails, null);

// Execute Native's assistant replacement construction, supersedes assignment,
// transcript projection, insert envelope construction, and JSONL JSON stringify.
const incomingRow = {
  type: 'assistant', timestamp: at(fixedNow),
  message: {id: 'provider-inner-id', model: 'served-model', content: [{type: 'text', text: 'answer'}], stop_reason: 'end_turn'},
  apiBlockIndex: 0, apiBlockIndices: [0], requestId: 'request-8', requestRef: 'request-ref-8',
  uuid: 'incoming-row-uuid', querySource: 'repl_main_thread',
};
const mergeVm = vm.createContext({X: incomingRow, it: incomingRow.message.content});
vm.runInContext(`${acceptedAssistantMerge.slice(0, acceptedAssistantMerge.indexOf(';if(Z.push'))};globalThis.mergeKeysResult=We;`, mergeVm);
const mergedAssistantRow = mergeVm.mergeKeysResult;
assert.equal(mergedAssistantRow.uuid, 'incoming-row-uuid');
assert.equal(Object.hasOwn(mergedAssistantRow, 'apiBlockIndex'), false);
const supersedesAssignment = acceptedAssistantMerge.slice(
  acceptedAssistantMerge.indexOf('We.supersedesUuids='),
  acceptedAssistantMerge.indexOf(',i("tengu_refusal_fallback_supersedes"'),
) + ';';
const supersedesVm = vm.createContext({
  We: mergedAssistantRow,
  At: [{uuid: 'retained-row-uuid'}],
  Ff: rows => rows,
  pre: () => true,
});
vm.runInContext(supersedesAssignment, supersedesVm);
assert.deepEqual(JSON.parse(JSON.stringify(mergedAssistantRow.supersedesUuids)), ['retained-row-uuid']);

const persistVm = vm.createContext({
  RS: () => true,
  uqn: () => 'external',
  Oqn: () => new Set(),
  cc: '__unused_tool_name__',
  jm: () => '/project',
  akn: () => '2.1.288',
  ore: row => row.type !== 'progress',
});
vm.runInContext(transcriptNormalizeAndProject, persistVm);
const transcriptProjection = persistVm.DHo([mergedAssistantRow], 'session-8');
assert.equal(transcriptProjection.length, 1);
assert.deepEqual(JSON.parse(JSON.stringify(transcriptProjection[0].supersedesUuids)), ['retained-row-uuid']);

const insertVm = vm.createContext({
  q: () => 'session-8',
  gdn: () => true,
  Qi: value => value,
  Fut: () => null,
  oye: () => ({}),
  ad: async () => 'branch',
  rFr: () => {},
  nge: () => new Map([['session-8', 'slug-8']]),
  ii: () => false,
  dqn: () => ({}),
  lkn: () => '2.1.288',
  ore: row => row.type !== 'progress',
  fdn: () => null,
});
vm.runInContext(`async function insertMessageChain${insertMessageChain.slice('async insertMessageChain'.length)}`, insertVm);
const jsonlEntries = [];
const fakeWriter = {
  sessionFile: '/session.jsonl',
  trackWrite: callback => callback(),
  store: {sessionMessages: async () => new Set()},
  shouldSkipPersistence: () => false,
  appendEntry: async entry => { jsonlEntries.push(JSON.parse(JSON.stringify(entry))); },
  currentSessionLeafTs: null,
};
await insertVm.insertMessageChain.call(
  fakeWriter, transcriptProjection, false, undefined, null, undefined,
  'session-8', true, undefined, null, undefined,
);
assert.equal(jsonlEntries.length, 1);
const persistedEntry = jsonlEntries[0];
assert.deepEqual(JSON.parse(JSON.stringify(persistedEntry.supersedesUuids)), ['retained-row-uuid']);
assert.deepEqual(
  Object.keys(persistedEntry),
  ['parentUuid', 'isSidechain', 'type', 'timestamp', 'message', 'requestId', 'requestRef', 'uuid', 'querySource', 'supersedesUuids', 'cwd', 'userType', 'sessionId', 'version', 'gitBranch', 'slug'],
);
assert(Object.keys(persistedEntry).indexOf('supersedesUuids') < Object.keys(persistedEntry).indexOf('version'));
const serializedJsonlRecord = JSON.stringify(persistedEntry);
assert(serializedJsonlRecord.indexOf('"supersedesUuids"') < serializedJsonlRecord.indexOf('"version"'));

// Execute Native LE twice to determine whether changed content/model for an
// already-persisted UUID is upserted. Session membership is the writer's gate.
const persistedUuids = new Set();
const writerCalls = [];
const liveWriterVm = vm.createContext({
  u9e: rows => rows,
  q: () => 'session-1',
  gdn: () => null,
  Qi: value => value,
  Fut: () => null,
  oye: () => null,
  ol: () => ({sessionMessages: async () => persistedUuids}),
  ore: row => row.type !== 'progress',
  Fa: () => false,
  i: () => {},
  dH: () => false,
  fi: () => ({insertMessageChain: async rows => {
    writerCalls.push(JSON.parse(JSON.stringify(rows)));
    for (const row of rows) persistedUuids.add(row.uuid);
  }}),
});
vm.runInContext(liveWriter, liveWriterVm);
const writtenThenChanged = {
  uuid: 'retained-row-uuid',
  type: 'assistant',
  timestamp: at(fixedNow),
  message: {id: 'provider-inner-id', model: 'first-provider', content: [{type: 'text', text: 'prefix'}]},
};
await liveWriterVm.LE([writtenThenChanged], null, undefined, undefined, undefined);
writtenThenChanged.message.model = 'second-provider';
writtenThenChanged.message.content = [{type: 'text', text: 'rewritten prefix'}];
await liveWriterVm.LE([writtenThenChanged], null, undefined, undefined, undefined);
assert.equal(writerCalls.length, 1);
assert.equal(writerCalls[0][0].message.model, 'first-provider');
assert.equal(writerCalls[0][0].message.content[0].text, 'prefix');

// Execute the exact Native chain walker/selector. Downstream sibling expansion is
// stubbed so this oracle isolates parent-chain recovery and its candidate choice.
const chainVm = vm.createContext({
  c: () => {},
  i: () => {},
  kRr: (_entries, chain) => chain,
  hRr: (_last, chain) => chain,
  Bkn: () => new Map(),
  x8t: () => {},
});
vm.runInContext(`${resumeNearestParent}${resumeLut}`, chainVm);
function chain(rows, lastUuid) {
  const entries = new Map(rows.map(row => [row.uuid, row]));
  return Array.from(chainVm.Lut(entries, entries.get(lastUuid), undefined), row => row.uuid);
}
const chainCases = [];
function addCase(name, rows, lastUuid, expected) {
  const actual = chain(rows, lastUuid);
  assert.deepEqual(actual, expected, name);
  chainCases.push({name, input: rows, lastUuid, output: actual});
}

addCase('nearest-prior-any-row-type-on-missing-parent', [
  {uuid: 'old-progress', type: 'progress', timestamp: at(1000), isSidechain: false},
  {uuid: 'latest', type: 'assistant', timestamp: at(2000), isSidechain: false, parentUuid: 'missing'},
], 'latest', ['old-progress', 'latest']);
addCase('exactly-5000ms-is-included', [
  {uuid: 'edge', type: 'system', timestamp: at(1000), isSidechain: false},
  {uuid: 'latest', type: 'assistant', timestamp: at(6000), isSidechain: false, parentUuid: 'missing'},
], 'latest', ['edge', 'latest']);
addCase('5001ms-is-outside-window', [
  {uuid: 'too-old', type: 'assistant', timestamp: at(999), isSidechain: false},
  {uuid: 'latest', type: 'assistant', timestamp: at(6000), isSidechain: false, parentUuid: 'missing'},
], 'latest', ['latest']);
addCase('sidechain-must-match-exactly', [
  {uuid: 'main-row', type: 'assistant', timestamp: at(1500), isSidechain: false},
  {uuid: 'side-row', type: 'assistant', timestamp: at(1800), isSidechain: true},
  {uuid: 'latest', type: 'assistant', timestamp: at(2000), isSidechain: false, parentUuid: 'missing'},
], 'latest', ['main-row', 'latest']);
addCase('cycle-recovers-through-nearest-unvisited-prior-row', [
  {uuid: 'prior', type: 'user', timestamp: at(1000), isSidechain: false},
  {uuid: 'a', type: 'assistant', timestamp: at(2000), isSidechain: false, parentUuid: 'b'},
  {uuid: 'b', type: 'assistant', timestamp: at(3000), isSidechain: false, parentUuid: 'a'},
], 'b', ['prior', 'a', 'b']);
addCase('equal-distance-tie-keeps-first-map-candidate', [
  {uuid: 'first', type: 'assistant', timestamp: at(1000), isSidechain: false},
  {uuid: 'second', type: 'assistant', timestamp: at(1000), isSidechain: false},
  {uuid: 'latest', type: 'assistant', timestamp: at(2000), isSidechain: false, parentUuid: 'missing'},
], 'latest', ['first', 'latest']);
addCase('twenty-row-chain-recovers-across-one-missing-link', Array.from({length: 20}, (_, index) => ({
  uuid: `row-${index}`,
  type: index % 3 === 0 ? 'progress' : index % 3 === 1 ? 'user' : 'assistant',
  timestamp: at(10000 + index * 500),
  isSidechain: false,
  ...(index > 0 ? {parentUuid: index === 10 ? 'deleted-row' : `row-${index - 1}`} : {}),
})), 'row-19', Array.from({length: 20}, (_, index) => `row-${index}`));

const data = {
  version: '2.1.288',
  native_sha256: sha256(binary),
  sourceByteRanges,
  scope: 'Source-anchored transcript lifecycle facts plus actual Native resume chain walker/nearest-parent selection. Parent chain is reconstructed for the returned transcript only; this oracle does not infer producer parentUuid, model real async filesystem scheduling, or prove host bridge/UI atomicity.',
  transcriptFacts: {
    assistantRowUuidCreatedAt: 'content_block_stop',
    outerUuid: 'fresh KY() per completed API content block',
    innerMessageIdAndModel: 'spread from current MessageStart object pf',
    parentUuidAtProducer: 'absent',
    messageDelta: 'mutates usage and sticky stop_reason on every row still in Pp; stop_details follows the conditional stop-state rule below; row identity/model are not changed by the delta loop',
    messageDeltaStopState: 'a string stop_reason replaces sticky Fu; absent/null/non-string reason leaves Fu unchanged; stop_details updates to delta value or null only when this delta has a string reason or Fu is still null',
    persistedParent: 'live insertMessageChain assigns parent from prior loggable row; LE seeds from preceding row already in sessionMessages',
    acceptedFallbackEvents: ['query_model_change', 'discarded row tombstones', 'compensation row tombstones', 'optional refusal_continuation begin'],
    jAcceptedReplacementEvents: ['eligible replacement assistant row', 'tombstones for J.originals in source order'],
    acceptedFallbackTombstone: 'displayOnly is stripped by callback adapter; Native transcript removal and JSONL remove-by-UUID are requested',
    sameUuidMutation: 'LE skips rows whose UUID is already in sessionMessages; changed model/content is not appended or upserted by a later LE pass',
    supersedesPersistence: 'when Je is true, We receives top-level supersedesUuids; Native DHo and insertMessageChain preserve it, and JSON.stringify writes it in insertion order before writer-added version/gitBranch/slug fields',
    removalReparenting: 'physical UUID removal does not reparent surviving rows; missing-parent resume reconstruction may attach nearest earlier row in returned chain only',
  },
  producerExecutions: {
    completedRow: producedRow,
    messageDeltaRows: deltaRows,
    messageDeltaStopState: {stickyNoReason, preTerminalDetails, newTerminalClearsDetails},
    changedAlreadyPersistedRowWrites: writerCalls,
    supersedesRow: mergedAssistantRow,
    persistedJsonlEntry: persistedEntry,
    serializedJsonlRecord,
  },
  chainCases,
};
const output = JSON.stringify(data, null, 2) + '\n';
const path = new URL('../../crates/orchestrator/tests/fixtures/server_fallback_transcript_2_1_288.json', import.meta.url);
if (process.argv.includes('--check')) assert.equal(readFileSync(path, 'utf8'), output);
else writeFileSync(path, output);
console.log(JSON.stringify({sourceRanges: sourceByteRanges.length, chainCases: chainCases.length}));
