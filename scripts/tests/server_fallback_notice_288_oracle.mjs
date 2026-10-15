#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binaryPath = process.argv[2];
assert(binaryPath, 'usage: node server_fallback_notice_288_oracle.mjs <2.1.288 native binary> [--check]');
const binary = readFileSync(binaryPath);
const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
assert.equal(sha256(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges = [];
function take(first, last, offset, label, maxLength = 6000) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start && end - start < maxLength, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha256(bytes)});
  return bytes.toString();
}

const feedbackAvailability = take(
  'function Hre(e="/feedback")', 'function $4n(', 185871000,
  'native feedback availability and QA footer gate', 1000,
);
const fablePrefix = take('function uIe(o)', 'function Sqo(', 182835000, 'native Fable canonical-prefix predicate', 200);
const fableAlias = take('function Pi(e){return e==="fable"', 'var fw=', 185893000, 'native Fable alias predicate', 200);
const defaultFable = take(
  'function K8(e){let n=a.ANTHROPIC_DEFAULT_FABLE_MODEL', 'function Ny(', 181764000,
  'native default Fable model predicate', 250,
);
const helpUrls = take(
  'var b$r="https://support.claude.com/en/articles/16049681"', 'function _w(', 185904000,
  'native refusal fallback help URLs', 250,
);
const noticeHelpers = take(
  'function _w(e)', 'function P1e()', 185904000,
  'native refusal fallback notice text and model-trait helpers', 5000,
);
const cyberBioPredicate = take('function Kpn(e)', 'function l9(', 185898000, 'native cyber and bio category predicate', 100);
const visibleReason = take('function Tle(e)', 'function _No(', 188771000, 'native user-visible reason predicate', 500);
const serverControl = take('function oHo(e,n)', 'function sHo(', 188775250, 'native accepted fallback control decision', 800);
const modelIdentity = take('function Mn(e){return e.replace', 'function Gy(', 179326700, 'native context-tag model identity', 500);
const receivedModel = take('function Ucn(e,n)', 'function jHt(', 188774000, 'native declared and received model preference', 500);
const noticeProducer = take('function sHo(e,n,r)', 'function MVn(', 188776000, 'native sHo notice producer', 1000);
const stitchHelper = take('var N9=80,uI=1e4;', 'function jFo(', 185910000, 'native H$r local text stitching helper', 3000);
const acceptedFallback = take(
  'if(M.midStream){W=new Set(K.slice(te)', '}continue}if(M.type==="refusal_no_fallback")', 195142000,
  'native accepted server-fallback seam and notice branch', 3500,
);
const nativeServerFallbackVisibleGate = take(
  'if(M.type==="server_fallback"){let X=oHo(M,{isMainThread:Je,emitsLocalScope:Is})',
  'if(!(uA(M.toModel)', 195138000,
  'native server-fallback user-visible outer gate', 1000,
);
const nativeServerFallbackStitchSeed = take(
  'if(M.midStream){W=new Set(K.slice(te)', 'ne.push(...M.discardedMessages,...to)',
  195142000, 'native server-fallback J seed assignment', 500,
);
const acceptedAssistantMerge = take(
  'if((io!==void 0||J!==void 0)&&M.type==="assistant"', 'let bt=!1;', 195150000,
  'native accepted assistant stitch and message identity branch', 4500,
);
const nativeAssistantCandidateAssignment = take(
  'let Me=M,eo,Ut=!1,_o;', 'if(M.type==="assistant"){', 195150000,
  'native current assistant candidate assignment', 200,
);
const nativeUtf16Prefix = take(
  'function re(t,n){if(n<=0)return"";', 'function sc(t,n)', 178910000,
  'native UTF-16-safe bounded prefix helper', 300,
);
const nativeUtf16RoundTrip = take(
  'function f(t){if(typeof Buffer<"u")', 'function x(t)', 178911000,
  'native UTF-16 round-trip helper', 200,
);
const nativePi = take(
  'function pi(e,n){let r=Uee(e)', 'function xA(', 178919000,
  'native sanitized-text cap wrapper', 300,
);
const nativeDetailsSanitizer = take(
  'function k1e(e)', 'var LFo=', 185906000,
  'native refusal-category details renderer', 200,
);
const noticeQueueFlush = take(
  'function*He(){let Z=Q', 'let Gt,se;', 195131000,
  'native queued fallback notice flush helper', 400,
);
const refusalDeclineFlush = take(
  'if(M.type==="refusal_no_fallback"){', 'if(M.type==="fallback_request")', 195143000,
  'native refusal-decline queued notice flush', 3000,
);
const nativeLocalStitchOccupancy = take(
  'yield{type:"refusal_continuation",phase:"begin",salvageText:Qt},io=Qt,Lo=!0', ';', 195148000,
  'native local fallback stitch occupancy', 200,
);
const nativeJeDefinition = take(
  'fe=b.startsWith("repl_main_thread")||b==="sdk",ot=fe&&!e.toolUseContext.agentId',
  'if(fe)try', 195111000, 'native main-query Je source predicate', 200,
);
const nativeJeAssignment = take(
  'mn=un(),Je=fe', ',Is=b==="side_question"', 195114000,
  'native Je assignment', 100,
);
const nativePreRowFilter = take(
  'function pre(e){', 'var yH=24;', 191003000,
  'native supersedes row eligibility filter', 500,
);
const nativeRowUuidHelpers = take(
  'function TNt(e,n){', 'class Fhn', 191003000,
  'native per-content-block UUID derivation helpers', 600,
);
const nativeFfNormalize = take(
  'function Ff(e,n=!1,r){', 'function BEr(', 191004000,
  'native original-message row expansion dispatcher', 1000,
);
const nativeAssistantRowExpansion = take(
  'function BEr(e,n){', 'function IDr(', 191004000,
  'native assistant per-content-block row expansion', 2500,
);
const normalTerminalFlush = take(
  'if(W!==void 0&&(K.at(-1)?.message.stop_reason??kn)==="refusal")', 'let M=K.at(-1)', 195154000,
  'native normal-terminal queued notice flush', 1200,
);
const errorCatchFlush = take(
  'if(Gt!==void 0||se!==void 0){', 'if(DVn(A,Z))', 195155000,
  'native query-error queued notice flush', 2000,
);
const nativeFallbackEventDispatch = take(
  'if(We.type==="system"&&We.subtype==="model_refusal_fallback"){X(QZt(We));continue}',
  'if(We.type==="system"&&We.subtype==="model_refusal_no_fallback")', 209302000,
  'native engine fallback-event dispatch', 150,
);
const nativeFallbackRowMapping = take(
  'function QZt(e){return{type:"system",subtype:"model_refusal_fallback"', 'function ZZt(', 194373000,
  'native fallback-event to transcript-row mapping', 1000,
);
const nativeMessageEventAdapter = take(
  'function m9e(e,n){', 'function zde(', 191051000,
  'native event-to-message callback adapter', 2000,
);
const nativeApplyEvent = take(
  'applyEvent=(h,v=null)=>', ';_noteTextBlockShown(', 209338000,
  'native query-event transcript application', 4000,
);
const nativeApplyMessage = take(
  '_applyMessage=(h)=>', ';applyEvent=', 209335000,
  'native message append to live transcript', 3000,
);
const nativeQueryEventCallback = take(
  'Rn=await FZe({engine:this._engine', 'finally{let wn=Rn?.terminal_reason', 209349000,
  'native engine callback applies query events', 1000,
);
const nativePersistFilter = take(
  'function RS(e){if(e.type==="progress")', 'function Oqn(', 191330000,
  'native transcript-entry persistence filter', 500,
);
const nativeTranscriptProjection = take(
  'function DHo(e,n){', 'async function Wps(', 191332000,
  'native transcript-to-JSONL row projection', 400,
);
const nativeJsonlWriter = take(
  'async function hHe(e,n,r){', 'function Ggs(', 180885000,
  'native JSONL transcript row writer', 1000,
);
const nativeTranscriptProjectionExport = take(
  'DHo as toTranscriptEntries', 'QLt as transcriptCursorEnd', 198852000,
  'native transcript projection export name', 200,
);
const nativeJsonlWriterExport = take(
  'hHe as writeEntriesToJsonlFile', '};', 210259000,
  'native JSONL writer export name', 80,
);

assert(nativeDetailsSanitizer.includes('maxCodeUnits:SGe'));
assert(nativePi.includes('re(r,n.maxCodeUnits).trim()'));
assert(nativeUtf16Prefix.includes('r>=55296&&r<=56319?e.slice(0,-1):e'));
assert(refusalDeclineFlush.includes('yield*He()'));
assert(nativeLocalStitchOccupancy.includes('io=Qt'));
assert(normalTerminalFlush.includes('yield*He()'));
assert(errorCatchFlush.includes('yield*He()'));
assert(nativeFallbackEventDispatch.includes('X(QZt(We));continue'));
assert(nativeFallbackRowMapping.includes('...e.scope!==void 0&&{scope:e.scope}'));
assert(nativeMessageEventAdapter.includes('b?.(()=>null),r(e);return'));
assert(nativeApplyEvent.includes('onMessage:this._applyMessage'));
assert(nativeApplyMessage.includes('Me({type:"append",messages:[Pe]}'));
assert(nativeQueryEventCallback.includes('this._frameNotifier.deferred(()=>this.applyEvent(wn,Tr))'));
assert(nativePersistFilter.includes('e.type==="system"&&"persist"in e&&e.persist===!1'));
assert(nativePersistFilter.includes('return!0'));
assert(nativeTranscriptProjection.includes('return u9e(e.slice()).map'));
assert(nativeJsonlWriter.includes('JSON.stringify(a)+`\n`'));
assert(nativeTranscriptProjectionExport.includes('toTranscriptEntries'));
assert(nativeJsonlWriterExport.includes('writeEntriesToJsonlFile'));
assert(nativeAssistantCandidateAssignment.includes('let Me=M'));
assert(acceptedAssistantMerge.includes('apiBlockIndex:tn,apiBlockIndices:to,...ke'));
assert(nativeJeDefinition.includes('b.startsWith("repl_main_thread")||b==="sdk"'));
assert(nativeJeAssignment.includes('Je=fe'));
assert(acceptedAssistantMerge.includes('ne.push(...At),Je)We.supersedesUuids=At.flatMap'));
assert(nativeFfNormalize.includes('BEr(h,b)'));
assert(nativeAssistantRowExpansion.includes('e.message.content.map'));
assert(nativeAssistantRowExpansion.includes('uuid:h'));
assert(nativePreRowFilter.includes('e.message.content[0].type!=="text"'));

const nativeSupersedesContext = vm.createContext({
  yH: 24,
  iu: '__native-refusal-placeholder__',
  Eu: '__native-empty-placeholder__',
});
vm.runInContext(
  `${nativeRowUuidHelpers}${nativeFfNormalize}${nativeAssistantRowExpansion}${nativePreRowFilter}`,
  nativeSupersedesContext,
);
const jeInputs = [
  {querySource: 'repl_main_thread', expected: true},
  {querySource: 'repl_main_thread_tool', expected: true},
  {querySource: 'sdk', expected: true},
  {querySource: 'subagent', expected: false},
  {querySource: 'side_question', expected: false},
  {querySource: '', expected: false},
];
const jeCases = jeInputs.map(input => {
  const context = vm.createContext({
    b: input.querySource,
    e: {toolUseContext: {agentId: null}, forkOrigin: undefined},
    un: () => null,
  });
  vm.runInContext(nativeJeDefinition, context);
  vm.runInContext(nativeJeAssignment, context);
  assert.equal(context.Je, input.expected, input.querySource);
  return {querySource: input.querySource, Je: context.Je};
});

const transcriptProjectionContext = vm.createContext({
  Buffer,
  p6n: () => false,
  uqn: () => 'ant',
  HRr: new Set(),
  jm: () => '/native-fixture-cwd',
  akn: () => '2.1.288',
  ore: () => true,
});
vm.runInContext(`${nativePersistFilter}${nativeTranscriptProjection}`, transcriptProjectionContext);
const projectedLocalNotice = transcriptProjectionContext.DHo([{
  type: 'system',
  subtype: 'model_refusal_fallback',
  scope: 'local',
  content: 'local fallback notice',
  level: 'warning',
  trigger: 'refusal',
  direction: 'retry',
  originalModel: 'origin',
  fallbackModel: 'fallback',
  requestId: 'req-local-persist',
  uuid: 'local-notice-uuid',
  timestamp: '2026-10-03T12:34:56.000Z',
}], 'session-id');
assert.equal(projectedLocalNotice.length, 1);
assert.equal(projectedLocalNotice[0].scope, 'local');
assert.equal(projectedLocalNotice[0].uuid, 'local-notice-uuid');
assert.equal(projectedLocalNotice[0].parentUuid, null);
const nativeLocalPersistenceProjection = JSON.parse(JSON.stringify(projectedLocalNotice));

const utf16SanitizerContext = vm.createContext({Buffer, Uee: value => value});
vm.runInContext(
  `${nativeUtf16Prefix}${nativeUtf16RoundTrip}${nativePi}${nativeDetailsSanitizer}`,
  utf16SanitizerContext,
);
const utf16CapInputs = [
  {name: 'astral-pair-fits-at-cap', input: `${'x'.repeat(253)}🚀`, expectedLength: 255, expectedTail: '🚀'},
  {name: 'astral-pair-straddles-cap-backs-off-high-surrogate', input: `${'x'.repeat(254)}🚀`, expectedLength: 254, expectedTail: 'xx'},
];
const utf16CapCases = utf16CapInputs.map(input => {
  const output = utf16SanitizerContext.pi(input.input, {maxCodeUnits: 255});
  assert.equal(output.length, input.expectedLength);
  assert.equal(output.slice(-2), input.expectedTail);
  if (input.name.includes('straddles')) {
    const details = utf16SanitizerContext.k1e(input.input);
    assert.equal(details, `\n\nDetails: \`[${'x'.repeat(254)}]\``);
    assert.equal(details.includes('\uD83D'), false);
  }
  return {
    name: input.name,
    inputUtf16CodeUnits: input.input.length,
    outputUtf16CodeUnits: output.length,
    outputTail: output.slice(-2),
    details: input.name.includes('straddles') ? utf16SanitizerContext.k1e(input.input) : null,
  };
});

assert(visibleReason.includes('e==="refusal"||e==="sticky"'));
assert(serverControl.includes('userVisible'));
assert(acceptedFallback.includes('if(X.showBanner&&!Tt)Q=sHo('));
assert(acceptedFallback.includes('J={text:ke,originals:M.retainedMessages??[]}'));
assert(acceptedFallback.includes('replacesUuids:J.originals.map((ke)=>ke.uuid)'));
assert(acceptedAssistantMerge.includes('text:J!==void 0?J.text+Tt.text:H$r(io,Tt.text)'));
assert(acceptedAssistantMerge.includes('W=new Set([We.uuid]),J=void 0'));
assert(nativeServerFallbackVisibleGate.includes('X.userVisible){y("refusal_fallback")'));
assert(nativeServerFallbackStitchSeed.includes('J={text:ke,originals:M.retainedMessages??[]}'));
assert(!nativeServerFallbackStitchSeed.includes('J===void 0'));

function runNativeServerFallbackSeed(event, initialJ, localStitchText) {
  const logs = [];
  const context = vm.createContext({
    M: event,
    W: new Set(),
    K: [{uuid: 'current-turn-row'}],
    te: 0,
    J: initialJ === undefined ? undefined : JSON.parse(JSON.stringify(initialJ)),
    io: localStitchText,
    t: (...args) => logs.push(args),
  });
  vm.runInContext(nativeServerFallbackStitchSeed, context);
  return {
    J: context.J === undefined ? null : JSON.parse(JSON.stringify(context.J)),
    visibleRows: [...context.W],
    logs,
  };
}

const nativeServerFallbackSeedCases = [
  {
    name: 'first-visible-midstream-event-seeds-J',
    event: {midStream: true, retainedText: 'first prefix', retainedMessages: [{uuid: 'first-row'}]},
    initialJ: undefined,
    localStitchText: undefined,
    expectedJ: {text: 'first prefix', originals: [{uuid: 'first-row'}]},
    expectedWarnings: 0,
  },
  {
    name: 'later-visible-midstream-event-replaces-pending-J',
    event: {midStream: true, retainedText: 'latest prefix', retainedMessages: [{uuid: 'latest-row'}]},
    initialJ: {text: 'first prefix', originals: [{uuid: 'first-row'}]},
    localStitchText: undefined,
    expectedJ: {text: 'latest prefix', originals: [{uuid: 'latest-row'}]},
    expectedWarnings: 0,
  },
  {
    name: 'missing-retained-text-keeps-existing-J',
    event: {midStream: true},
    initialJ: {text: 'prior prefix', originals: [{uuid: 'prior-row'}]},
    localStitchText: undefined,
    expectedJ: {text: 'prior prefix', originals: [{uuid: 'prior-row'}]},
    expectedWarnings: 0,
  },
  {
    name: 'empty-retained-text-keeps-existing-J',
    event: {midStream: true, retainedText: '', retainedMessages: []},
    initialJ: {text: 'prior prefix', originals: [{uuid: 'prior-row'}]},
    localStitchText: undefined,
    expectedJ: {text: 'prior prefix', originals: [{uuid: 'prior-row'}]},
    expectedWarnings: 0,
  },
  {
    name: 'local-silent-stitch-blocks-server-J-seed',
    event: {midStream: true, retainedText: 'server prefix', retainedMessages: [{uuid: 'server-row'}]},
    initialJ: undefined,
    localStitchText: 'local prefix',
    expectedJ: null,
    expectedWarnings: 1,
  },
  {
    name: 'missing-retained-messages-defaults-to-empty-array',
    event: {midStream: true, retainedText: 'prefix'},
    initialJ: undefined,
    localStitchText: undefined,
    expectedJ: {text: 'prefix', originals: []},
    expectedWarnings: 0,
  },
];
const nativeServerFallbackSeedResults = nativeServerFallbackSeedCases.map(input => {
  const result = runNativeServerFallbackSeed(input.event, input.initialJ, input.localStitchText);
  assert.deepEqual(result.J, input.expectedJ, input.name);
  assert.equal(result.logs.length, input.expectedWarnings, input.name);
  assert.deepEqual(result.visibleRows, ['current-turn-row'], input.name);
  return {
    name: input.name,
    input: {
      midStream: input.event.midStream,
      retainedText: input.event.retainedText,
      retainedTextState: input.event.retainedText === undefined ? 'undefined' : input.event.retainedText.length === 0 ? 'empty' : 'nonempty',
      retainedMessages: input.event.retainedMessages,
      initialJ: input.initialJ,
      localStitchPending: input.localStitchText !== undefined,
    },
    output: result,
  };
});
let nativeSequentialPendingJ;
const nativeSequentialServerFallbackHops = [
  {midStream: true, retainedText: 'A retained text', retainedMessages: [{uuid: 'A-row'}]},
  {midStream: true, retainedText: 'B retained text', retainedMessages: [{uuid: 'B-row'}]},
].map((event, index) => {
  const result = runNativeServerFallbackSeed(event, nativeSequentialPendingJ, undefined);
  nativeSequentialPendingJ = result.J;
  return {hop: index + 1, event, output: result};
});
assert.deepEqual(nativeSequentialServerFallbackHops.map(hop => hop.output.J), [
  {text: 'A retained text', originals: [{uuid: 'A-row'}]},
  {text: 'B retained text', originals: [{uuid: 'B-row'}]},
]);

function makeContext(facts = {}) {
  const appSettings = {
    DISABLE_FEEDBACK_COMMAND: facts.disableFeedbackCommand === true,
    DISABLE_BUG_COMMAND: facts.disableBugCommand === true,
    ANTHROPIC_DEFAULT_FABLE_MODEL: facts.defaultFableModel ?? 'claude-fable-default',
  };
  const context = vm.createContext({
    a: appSettings,
    Tt: () => facts.disableNonessentialTraffic === true,
    lp: (key, command, method) => {
      assert.equal(key, 'allow_product_feedback');
      assert.equal(command, '/feedback');
      assert.equal(method, 'is');
      return facts.allowProductFeedback === true ? null : 'feedback unavailable';
    },
    Sfe: () => false,
    Be: model => model,
    kt: model => model,
    tO: model => model,
    Bh: () => false,
    Da: model => facts.sonnetModel === model ? {family: 'sonnet'} : undefined,
    Nm: model => model,
    Fo: model => ({
      origin: 'Origin',
      fallback: 'Fallback',
      'origin-sonnet': 'Sonnet origin',
    })[model] ?? model,
    // Test categories are safe ASCII labels, so native pi sanitization is identity for these inputs.
    pi: value => value,
    SGe: 255,
    _No: reason => reason,
    f3: () => null,
    Mre: category => category,
    cs: event => event,
  });
  vm.runInContext(
    `${fablePrefix}${fableAlias}${defaultFable}${feedbackAvailability}${helpUrls}` +
      cyberBioPredicate + noticeHelpers + visibleReason + serverControl + modelIdentity +
      receivedModel + noticeProducer + stitchHelper,
    context,
  );
  return context;
}

const fixedNoticeMeta = {timestamp: '2026-10-03T12:34:56.000Z', uuid: 'uuid-fixed'};
const noticeInputs = [
  {
    name: 'session-refusal-generic-feedback-disabled', scope: 'session', reason: 'refusal',
    fromModel: 'origin', toModel: 'fallback', requestId: 'req-refusal', categoryState: 'undefined',
    facts: {allowProductFeedback: false},
  },
  {
    name: 'session-sticky-generic-feedback-disabled', scope: 'session', reason: 'sticky',
    fromModel: 'origin', toModel: 'fallback', requestId: 'req-sticky', categoryState: 'undefined',
    facts: {allowProductFeedback: false},
  },
  {
    name: 'session-cyber-generic', scope: 'session', reason: 'refusal',
    fromModel: 'origin', toModel: 'fallback', requestId: 'req-cyber', category: 'cyber', categoryState: 'value',
    facts: {allowProductFeedback: false},
  },
  {
    name: 'session-bio-generic-feedback-enabled', scope: 'session', reason: 'sticky',
    fromModel: 'origin', toModel: 'fallback', requestId: 'req-bio', category: 'bio', categoryState: 'value',
    facts: {allowProductFeedback: true},
  },
  {
    name: 'local-empty-retained-text', scope: 'local', reason: 'refusal', retainedText: '',
    fromModel: 'origin', toModel: 'fallback', requestId: 'req-local-empty', categoryState: 'undefined',
    facts: {allowProductFeedback: false},
  },
  {
    name: 'local-nonempty-retained-text', scope: 'local', reason: 'sticky', retainedText: 'saved prefix',
    fromModel: 'origin', toModel: 'fallback', requestId: 'req-local-retained', category: 'bio', categoryState: 'value',
    facts: {allowProductFeedback: false},
  },
  {
    name: 'local-empty-other-reason-builder-only', scope: 'local', reason: 'other', retainedText: '',
    fromModel: 'origin', toModel: 'fallback', requestId: 'req-local-other', categoryState: 'undefined',
    facts: {allowProductFeedback: false},
    note: 'Direct sHo producer case only; Tle/oHo keeps reason=other outside the accepted user-visible consumer path.',
  },
  {
    name: 'nullable-category-and-ignored-extra-fields', scope: 'session', reason: 'refusal',
    fromModel: 'origin', toModel: 'fallback', requestId: null, category: null, categoryState: 'null',
    extra: {profile: 'provider-profile', retainedMessages: [{uuid: 'ignored'}], unrelated: true},
    facts: {allowProductFeedback: false},
  },
  {
    name: 'opus-help-url-selection', scope: 'session', reason: 'refusal',
    fromModel: 'claude-opus-5', toModel: 'fallback', requestId: 'req-opus', categoryState: 'undefined',
    facts: {allowProductFeedback: false},
  },
  {
    name: 'fable-help-url-selection', scope: 'session', reason: 'refusal',
    fromModel: 'claude-fable-opus', toModel: 'fallback', requestId: 'req-fable', categoryState: 'undefined',
    facts: {allowProductFeedback: false},
  },
  {
    name: 'sonnet-cyber-trait-copy', scope: 'session', reason: 'refusal',
    fromModel: 'origin-sonnet', toModel: 'fallback', requestId: 'req-sonnet', category: 'cyber', categoryState: 'value',
    facts: {allowProductFeedback: false, sonnetModel: 'origin-sonnet'},
  },
  {
    name: 'feedback-env-disabled', scope: 'session', reason: 'refusal',
    fromModel: 'origin', toModel: 'fallback', requestId: 'req-feedback-env', categoryState: 'undefined',
    facts: {allowProductFeedback: true, disableFeedbackCommand: true},
  },
];

const noticeCases = noticeInputs.map(input => {
  const context = makeContext(input.facts);
  const event = {
    fromModel: input.fromModel,
    toModel: input.toModel,
    reason: input.reason,
    requestId: input.requestId,
    apiRefusalCategory: input.categoryState === 'undefined' ? undefined : input.category,
    ...(input.retainedText !== undefined ? {retainedText: input.retainedText} : {}),
    ...input.extra,
  };
  const notice = context.sHo(event, fixedNoticeMeta, input.scope);
  const serialized = JSON.parse(JSON.stringify(notice));
  assert.equal(notice.apiRefusalCategory, input.categoryState === 'undefined' ? undefined : input.category);
  assert.equal(notice.apiRefusalExplanation, null);
  assert.equal(notice.timestamp, fixedNoticeMeta.timestamp);
  assert.equal(notice.uuid, fixedNoticeMeta.uuid);
  return {
    input: {
      name: input.name,
      scope: input.scope,
      reason: input.reason,
      fromModel: input.fromModel,
      toModel: input.toModel,
      requestId: input.requestId ?? null,
      categoryState: input.categoryState,
      ...(input.retainedText !== undefined ? {retainedText: input.retainedText} : {}),
      ...(input.facts.sonnetModel ? {injectedModelFamily: 'sonnet'} : {}),
      ...(input.facts.allowProductFeedback !== undefined ? {allowProductFeedback: input.facts.allowProductFeedback} : {}),
      ...(input.facts.disableFeedbackCommand ? {disableFeedbackCommand: true} : {}),
      ...(input.note ? {note: input.note} : {}),
      ...(input.extra ? {extraProducerFields: Object.keys(input.extra)} : {}),
    },
    fields: Object.keys(notice),
    content: notice.content,
    serialized,
  };
});

const stitchInputs = [
  {name: 'overlap-identical', oldText: 'The answer', nextText: 'The answer'},
  {name: 'overlap-with-continuation', oldText: 'The answer', nextText: 'The answer continues'},
  {name: 'punctuation-word-seam', oldText: 'Done.', nextText: 'Next'},
  {name: 'word-word-seam', oldText: 'Draft', nextText: 'answer'},
  {name: 'list-seam-adds-newline', oldText: 'Intro', nextText: '- item'},
  {name: 'list-seam-keeps-existing-newline', oldText: 'Intro\n', nextText: '- item'},
  {name: 'blank-overlap', oldText: 'Kept text', nextText: '  … Kept text  '},
  {name: 'j-server-seam-is-separate-exact-concat', oldText: 'Kept prefix', nextText: 'continued'},
];
const stitchContext = makeContext();
const hDollarRResults = stitchInputs.map(input => ({...input, result: stitchContext.H$r(input.oldText, input.nextText)}));
assert.equal(hDollarRResults.at(-1).result, 'Kept prefix continued');

function runNativeServerStitch(input) {
  const originals = input.originals ?? [{uuid: 'old-row-1'}, {uuid: 'old-row-2'}];
  const assistant = {
    type: 'assistant',
    isApiErrorMessage: input.apiError === true,
    uuid: input.outerUuid ?? 'next-outer-uuid',
    parentUuid: input.parentUuid ?? 'prior-parent-uuid',
    apiBlockIndex: 4,
    apiBlockIndices: [4, 5],
    message: {
      id: input.innerMessageId ?? 'provider-inner-message-id',
      model: 'fallback-model',
      stop_reason: 'end_turn',
      providerEnvelope: {preserved: true},
      content: input.content ?? [
        {type: 'text', text: input.text ?? ' answer', cache_control: {type: 'ephemeral'}},
        {type: 'thinking', thinking: 'preserve this block'},
        {type: 'tool_use', id: 'tool-from-current-row', name: 'Lookup', input: {q: 'x'}},
      ],
    },
  };
  const context = vm.createContext({
    M: assistant,
    Me: assistant,
    io: undefined,
    J: input.prefix === undefined ? undefined : {text: input.prefix, originals},
    // The SDK event and the in-flight query accumulator carry separate arrays
    // of the same retained rows; J.originals must survive K's row removals.
    K: [...(input.initialRows ?? originals)],
    Z: [],
    Ut: false,
    _o: undefined,
    ne: [],
    Je: input.isMainThread === true,
    Ff: nativeSupersedesContext.Ff,
    pre: nativeSupersedesContext.pre,
    _: value => value,
    i: () => {},
  });
  vm.runInContext(`${acceptedAssistantMerge};`, context);
  const row = context.K.findLast(item => item.uuid === assistant.uuid) ?? null;
  return {
    row: row === null ? null : JSON.parse(JSON.stringify(row)),
    pendingStitch: context.J === undefined ? null : JSON.parse(JSON.stringify(context.J)),
    rows: JSON.parse(JSON.stringify(context.K)),
    stitched: context.W?.has(assistant.uuid) === true,
  };
}

const nativeStitchInputs = [
  {
    name: 'local-stitch-preserves-distinct-outer-and-provider-ids-envelope-and-content',
    prefix: 'saved prefix ', text: ' answer',
  },
  {
    name: 'local-stitch-has-zero-inserted-separator-bytes',
    prefix: 'retained ', text: ' answer',
  },
  {
    name: 'api-error-assistant-does-not-consume-pending-stitch',
    prefix: 'saved prefix',
    content: [{type: 'text', text: ' error'}],
    apiError: true,
  },
  {
    name: 'whitespace-only-first-text-does-not-consume-pending-stitch',
    prefix: 'saved prefix',
    content: [{type: 'text', text: '  \n  '}, {type: 'text', text: 'later visible block'}],
  },
  {
    name: 'assistant-without-text-does-not-consume-pending-stitch',
    prefix: 'saved prefix',
    content: [{type: 'thinking', thinking: 'no text block'}],
  },
  {
    name: 'native-main-je-flattens-and-filters-superseded-rows-in-order',
    isMainThread: true,
    prefix: 'saved prefix',
    originals: [
      {
        type: 'assistant', uuid: 'a'.repeat(24), timestamp: '2026-10-03T12:00:00.000Z',
        message: {
          id: 'old-provider-id', model: 'origin-model', stop_reason: 'end_turn',
          content: [
            {type: 'text', text: 'old answer'},
            {type: 'tool_use', id: 'old-tool', name: 'Search', input: {}},
            {type: 'thinking', thinking: 'old thinking'},
            {type: 'text', text: '  \n  '},
          ],
        },
      },
      {
        type: 'assistant', uuid: 'b'.repeat(24), timestamp: '2026-10-03T12:00:01.000Z',
        message: {id: 'old-single-provider-id', model: 'origin-model', content: [{type: 'text', text: 'second old answer'}]},
      },
    ],
  },
];
const nativeStitchCases = nativeStitchInputs.map(input => {
  const result = runNativeServerStitch(input);
  const shouldStitch = !input.apiError && input.content?.[0]?.type !== 'thinking' &&
    (input.content?.[0]?.text ?? input.text ?? ' answer').trim().length > 0;
  assert.equal(result.stitched, shouldStitch, input.name);
  if (shouldStitch) {
    assert.equal(result.row.uuid, input.outerUuid ?? 'next-outer-uuid');
    assert.equal(result.row.parentUuid, input.parentUuid ?? 'prior-parent-uuid');
    assert.equal(result.row.message.id, input.innerMessageId ?? 'provider-inner-message-id');
    assert.equal(result.row.message.providerEnvelope.preserved, true);
    assert.equal(Object.hasOwn(result.row, 'apiBlockIndex'), false);
    assert.equal(Object.hasOwn(result.row, 'apiBlockIndices'), false);
    assert.equal(result.row.message.content[0].text, `${input.prefix}${input.text ?? ' answer'}`);
    assert.equal(result.row.message.content[0].cache_control.type, 'ephemeral');
    const originalContent = input.content ?? [
      {type: 'text', text: input.text ?? ' answer', cache_control: {type: 'ephemeral'}},
      {type: 'thinking', thinking: 'preserve this block'},
      {type: 'tool_use', id: 'tool-from-current-row', name: 'Lookup', input: {q: 'x'}},
    ];
    assert.deepEqual(result.row.message.content.slice(1), originalContent.slice(1));
    if (!input.isMainThread) assert.equal(Object.hasOwn(result.row, 'supersedesUuids'), false);
    if (input.name.includes('zero-inserted')) assert.equal(result.row.message.content[0].text, 'retained  answer');
    assert.deepEqual(result.rows.map(row => row.uuid), [result.row.uuid]);
    assert.deepEqual(result.pendingStitch, null);
    if (input.isMainThread) {
      const expectedSupersedes = input.originals.flatMap(original =>
        Array.from(nativeSupersedesContext.Ff([original]))
          .filter(row => nativeSupersedesContext.pre(row))
          .map(row => row.uuid),
      );
      assert.deepEqual(result.row.supersedesUuids, expectedSupersedes);
    }
  } else {
    assert.equal(result.row, null);
    assert.deepEqual(result.pendingStitch?.text, input.prefix);
    assert.deepEqual(result.rows.map(row => row.uuid), ['old-row-1', 'old-row-2']);
  }
  return {
    input: {
      name: input.name,
      prefix: input.prefix,
      Je: input.isMainThread === true,
      assistantIsApiError: input.apiError === true,
      firstContentBlock: input.content?.[0] ?? {type: 'text', text: input.text ?? ' answer'},
      inputOuterUuid: input.outerUuid ?? 'next-outer-uuid',
      inputProviderMessageId: input.innerMessageId ?? 'provider-inner-message-id',
      inputParentUuid: input.parentUuid ?? 'prior-parent-uuid',
    },
    output: result,
  };
});

const noticeRangeLabels = new Set([
  'native feedback availability and QA footer gate',
  'native Fable canonical-prefix predicate',
  'native Fable alias predicate',
  'native default Fable model predicate',
  'native refusal fallback help URLs',
  'native refusal fallback notice text and model-trait helpers',
  'native cyber and bio category predicate',
  'native sHo notice producer',
  'native UTF-16-safe bounded prefix helper',
  'native UTF-16 round-trip helper',
  'native sanitized-text cap wrapper',
  'native refusal-category details renderer',
]);
const noticeData = {
  version: '2.1.288',
  native_sha256: sha256(binary),
  sourceByteRanges: sourceByteRanges.filter(item => noticeRangeLabels.has(item.label)),
  scope: 'Executes native sHo and its native text/model-trait helpers with explicitly injected safe display labels, QA feedback availability, and model-family facts. The composed notice cases use safe ASCII categories. Separate cases directly execute native pi/k1e and the bounded UTF-16 helper with Uee injected as identity, proving the 255-code-unit boundary and delimiter rendering but not ANSI stripping or control/default-ignorable input sanitation. Host settings, model/catalog acquisition, and allowlist policy remain outside this oracle.',
  noticeCases,
  utf16CapCases,
};

const acceptedCases = [
  {
    name: 'accepted-main-refusal-changes-session-route',
    admittedByServerModelPolicy: true, isMainThread: true, emitsLocalScope: false,
    sessionModel: 'origin', selectedRouteModel: 'origin', reason: 'refusal', midStream: false,
    fromModel: 'origin', toModel: 'fallback', category: 'cyber', requestId: 'req-main',
  },
  {
    name: 'accepted-local-sticky-midstream',
    admittedByServerModelPolicy: true, isMainThread: false, emitsLocalScope: true,
    sessionModel: 'origin', selectedRouteModel: 'origin', reason: 'sticky', midStream: true,
    fromModel: 'origin', toModel: 'fallback', category: 'bio', requestId: 'req-local',
    retainedText: 'saved prefix', retainedMessages: [{uuid: 'old-1'}, {uuid: 'old-2'}],
    nextAssistantUuid: 'next-assistant', nextAssistantMessageId: 'next-assistant', nextAssistantText: 'answer',
  },
  {
    name: 'same-session-model-suppresses-banner',
    admittedByServerModelPolicy: true, isMainThread: true, emitsLocalScope: false,
    sessionModel: 'fallback', selectedRouteModel: 'fallback', reason: 'refusal', midStream: false,
    fromModel: 'fallback', toModel: 'fallback', category: null, requestId: 'req-same',
  },
  {
    name: 'other-reason-is-outside-visible-consumer',
    admittedByServerModelPolicy: true, isMainThread: true, emitsLocalScope: false,
    sessionModel: 'origin', selectedRouteModel: 'origin', reason: 'other', midStream: true,
    fromModel: 'origin', toModel: 'fallback', category: 'cyber', requestId: 'req-other',
    retainedText: 'must not seed', retainedMessages: [{uuid: 'ignored'}],
  },
  {
    name: 'denied-target-has-no-accepted-notice-or-stitch',
    admittedByServerModelPolicy: false, isMainThread: true, emitsLocalScope: false,
    sessionModel: 'origin', selectedRouteModel: 'origin', reason: 'refusal', midStream: true,
    fromModel: 'origin', toModel: 'denied', category: 'cyber', requestId: 'req-denied',
    retainedText: 'must not seed', retainedMessages: [{uuid: 'ignored'}],
  },
  {
    name: 'undefined-retained-text-does-not-start-server-seam',
    admittedByServerModelPolicy: true, isMainThread: true, emitsLocalScope: false,
    sessionModel: 'origin', selectedRouteModel: 'origin', reason: 'sticky', midStream: true,
    fromModel: 'origin', toModel: 'fallback', category: null, requestId: 'req-no-retained',
  },
  {
    name: 'empty-retained-text-does-not-start-server-seam',
    admittedByServerModelPolicy: true, isMainThread: true, emitsLocalScope: false,
    sessionModel: 'origin', selectedRouteModel: 'origin', reason: 'sticky', midStream: true,
    fromModel: 'origin', toModel: 'fallback', category: null, requestId: 'req-empty-retained',
    retainedText: '', retainedMessages: [{uuid: 'ignored-empty'}],
  },
  {
    name: 'pending-local-silent-stitch-skips-server-seed',
    admittedByServerModelPolicy: true, isMainThread: true, emitsLocalScope: false,
    sessionModel: 'origin', selectedRouteModel: 'origin', reason: 'sticky', midStream: true,
    fromModel: 'origin', toModel: 'fallback', category: null, requestId: 'req-pending',
    retainedText: 'second prefix', retainedMessages: [{uuid: 'ignored-pending'}], localStitchAlreadyExists: true,
  },
];

const consumerCases = acceptedCases.map(input => {
  const context = makeContext({allowProductFeedback: false});
  const event = {
    type: 'server_fallback',
    reason: input.reason,
    fromModel: input.fromModel,
    toModel: input.toModel,
    requestId: input.requestId,
    apiRefusalCategory: input.category,
    midStream: input.midStream,
    discardedMessages: [],
    finalStopReason: 'end_turn',
    ...(input.retainedText !== undefined ? {retainedText: input.retainedText} : {}),
    ...(input.retainedMessages !== undefined ? {retainedMessages: input.retainedMessages} : {}),
  };
  const decision = context.oHo(event, {isMainThread: input.isMainThread, emitsLocalScope: input.emitsLocalScope});
  const resolvedModel = context.Ucn(input.selectedRouteModel, input.toModel);
  const sameModel = input.sessionModel === resolvedModel;
  let notice;
  let stitch;
  if (decision.userVisible && input.admittedByServerModelPolicy) {
    if (decision.showBanner && !sameModel) {
      notice = context.sHo({...event, fromModel: input.sessionModel, toModel: resolvedModel}, fixedNoticeMeta, decision.bannerScope);
    }
    if (input.midStream && event.retainedText !== undefined && event.retainedText.length > 0) {
      if (!input.localStitchAlreadyExists) {
        const originals = event.retainedMessages ?? [];
        stitch = {
          phase: 'begin',
          salvageText: event.retainedText,
          join: 'exact',
          replacesUuids: originals.map(message => message.uuid),
        };
        if (input.nextAssistantText?.trim().length > 0) {
          stitch.merge = {
            assistantUuid: input.nextAssistantUuid,
            messageId: input.nextAssistantMessageId,
            text: event.retainedText + input.nextAssistantText,
            separator: '',
            removedOriginalUuids: originals.map(message => message.uuid),
          };
        }
      } else {
        stitch = {skipped: 'a local silent stitch is already pending'};
      }
    }
  }
  return {
    input: {
      name: input.name,
      admittedByServerModelPolicy: input.admittedByServerModelPolicy,
      reason: input.reason,
      midStream: input.midStream,
      isMainThread: input.isMainThread,
      sessionModel: input.sessionModel,
      selectedRouteModel: input.selectedRouteModel,
      receivedModel: input.toModel,
      resolvedModel,
      sameModel,
      retainedTextState: input.retainedText === undefined ? 'undefined' : input.retainedText.length === 0 ? 'empty' : 'nonempty',
      localStitchAlreadyExists: input.localStitchAlreadyExists === true,
    },
    control: {
      userVisible: decision.userVisible,
      swapSession: decision.swapSession,
      showBanner: decision.showBanner,
      bannerScope: decision.bannerScope,
    },
    notice: notice ? JSON.parse(JSON.stringify(notice)) : null,
    stitch: stitch ?? null,
  };
});

const seamData = {
  version: '2.1.288',
  native_sha256: sha256(binary),
  sourceByteRanges: sourceByteRanges.filter(item => [
    'native user-visible reason predicate',
    'native accepted fallback control decision',
    'native context-tag model identity',
    'native declared and received model preference',
    'native accepted server-fallback seam and notice branch',
    'native server-fallback user-visible outer gate',
    'native server-fallback J seed assignment',
    'native accepted assistant stitch and message identity branch',
    'native current assistant candidate assignment',
    'native H$r local text stitching helper',
    'native queued fallback notice flush helper',
    'native refusal-decline queued notice flush',
    'native local fallback stitch occupancy',
    'native main-query Je source predicate',
    'native Je assignment',
    'native supersedes row eligibility filter',
    'native per-content-block UUID derivation helpers',
    'native original-message row expansion dispatcher',
    'native assistant per-content-block row expansion',
    'native normal-terminal queued notice flush',
    'native query-error queued notice flush',
    'native engine fallback-event dispatch',
    'native fallback-event to transcript-row mapping',
    'native event-to-message callback adapter',
    'native query-event transcript application',
    'native message append to live transcript',
    'native engine callback applies query events',
    'native transcript-entry persistence filter',
    'native transcript-to-JSONL row projection',
    'native JSONL transcript row writer',
    'native transcript projection export name',
    'native JSONL writer export name',
  ].includes(item.label)),
  scope: 'Executes native Tle/oHo/Mn/Ucn/H$r, the exact native midstream J-seed clause, accepted J assistant-row merge, native Je source selection, and native Ff/pre supersedes expansion. The accepted consumer projection receives admittedByServerModelPolicy as an explicit fact because organization policy acquisition is outside this pure fixture. Row tests use a synthetic current assistant envelope with parentUuid already populated; they prove the merge preserves populated outer fields, not that upstream producers always provide a parent. Native accepted local notice queue/yield, engine dispatch, live transcript append, entry filter, and JSONL writer are source-checked here; UI display/JSONL file I/O itself and full host policy remain outside.',
  consumerSourceFacts: {
    noticeOnlyWhenShowBannerAndRouteChanged: 'if(X.showBanner&&!Tt)Q=sHo(...)',
    queuedNoticeFlush: 'function*He() yields the query-local Q and clears it; no scope check is present',
    noticeFlushBranches: ['refusal_no_fallback', 'normal terminal', 'query catch/error'],
    nativeSupersedesGate: 'Je=fe; fe is true exactly when querySource starts with repl_main_thread or equals sdk',
    outerVisibilityGate: 'server_fallback runs the decline/swap/tombstone/reset/J/notice body only under X.userVisible from oHo/Tle; non-visible reasons are observation-only',
    eventDispatch: 'FZe forwards every system/model_refusal_fallback through QZt without testing scope',
    liveTranscriptAppend: 'applyEvent -> m9e(onMessage) -> _applyMessage -> transcript.keep append; local scope is not tested on this path',
    transcriptJsonlFilter: 'RS excludes progress, selected empty attachments, and system persist:false; it has no scope exclusion, so an emitted local fallback row passes',
    jsonlWriter: 'writeEntriesToJsonlFile JSON.stringify-serializes each passed row and writes a newline',
    localPersistenceProjection: 'native toTranscriptEntries executed over a synthetic scope:local fallback system row retains it and its uuid after RS filtering',
    retainedTextSeedGate: 'inside M.midStream: ke!==void 0&&ke.length>0&&io===void 0',
    repeatedServerHopSeed: 'there is no J===void 0 guard; a later visible hop replaces pending J.text and J.originals with that event retainedText/retainedMessages',
    retainedTextHostReachability: 'Native source has an undefined retainedText branch, but current Rust ServerFallbackEvent.retained_text is String; undefined is a source-only case here, while empty string and empty retained_blocks are host-reachable',
    pendingLocalStitchSource: 'io is the pending local/silent refusal continuation text; when non-undefined, a server retainedText does not seed J and logs the native silent-stitch warning',
    retainedMessageDefault: 'M.retainedMessages??[]',
    beginEventJoin: 'join:"exact"',
    nextAssistantTextJoin: 'J.text+Tt.text',
    currentAssistantCandidate: 'let Me=M before the merge guard; X=Me.type==="assistant"?Me:M',
    nextAssistantIdentity: 'We spreads X outer fields (uuid/parentUuid when populated) and spreads X.message (provider message.id/envelope); no id is minted from J.originals',
    firstTextEligibility: 'first text block only; it must have text.trim().length>0 and M must not be isApiErrorMessage',
    transientApiIndices: 'apiBlockIndex and apiBlockIndices are removed when We is spread; numeric block-index equality is not used to pair across hops',
    supersedesExpression: 'inside Je only: At.flatMap(original => Ff([original]).filter(pre).map(row => row.uuid)); fixture executes the exact Ff/pre helpers for one Je=true multi-block case and confirms retained order',
    sourceNote: 'J.originals UUIDs are removed from K and populate begin.replacesUuids; only Je=true writes the filtered/flattened supersedesUuids. Per-block assistant UUIDs derive from the original row UUID plus the block index; single-block rows keep their UUID. No original UUID inheritance branch appears for the new We row. The synthetic parentUuid case tests preservation only when supplied; real upstream parent assignment remains separate.',
  },
  consumerCases,
  nativeServerFallbackSeedCases: nativeServerFallbackSeedResults,
  nativeSequentialServerFallbackHops,
  nativeStitchCases,
  jeCases,
  nativeLocalPersistenceProjection,
  hDollarRCases: hDollarRResults,
};

const corePath = new URL('../../crates/core/tests/fixtures/server_fallback_notice_2_1_288.json', import.meta.url);
const orchestratorPath = new URL('../../crates/orchestrator/tests/fixtures/server_fallback_consumer_2_1_288.json', import.meta.url);
const coreOutput = JSON.stringify(noticeData, null, 2) + '\n';
const orchestratorOutput = JSON.stringify(seamData, null, 2) + '\n';
if (process.argv.includes('--check')) {
  assert.equal(readFileSync(corePath, 'utf8'), coreOutput);
  assert.equal(readFileSync(orchestratorPath, 'utf8'), orchestratorOutput);
} else {
  writeFileSync(corePath, coreOutput);
  writeFileSync(orchestratorPath, orchestratorOutput);
}
console.log(JSON.stringify({noticeCases: noticeCases.length, consumerCases: consumerCases.length, hDollarRCases: hDollarRResults.length, sourceRanges: sourceByteRanges.length}));
