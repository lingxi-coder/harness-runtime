#!/usr/bin/env node
// Source-pinned Claude Code 2.1.289 Agent stream/append/W1 and concurrency-cap probe.
// This executes only exact pure Native integer-parser helpers in a VM. It does
// not start Claude Code, call a provider, or run a Harness production path.
//
// Usage:
//   node scripts/tests/mods_agent_stream_order_289_probe.mjs <2.1.289-binary>

import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const [binaryPath] = process.argv.slice(2);
assert(binaryPath, 'usage: node scripts/tests/mods_agent_stream_order_289_probe.mjs <2.1.289-binary>');
const binary = readFileSync(binaryPath);
const sha256 = value => createHash('sha256').update(value).digest('hex');
assert.equal(
  sha256(binary),
  '03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69',
  'unexpected Claude Code 2.1.289 binary SHA-256',
);

const sourceWindows = [];
function take(label, start, end, expectedHash, startText) {
  assert(start >= 0 && end > start, `${label}: invalid source byte range`);
  const bytes = binary.subarray(start, end);
  assert.equal(bytes.length, end - start, `${label}: source range is short`);
  if (startText) {
    assert.equal(bytes.subarray(0, Buffer.byteLength(startText)).toString(), startText,
      `${label}: source start marker changed`);
  }
  assert.equal(sha256(bytes), expectedHash, `${label}: source hash changed`);
  sourceWindows.push({label, start, end_exclusive: end, sha256: expectedHash});
  return bytes.toString('utf8');
}

const sessionStartSource = take(
  'main session.start dispatch and late-module replay',
  205788724, 205789464,
  'fa0cdf555259d293125289387ca95297d74121f848d304149e7e13ab5d976f28',
  'function hn(e,n,o){',
);
const agentRunnerSource = take(
  'Agent N5 row consumer',
  194760296, 194770594,
  'c3fbc7491ba06e1c1b4297004f310679fcf8df1eecb622159bcff42160783021',
  'function N5({taskId:e,abortController:s,makeStream:n',
);
const asyncAgentLaunchSource = take(
  'async Agent launch into N5 and MC',
  194849961, 194850600,
  '953e0221437e2f90475e88e4f2157ee1440938fa6637df73a13062fbbbf5eff6',
  'N5({taskId:w.agentId,abortController:w.abortController',
);
const contentBlockStopSource = take(
  'content_block_stop completed assistant row producer',
  189368685, 189370128,
  'aa8ce11178f40647830a76867eab1458645b378e24fbe5f6092a9f9042e4df50',
  'case"content_block_stop":{let ha=ng[ii.index]',
);
const responseRowsResetSource = take(
  'completed-row accumulator is reset for each stream response',
  189360098, 189360254,
  '48f132ffe226519d54e1fe6d2b12daf8f6fabe157c603f5c26e4cdbf8dcae9b0',
  'let ss=he.monotonicNow();wp.length=0',
);
const messageDeltaAllRowsSource = take(
  'message_delta terminal metadata updates every completed row',
  189371401, 189371840,
  '03a6b386623e88e67ffa67250960e20624d4f80cd91efe36193835b2f6a299c1',
  'let jc=typeof ii.delta.stop_reason',
);
const uiAppendBridgeSource = take(
  'Ui session append bridge',
  195330691, 195331847,
  'e4f8db2c16eefdd434b5a3cbf728bf54d228f83abc75806932c411df6e43e3e4',
  'function Ui(e,o){B5e.set(Yr)',
);
const sessionAppendEligibleRowTypesSource = take(
  'session append accepts user result rows in Ui.R',
  195330200, 195330691,
  '24ba3da2d11657f5126b59c8a5362e6e5022e0230907f7dac22b7ca4754fd823',
  's:e.callMs,retry_ms:e.retryMs,model:e.model===void 0?void 0:Et(e.model)})}',
);
const queryOuterAppendWrapperSource = take(
  'main query generator wraps Xl output in Ui.through',
  195423499, 195425275,
  'b351439ed10addcc0c52a9a30e82e2db44fa3e092edbc9f6a080f93b5d69a975',
  'async function*n0(e){',
);
const xqSource = take(
  'session.append hook gate',
  191180248, 191180513,
  '962d2adb18e375911f4fd513a0d71f3d79550fda81a6792b82f7863a1bddc732',
  'function Xq(e,n,r={})',
);
const appendContractSource = take(
  'session.append validation contract',
  182727018, 182728316,
  '481f4920b5e69666b7f31c884d9869b4b8bf0cc4f219a9c82dd3a6bdff4d13de',
  'function ln(e,t){',
);
const appendRowDispatchSource = take(
  'session.append row dispatch',
  212347111, 212348000,
  '135dea13c52fc00a732fa842002f689186bfc67b265c4321cb52eeac7f6fc156',
  'async function ue(e,n,r){',
);
const queryAppendThenEnqueueSource = take(
  'query append, yield, and tool enqueue order',
  195466181, 195469377,
  '380eb29894b93878ef7fc2d3b75c57ffc73b9d331a838520dff9b949e656570c',
  'if(M.type==="assistant"&&!M.isApiErrorMessage&&!mr([M]))M.permissionModeAtRequest=ud;',
);
const transcriptRecorderSource = take(
  'incremental transcript recorder and per-call cursor',
  206296696, 206298650,
  '531bf0855c64095b0b966bfb226989c211cbd5e4eb37d3b225a132574fbc0428',
  'function TS({conversation:e,messages:r,persistSession:n,storageV5:s})',
);
const transcriptRecordCallSitesSource = take(
  'headless assistant and user row recording call sites',
  206326212, 206326987,
  '134049f5b797bdfe0badd558aa74b9e8cae82ba2a69295c85010f22d3270f6f2',
  'case"assistant":{if("parent_tool_use_id"in De)break;',
);
const incompleteAssistantCutoffSource = take(
  'complete-only transcript cutoff for stop_reason-null assistant rows',
  191474132, 191474292,
  '26568c4169dbedee19b93739d3b24b071c8ba68ce53ee58eef52a5623095178c',
  'function ANt(e,n,r)',
);
const transcriptInsertMessageChainSource = take(
  'JSONL transcript row projection and ordered row append loop',
  191515206, 191517528,
  '4a4b60fe83a56a2d4d383dfb0888b680c85ada248396d35d2bf845ed4a4e5554',
  'async insertMessageChain(e,n=!1,r,s,g,h,b,w,M,B)',
);
const transcriptAppendEntrySource = take(
  'deduplicated JSONL append queue admission',
  191519854, 191522183,
  '31394d959e55eb7974664c65474b9ab9c4e44541e564d2d3725ed0447dd3b8aa',
  'async appendEntry(e,n=this.store.getSessionId(),r,s,g,h)',
);
const transcriptWriteQueueEnqueueSource = take(
  'transcript write queue FIFO admission and delayed flush scheduler',
  191492478, 191493979,
  'f55a9c781062dcb299443f1d2a95aeb29a4d6611e449b116822012f18a05052c',
  'trackWrite(e){this.incrementPendingWrites();',
);
const transcriptWriteQueueDrainSource = take(
  'transcript write queue serialization and file append',
  191494860, 191496938,
  '1748fcdb8453e3f8ec0a8d2a6d5727f91d95be7a13c03c56dfd84224f1ca6183',
  'drainWriteQueue(){this.drainsInFlight++;',
);
const transcriptWriterFlushIntervalSource = take(
  'transcript writer flush interval',
  191491220, 191491360,
  '39376c492da40dae7de186b1e42bc2478041187ba13817b0736fbd782abee48a',
  'veDrain=null;storeFence=void 0;drain',
);
const transcriptWriterAdmissionSource = take(
  'session writer call after candidate assistant tool-use row',
  191527156, 191527568,
  '16d416ec204854967a784c83e3c13dbd9bcae07f40b14ffd6a143046edcc3385',
  'async function NE(e,n,r,s,g)',
);
const transcriptWriterRowFilterSource = take(
  'transcript normalization retains ordinary source row references',
  191644161, 191644274,
  '26f9f2cd7fbbd1f530c31f1ebede2607a8433a6e6e3fdfc7f606977ee3597f57',
  'function T9e(e,n=e)',
);
const transcriptRowMapSource = take(
  'ordinary transcript rows retain the original row object',
  191642780, 191643948,
  'c378235230c2b287f67be53ae8875c7b523f4dec17c1cd7e1a2110ea9b83b0cd',
  'function xAr(e,n){',
);
const refusalTombstoneConsumerSource = take(
  'headless tombstone requests persisted row eviction',
  206327451, 206328033,
  '11b53a15cebf3070dadf5e34ab6259080d0acca6ecb6db3cc921df8511022fcb',
  'case"tombstone":{yield*fo.onTombstone(De),xr.evict(De.message.uuid)',
);
const messageDeltaRecordCallSource = take(
  'terminal message_delta record call site',
  206328600, 206330000,
  '4fed172c382b09a909f900f5b342843613e005d48dc4a6d04661b67b04bef3b6',
  'ssage_delta")',
);
const headlessRowUnwrapSource = take(
  'headless row unwrap preserves nested message object reference',
  206339865, 206339914,
  '107d9136b73d06221433e0a6c46889b8466305b1426aba9095b1d3a4e04f27e0',
  'function xl(e){let{session_id:r,...n}=e;return n}',
);
const interactiveTranscriptRecorderSource = take(
  'interactive transcript recorder and in-flight incomplete-row cutoff',
  209701325, 209704237,
  '2c7b630b663b9d7c8839931dc5d2d7f264f45e155d891e65ececec285408d7ca',
  'class MSe{lastRecordedLength=0;',
);
const interactiveTranscriptRecorderHookSource = take(
  'interactive transcript recorder React snapshot subscriptions',
  209704237, 209704933,
  '1c73559cad8006fad39aca770df29db1981cbb393d773212c152d092def4f2b2',
  'function ASe(h,v=!1,M=!1,N,X=!1)',
);
const queryRoundInputSource = take(
  'next query round builds working history from H.messages',
  195431190, 195432057,
  'd2e569c74e02132f10467367df29052a00d78dbcc16b2bd1d14311612902c359',
);
const providerMessagesSource = take(
  'provider request consumes the current working history',
  195449754, 195450504,
  '5761508c90937c49e30ba530b8c23bfd090ea17e7f917b10359dd6704cb71ba4',
);
const nextTurnHistorySource = take(
  'completed query round carries raw K rows into next H.messages',
  195494144, 195495020,
  'e9c6e49c60a6ec63600f75d76c4d1740b96535a0661e6d0cefa91d1e39bf38a8',
);
const queryErrorDispositionSource = take(
  'query stream throw disposition and fallback-chain split',
  195470508, 195472449,
  '8be9c1b0f8a16e81007a48a86a89d5ac1e9717083bf17461f0b4bc51fbb66bce',
  'if(Gt!==void 0||se!==void 0){',
);
const queryErrorFinalizerSource = take(
  'model error completed-result drain and finalizer',
  195439370, 195440609,
  'd082ae78e6737bc5c45d5adda2d515c8406231b5f3d8e63d23f0ac0041ce3ce7',
  'function*Tn(h=!0){',
);
const attemptStateResetSource = take(
  'attempt-local rows, reset, and abandonAttempt tombstones',
  195408929, 195409771,
  '82fa20588a3b3404c18113d3de3839f31742cb6ca181803c7fbcaab5d71fb6ea',
  'function ul({createStreamingToolExecutor:e,onReset:o})',
);
const modelErrorCleanupSource = take(
  'pn preserves finished rows and rebuilds after discard helper',
  195414914, 195415236,
  '6e60a0aec6abb094ef5f3f236b8e28ff285147929b7077e68a44a8a9d112f1b3',
  'function pn(e,o){',
);
const abortExecutingToolsSource = take(
  'executor aborts executing tools and leaves queued tasks unstarted',
  195348540, 195349196,
  '93a6cbb46376c2e709da66fc58341cdbde7c78638338d46ddbe1d340d8033edb',
  'function nn(e,o,n){',
);
const unresolvedToolResultSource = take(
  'unanswered tool-use cancellation result rows',
  195420477, 195420769,
  'a57cc6636bc80c34121c148bc729cfafdb058e66a1a6238def0a59a77b82d410',
  'function*Wl(e,o,n,r){',
);
const toolCancellationTextSource = take(
  'model-error cancellation text warns effects may have happened',
  195414572, 195414734,
  '73da64a566740e4e4897ce3f5cca1457602bc630f7800a74306482dd22751658',
  'function xl(e){',
);
const streamingExecutorSource = take(
  'streaming tool executor W1 dispatch',
  195349196, 195357760,
  '6459257ae75dd02d6b6f06df7f14bf7ce0468ebea7d3ce7909e239bdd527cba8',
  'class ds{toolDefinitions;canUseTool;now;tools=[]',
);
const streamingToolContextLayerSource = take(
  'completed tool context layers update the executor context',
  195354117, 195355928,
  'ab6baabfd849f979f60a49d4ff78957f58d819da25513c83ed9d8ca6c8089e80',
  'async executeTool(e){',
);
const providerStreamWrapperSource = take(
  'provider event iterator wrapper return cleanup',
  195357760, 195358236,
  '6f4e4413fb4254a799ea9f686bea69ec26fec13f73796d3b4d90d28c50ad7192',
  'async function*sa(e,o){',
);
const toolResultAccumulatorSource = take(
  'tool result user rows enter the attempt-local je accumulator',
  195429764, 195430089,
  '0f2b2a6e52dbbc3f5932f2f60e43d6912d64070d27545cbaa103f2dbe6411e23',
  'xr=function(h,A){',
);
const completedResultPollSource = take(
  'toolResults je alias and per-event Tn completed-result poll',
  195439205, 195439671,
  'acbac16529445942a6c430201d13f36f9f1401f1ed384d0ddebfa31164c77e5d',
  'let ue=ul({createStreamingToolExecutor:',
);
const serverFallbackDispositionSource = take(
  'server fallback decline/accept cleanup and synthetic decline row',
  195453912, 195458354,
  'cbeb6affbd77bc8220676eef8842abe4b17551e562f5058519458d88b30a62e3',
  'if(M.type==="server_fallback"){',
);
const refusalFallbackRowSource = take(
  'refusal decline helper builds the native refusal api-error row',
  188298052, 188299837,
  '7a19dc31283968e16b1583ddc200d3b9ad0097f094e9de501f61733083242455',
  'function $dt(e,n,r,s,g){',
);
const apiErrorRowConstructorsSource = take(
  'generic assistant api-error row and constructor',
  191309034, 191310574,
  '194c892f3602f2fe05b6bc63e35ffc97bd0b5c795f96667fc8f4eddce4bfa521',
  'function Dyn({content:e,isApiErrorMessage:n=!1',
);
const userMessageRowConstructorSource = take(
  'Native user row constructor preserves content and assigns UUID/timestamp',
  191312309, 191313825,
  '643c2975cfe603827a217f3ebc358cdd155416c704bd0d6b37bcd0a25de1e33f',
  'function Re({content:e,isMeta:n',
);
const genericTombstoneDispatchSource = take(
  'generic event dispatcher strips displayOnly before tombstone callback',
  191363275, 191363406,
  'f0ef9d2cbe5620fbf0146acd1280e3bb386cd744fdafa69cd7d65ba61b809d07',
  'onTombstone:s,onStreamingThinking:g',
);
const interactiveTombstoneConsumerSource = take(
  'interactive transcript tombstone callback removes by UUID without displayOnly gate',
  209676451, 209676591,
  'eb408c6c149e1977195ef9c08578327e7d0e5e6dc79e35e373a50881d8fd4d55',
  'onTombstone:(St)=>{',
);
const persistedTombstoneRemovalSource = take(
  'Native tombstone persistence helper calls removeMessageByUuid',
  191536950, 191537035,
  '4644d4eab64b58b9e950f547eb0ca17685e89f03e587781565d54a5ceab435f8',
  'function xNt(e,n){',
);
const normalPostStreamToolDrainSource = take(
  'post-stream wait drains results and updates query context before hooks',
  195485961, 195489200,
  '7fcac4553bbb01791b1e0da0e5005fdc6c061a051391aa00de6bc79d42995a47',
  'let fd=ue.streamingToolExecutor.getRemainingResults();',
);
const capSource = take(
  'tool concurrency cap getter',
  194677799, 194677899,
  '9e01656ccb32b87bd6bdc5d449679274c1abf76f4176ee7158c5b8636ab705d1',
  'function la(){return a.CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY??10}',
);
const envBcSource = take(
  'typed integer Bc helper',
  179345260, 179345616,
  'c1919a81334a14b32c0d05b5f06d881e1064765c4eac51ecd8163299e67508a8',
  ' l(e)?e:void 0}var N=',
);
const envParserSource = take(
  'typed environment parser M.int',
  179454477, 179455158,
  'ea0d69c684767230a5ab95f695d98f495cdbcad75584b1ae9d5d92e7b8009fd7',
  'function i(n){return{parse:(e)=>n(u(e))}}',
);
const capDeclarationSource = take(
  'typed concurrency environment property',
  179517029, 179517159,
  'a7564ab9503e6989bfc94e0a047cd0dd2596a68c6050455c1424304fe2bb0a20',
  '.int(),Ai=M.int(),Di=M.int(),Ti=M.int(),Li=M.int({min:1}),ci=M.str(),',
);
const envProxySource = take(
  'environment proxy re-read/caching behavior',
  179520867, 179521350,
  'a542a4ddf40a5f0478d1528c5eeb4a96877071cfce0e06619535136cc63553e8',
  'function ant(o,t){let _=Object.create(t)',
);
const clockSource = take(
  'clock.sleep API validation and signal path',
  182777887, 182779180,
  'aed22be5b6ee6394261c0b864cb48af4748230765b2edb517606eaa8279fc083',
  'function Eg({pluginName:e,host:t,live:o,unloaded:r,invoke:n,signalFrom:s,makeSignal:i})',
);
const clockHandlerSource = take(
  'host clock.sleep handler',
  188479562, 188479742,
  '4c5ce44bdb68faab0383c97d6db3e390df7bcc7e0d9ab42f2c28a03e14eb7f86',
  'var edo=(e,n)=>ee(e.ms,n,{...n&&{abortError:()=>kas(n)}});',
);

assert.equal((binary.toString('latin1').match(/UJ\(\)\.session\.start\(/g) ?? []).length, 1,
  'expected the one explicit Native session.start dispatcher call');
assert.match(sessionStartSource, /async function yn\(\{loaded:e,surface:n,isInteractive:o\},r\)/);
assert.match(sessionStartSource, /await UJ\(\)\.session\.start\(\{cwd:oe\(\),surface:n,isInteractive:o\}\)/);
assert.match(agentRunnerSource, /function N5\(/);
assert.match(asyncAgentLaunchSource, /makeStream:\(E,P,G\)=>MC\(/);
assert.match(contentBlockStopSource, /uuid:t4\(\),timestamp:new Date\(\)\.toISOString\(\)/);
assert.match(contentBlockStopSource, /yield wd;break\}/);
assert.match(contentBlockStopSource, /wp\.push\(wd\)/);
assert.match(responseRowsResetSource, /wp\.length=0/);
assert.match(messageDeltaAllRowsSource,
  /if\(jc!==null\)Bu=jc.*for\(let Ml of wp\)\{if\(Ml\.message\.usage=fl,Ml\.message\.stop_reason=Bu/);
assert.match(messageDeltaAllRowsSource, /Ml\.message\.stop_details=ii\.delta\.stop_details\?\?null/);
assert.match(uiAppendBridgeSource, /appended:async\(v\)=>\{await R\(v\)\}/);
assert.match(uiAppendBridgeSource, /Promise\.resolve\(Xq\(b,\[v\],\{fills:!0\}\)\)\.catch/);
assert.match(sessionAppendEligibleRowTypesSource,
  /new Set\(\["user","assistant","attachment","system"\]\),Su=\(e\)=>typeof e==="object"&&e!==null&&\("uuid"in e\)&&\("type"in e\)&&_u\.has\(e\.type\)/);
assert.match(queryOuterAppendWrapperSource,
  /s=yield\*den\(\)\?up\(e,o,r,u\):u\.through\(Xl\(e,o,r,u\)\)/);
assert.match(queryOuterAppendWrapperSource, /u=r\.through\(Xl\(e,o,n,r\)\)/);
assert.match(uiAppendBridgeSource,
  /let D=R\(I\.value\);if\(D!==void 0\)await D;yield I\.value/);
assert.match(xqSource, /rowsAppendedTo\(e,n/);
assert.match(appendContractSource, /var jt=\["type","name","role","isMeta"\]/);
assert.match(appendContractSource, /if\(e\.uuid!==t\.uuid\)return/);
assert.match(appendContractSource, /Mt\(e\.message\)/);
assert.match(appendRowDispatchSource, /return m\?\?s/);
assert.match(queryAppendThenEnqueueSource, /await r\.appended\(M\)/);
assert.ok(queryAppendThenEnqueueSource.indexOf('await r.appended(M)') <
  queryAppendThenEnqueueSource.indexOf('yield Me'));
assert.ok(queryAppendThenEnqueueSource.indexOf('yield Me') <
  queryAppendThenEnqueueSource.indexOf('ue.streamingToolExecutor.addTool(ve,X)'));
assert.match(queryAppendThenEnqueueSource, /let X=eo\?\?M;if\(!Ut\)K\.push\(X\)/);
assert.match(queryAppendThenEnqueueSource, /yield\*Tn\(\)/);
assert.match(transcriptRecorderSource, /function A\(ge=!1\).*g=Fe;.*return NE\(ke,void 0,Ne,r,s\)/);
assert.match(transcriptRecorderSource, /return\{record:A,appendHostRows:he/);
assert.match(transcriptRecorderSource, /if\(g--,n\).*xNt\(ge,s\)/);
assert.match(transcriptRecordCallSitesSource,
  /Pn\.push\(xe\),mr\)xr\.record\(\);g\.keep\(\[xe\]\)/);
assert.match(transcriptRecordCallSitesSource,
  /Pn\.push\(xe\),mr\)await xr\.record\(\);g\.keep\(\[xe\]\)/);
assert.match(incompleteAssistantCutoffSource,
  /if\(!r\)return e\.length;.*g\.message\.stop_reason===null/);
assert.match(transcriptInsertMessageChainSource,
  /await this\.store\.sessionMessages\(Te,w\)[\s\S]*?let Xe=\{parentUuid:[\s\S]*?\.\.\.Ne[\s\S]*?await this\.appendEntry\(Xe/);
const transcriptEnvelopeStart = transcriptInsertMessageChainSource.indexOf('let Xe={');
const transcriptEnvelopeEnd = transcriptInsertMessageChainSource.indexOf('};if(Xe.type', transcriptEnvelopeStart);
assert.ok(transcriptEnvelopeStart >= 0 && transcriptEnvelopeEnd > transcriptEnvelopeStart);
const transcriptEnvelope = transcriptInsertMessageChainSource.slice(transcriptEnvelopeStart, transcriptEnvelopeEnd);
assert.match(transcriptEnvelope, /\.\.\.Ne/);
assert.doesNotMatch(transcriptEnvelope, /message:/,
  'the persisted outer envelope does not clone the nested message object');
assert.match(transcriptAppendEntrySource,
  /case"dedup-transcript".*let ye=!B\.has\(e\.uuid\);if\(K\|\|ye\)\{this\.enqueueWrite\(V,e,s\)/);
assert.match(transcriptWriteQueueEnqueueSource,
  /r\.push\(n\),this\.scheduleDrain\(\).*this\.pushQueueItem\(e,\{entry:n/);
assert.match(transcriptWriteQueueEnqueueSource,
  /setTimeout\(async\(\)=>\{if\(this\.flushTimer=null,this\.activeDrain=this\.drainWriteQueue\(\)/);
assert.match(transcriptWriteQueueDrainSource,
  /let\{entry:V\}=K,he=LM\(V\).*await this\.appendToFile\(e,g,b\)/);
assert.match(transcriptWriterFlushIntervalSource, /FLUSH_INTERVAL_MS=100/);
assert.match(transcriptWriterAdmissionSource,
  /let h=T9e\(e,s\).*await il\(\)\.sessionMessages\(b,g\).*insertMessageChain\(V/);
assert.match(transcriptWriterRowFilterSource,
  /function T9e\(e,n=e\)\{let r=e\.filter\(PS\);.*return xAr\(r,s\)/);
assert.match(transcriptRowMapSource, /if\(w!==h\)return\[\{\.\.\.s,message:\{\.\.\.s\.message,content:w\}\}\];return\[s\]/);
assert.match(refusalTombstoneConsumerSource, /yield\*fo\.onTombstone\(De\),xr\.evict\(De\.message\.uuid\)/);
assert.match(messageDeltaRecordCallSource, /ssage_delta"\)\{[\s\S]*?,mr\)xr\.record\(\)/);
assert.match(headlessRowUnwrapSource, /let\{session_id:r,\.\.\.n\}=e;return n/);
assert.match(interactiveTranscriptRecorderSource,
  /class MSe\{[\s\S]*?write\(h,v\)\{[\s\S]*?yt=ANt\(h,Math\.max\(St,ht\),X\)/);
assert.match(interactiveTranscriptRecorderSource, /NE\(Rt,Lo\(\)&&be\?/);
assert.match(interactiveTranscriptRecorderHookSource,
  /new MSe\(he,Me\).*Ie\.record\(h,\{ignore:v,headPending:X,isResponseInFlight:M/);
assert.match(interactiveTranscriptRecorderHookSource,
  /N\.isWritePaced\(\).*N\.isResponseInFlight\(\)/);
assert.match(nextTurnHistorySource,
  /H=\{guards:\{\.\.\.we,\.\.\.Cs,turnCount:zn\},messages:\[\.\.\.w,\.\.\.K,\.\.\.je\]/);
assert.match(queryRoundInputSource, /let w=\[\.\.\._i\(at\)\];w=r\.withArrivedRows\(w\)/);
assert.match(providerMessagesSource,
  /B\.callModel;for await\(let M of sa\(Cd\(\{messages:Ye\.bare\?W0o\(Zt\.messages,Ye\.userContext\):Zt\.messages/);
assert.match(queryErrorDispositionSource, /if\(Gt!==void 0\|\|se!==void 0\)/);
assert.match(queryErrorDispositionSource, /if\(se!==void 0&&ye!==void 0\)/);
assert.match(queryErrorDispositionSource, /throw Z/);
assert.match(queryErrorFinalizerSource,
  /getCompletedResults\(\).*yield\*Tn\(!1\).*W=pn\(ue,A\?"image_error":"model_error"\)/);
assert.match(modelErrorCleanupSource, /takeFinishedResults\(\);let\{removes:s\}=nn\(e\.streamingToolExecutor,o\)/);
assert.match(abortExecutingToolsSource, /discardAndAbortInFlight\(uwt\(\)\)/);
assert.match(unresolvedToolResultSource, /tool_result.*toolUseResult:o,sourceToolAssistantUUID:s\.uuid/);
assert.match(toolCancellationTextSource, /If it had already started, some of its effects may have happened/);
assert.match(streamingExecutorSource, /buildSameTurnToolUses\(e\)/);
assert.match(streamingExecutorSource,
  /W1\(e\.block,e\.assistantMessage,this\.canUseTool,\{\.\.\.this\.toolUseContext,abortController:g,markToolCallStarted:/);
assert.match(streamingToolContextLayerSource,
  /!e\.isConcurrencySafe&&n\.length>0\)this\.toolUseContext=owe\(this\.toolUseContext,n\)/);
assert.match(providerStreamWrapperSource,
  /finally\{Promise\.resolve\(n\.return\?\.\(void 0\)\)\.catch\(\(\)=>\{\}\)\}/);
assert.doesNotMatch(providerStreamWrapperSource, /finally\{[^}]*yield/);
assert.match(attemptStateResetSource,
  /assistantMessages:\[\],toolResults:\[\].*reset\(\{clearAssistantMessages:r\}\).*n\.toolResults\.length=0/);
assert.match(attemptStateResetSource,
  /\*abandonAttempt\(r,s\)\{for\(let g of n\.assistantMessages\)yield\{type:"tombstone",message:g\};for\(let g of n\.toolResults\)yield\{type:"tombstone",message:g\};n\.reset\(\{clearAssistantMessages:!0\}\)/);
assert.match(toolResultAccumulatorSource,
  /xr=function\(h,A\)\{if\(h\.type==="attachment"\)\{je\.push\(h\);return\}.*je\.push\(\.\.\.te\.filter\(\(W\)=>W\.type==="user"\)\)/);
assert.match(completedResultPollSource,
  /toolResults:je.*getCompletedResults\(\).*yield bn\(A\.message\),Ar\(A\.message\),xr\(A\.message,f\)/);
assert.doesNotMatch(completedResultPollSource, /newContext/);
assert.match(serverFallbackDispositionSource,
  /if\(M\.type==="server_fallback"\).*\),X\.userVisible\)\{.*if\(!\(uA\(M\.toModel\)\?\?\(Ur\(M\.toModel\)\|\|ay\(M\.toModel\)\)\)\)/);
assert.match(serverFallbackDispositionSource,
  /let We=je\.slice\(\);ue\.reset\(\{clearAssistantMessages:!1\}\)/);
assert.match(serverFallbackDispositionSource,
  /for\(let Xe of We\)yield\{type:"tombstone",message:Xe\}/);
assert.match(serverFallbackDispositionSource,
  /M\.reason==="refusal"\?\$dt\("refusal",\{type:"refusal",category:M\.apiRefusalCategory\?\?null,explanation:null,fallback_credit_token:null,fallback_has_prefill_claim:null,recommended_model:null\},M\.requestId,ge\):cs\(\{content:"The server routed this response to a model that is not in your organization\\u2019s availableModels allowlist; the response was discarded\.",error:"invalid_request",now:B\.now,uuid:B\.uuid\}\)/);
assert.match(serverFallbackDispositionSource,
  /if\(Ro\)await r\.appended\(Ro\),K\.push\(Ro\),yield Ro;break\}/);
assert.match(serverFallbackDispositionSource,
  /let tn=new Set\(M\.discardedMessages\.map\(\(ke\)=>ke\.uuid\)\).*if\(X\.tombstonedToolUse\)\{.*to=\[\.\.\.je\],ue\.reset\(\{clearAssistantMessages:!1\}\),ue\.rebuildStreamingToolExecutor\(\)/);
assert.match(serverFallbackDispositionSource,
  /for\(let ke of M\.discardedMessages\)yield\{type:"tombstone",message:ke,displayOnly:!0\};for\(let ke of to\)yield\{type:"tombstone",message:ke,displayOnly:!0\}/);
assert.match(refusalFallbackRowSource,
  /if\(e!=="refusal"\)return;[\s\S]*cs\(\{content:w\+M\+V,error:"invalid_request"\}\);return he\.requestId=r\?\?void 0,he\.message\.stop_reason="refusal",he\.message\.stop_details=n\?\?null,he/);
assert.match(apiErrorRowConstructorsSource,
  /function Dyn\(\{content:e,isApiErrorMessage:n=!1.*type:"assistant",uuid:he\(\),timestamp:V\(\),message:\{diagnostics:null,id:he\(\),container:null,model:xa,role:"assistant",stop_details:null,stop_reason:"stop_sequence",stop_sequence:"",type:"message",usage:K,content:e,context_management:null\}/);
assert.match(apiErrorRowConstructorsSource,
  /function cs\(\{content:e,apiError:n.*isApiErrorMessage:!0,apiError:n.*error:h/);
assert.match(userMessageRowConstructorSource,
  /return\{type:"user",message:\{role:"user",content:e\|\|iu\}.*uuid:Ne\|\|\(Bt\?Bt\(\):vg\(\)\),timestamp:Ge\?\?\(Pt\?Pt\(\):new Date\(\)\.toISOString\(\)\)/);
assert.match(genericTombstoneDispatchSource,
  /if\(e\.type==="tombstone"\)\{s\?\.\(e\.message\);return\}/);
assert.match(interactiveTombstoneConsumerSource,
  /onTombstone:\(St\)=>\{if\(We\(\{type:"remove-by-uuid",uuid:St\.uuid\}\),this\.stream\.transcriptRetractedUuids\.delete\(St\.uuid\)\)return;xNt\(St\.uuid,Ne\)\}/);
assert.match(persistedTombstoneRemovalSource,
  /function xNt\(e,n\)\{let r=F\(\)&&n!==void 0\?n:void 0;await mi\(\)\.removeMessageByUuid\(e,r\)\}/);
assert.match(normalPostStreamToolDrainSource,
  /getRemainingResults\(\);try\{for await\(let h of fd\).*if\(h\.newContext\)he=\{\.\.\.h\.newContext,queryTracking:De\}/);
assert.ok(normalPostStreamToolDrainSource.indexOf('getRemainingResults()') <
  normalPostStreamToolDrainSource.indexOf('dx("PostToolBatch"'));
assert.match(capSource, /CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY\?\?10/);
assert.match(capDeclarationSource, /Li=M\.int\(\{min:1\}\)/);
assert.match(envProxySource,
  /get:\(\)=>\{let C=process\.env\[r\];if\(C!==n\)e=s\.parse\(C\),n=C;return e\}/);
assert.match(clockSource, /\.clock\.\$\{d\} takes a non-negative number of milliseconds/);
assert.match(clockSource, /t\(["']clock\.sleep["'],\{ms:y\},x\.signal\)/);
assert.match(clockHandlerSource, /ee\(e\.ms,n/);

const parserStart = envParserSource.indexOf('function i(n)');
assert.notEqual(parserStart, -1);
const parserExecutable = envParserSource.slice(parserStart);
const helperContext = vm.createContext({});
const helperStart = envBcSource.indexOf('var N=');
assert.notEqual(helperStart, -1);
vm.runInContext(envBcSource.slice(helperStart), helperContext);
const context = vm.createContext({
  Bc: helperContext.Bc,
  Le: () => false,
  Ss: () => false,
});
vm.runInContext(parserExecutable, context);
const nativeInt = context.M.int({min: 1});
const cases = [
  {label: 'missing', input: undefined, value: undefined, cap: 10},
  {label: 'empty', input: '', value: undefined, cap: 10},
  {label: 'whitespace', input: '   ', value: undefined, cap: 10},
  {label: 'zero', input: '0', value: undefined, cap: 10},
  {label: 'negative', input: '-1', value: undefined, cap: 10},
  {label: 'integer', input: '1', value: 1, cap: 1},
  {label: 'decimal-parseInt', input: '2.9', value: 2, cap: 2},
  {label: 'numeric-prefix', input: '12junk', value: 12, cap: 12},
  {label: 'scientific-integer', input: '1e3', value: 1000, cap: 1000},
  {label: 'scientific-negative-exponent', input: '10e-1', value: 1, cap: 1},
  {label: 'scientific-non-integer', input: '1e-1', value: undefined, cap: 10},
  {label: 'scientific-fractional-mantissa', input: '1.5e2', value: 150, cap: 150},
  {label: 'grouped-comma', input: '1,000', value: 1000, cap: 1000},
  {label: 'grouped-underscore', input: '1_000', value: 1000, cap: 1000},
  {label: 'mixed-groups-fall-through', input: '1_000,000', value: 1, cap: 1},
  {label: 'hex-prefix-parseInt-base10', input: '0x10', value: undefined, cap: 10},
  {label: 'invalid', input: 'not-a-number', value: undefined, cap: 10},
  {label: 'oversized-finite-number-no-cap', input: '9'.repeat(80), value: Number.parseInt('9'.repeat(80), 10), cap: Number.parseInt('9'.repeat(80), 10)},
];
const parserResults = cases.map(({label, input, value, cap}) => {
  const actual = nativeInt.parse(input);
  if (value === undefined) assert.equal(actual, undefined, `${label}: parser value`);
  else assert.equal(actual, value, `${label}: parser value`);
  const effective = actual ?? 10;
  assert.equal(effective, cap, `${label}: effective cap`);
  return {label, input: input === undefined ? '<undefined>' : input,
    value: actual === undefined ? '<undefined>' : actual, effective_cap: effective};
});

const output = {
  native_binary_sha256: sha256(binary),
  source_windows: sourceWindows,
  session_start: {
    explicit_native_dispatcher_occurrences: 1,
    dispatcher: 'main process startup only; async Agent launch path is N5/MC and has no second session.start dispatcher call',
    fixture_guard: 'module-scoped spawnedOnce; set before agent.spawn; later same-module session.start invocation returns next(event)',
  },
  stream_order: {
    row: 'ContentBlockStop yields a completed assistant row with current block, outer UUID, timestamp, and API block index before message_delta/message_stop.',
    terminal_row_metadata: 'The producer resets wp per response, pushes every completed content-block row into wp, then on message_delta updates usage and terminal fields by iterating every row in wp; it is not limited to the last row.',
    order: ['session.append side effect is awaited', 'assistant projection is yielded', 'on generator resume, the query stores X=eo??M and enqueues X tool_use blocks into streamingToolExecutor', 'executor invokes W1 with the stop-time assistant row separately from the pre-query ToolUseContext.messages snapshot'],
    append_result: 'Ui.appended awaits append side effects but returns no accepted replacement row; the query uses its native raw/projection row for tool enqueue. This is the pinned Native behavior.',
    next_request_history: 'The selected query row X=eo??M is pushed to K. At the next turn boundary H.messages is [...w,...K,...je]; the next round derives w from H.messages and passes its prompt-built Zt.messages to callModel. The session.append projection result is not assigned into K/H. Normal next-round history preparation can still transform those rows.',
    sibling_context: 'sameTurnToolUses contains prior sibling blocks in the executor only; the current block is a separate W1 argument and later sibling blocks are not included.',
    live_tool_settlement: 'After each stream event the query polls Tn, which emits completed results without waiting for unfinished tools; the final getRemainingResults wait occurs after the API stream loop. Executor context layers can update its internal context on completion, while query-local he adopts newContext from the final drain before PostToolBatch.',
  },
  jsonl_persistence: {
    path_scope: 'The TS recorder below is the headless turn-consumer path. The interactive transcript UI has a separate MSe/ASe recorder; it uses the same NE/transcript-writer path, but not TS.record().',
    assistant_row_recording: 'With persistSession enabled, the headless consumer pushes each assistant row into its current transcript and calls TS.record() immediately on row arrival, after the query append side effect. That call is not awaited. Default record() selects all currently present rows: its default ge=false makes ANt(...,!ge) return the full list, including an assistant row whose stop_reason is still null. The TS cursor advances before NE() is called; the later message_delta record call does not select the same row again.',
    query_append_scope: 'The main query wraps Xl in Ui.through (also through the observer wrapper). Ui.R accepts uuid/type rows of type user, assistant, attachment, or system and calls Xq when session.append is enabled and the running conversation is hookable. Thus the Tn user/tool-result row also passes through session.append before it is yielded to the headless consumer. Ui.through awaits that side effect, then yields the original event value; the hook projection is not substituted into the query event. The explicit r.appended(M) call remains a separate assistant-tool-use path, with Ui deduplicating the same row through its WeakSet.',
    live_tool_result_recording: 'A tool_result is delivered as a user row. After Ui.through has completed the session.append side effect and yielded the original row, the headless user-row branch pushes it into the current transcript, awaits TS.record(), and only then continues consuming the stream. Therefore a completed tool result may be persisted while the provider stream is still open; the writer receives the original Native tool-result row, not the session.append projection.',
    writer_path: 'TS.record() calls NE(), which normalizes the selected rows and invokes mi().insertMessageChain(). The writer awaits session-message reads, creates an outer transcript envelope by spreading each source row, then appendEntry enqueues that entry. The per-file queue preserves FIFO after admission; drain serializes with LM(entry) and appends the serialized chunk. Its scheduled flush interval is 100 ms.',
    snapshot_limit: 'Static source proves an early write attempt, not whether its eventual JSONL bytes contain stop_reason:null or the terminal value. The row unwrap and transcript envelope are shallow spreads that preserve the nested message object; MessageDelta later mutates that same message object in the parser. Because the assistant record is not awaited and JSONL serialization happens later in the queue drain, either snapshot can win. The assistant and tool-result insertions also have async reads before queue admission, so this source alone does not guarantee their queue-admission order across separate record calls.',
    interactive_cutoff: 'The interactive MSe.write path calls the same NE writer but passes isResponseInFlight to ANt. While that flag is true, ANt stops before the first assistant row whose stop_reason is null, so the incomplete trailing assistant row is held out of the persisted slice. ASe invokes MSe.record from transcript-snapshot and response-state effects. This pins the in-flight cutoff, not the precise scheduler moment when a post-delta row is flushed.',
    tombstone_path: 'The headless tombstone branch calls TS.evict(message.uuid). If that row has passed the record cursor, TS.evict requests removeMessageByUuid; accepted fallback tombstones can therefore remove an already-recorded discarded assistant row. This does not establish that every error/decline path tombstones every earlier row.',
  },
  stream_error: {
    disposition: 'A final thrown model-stream error with no fallback-chain target reaches the model-error finalizer.',
    completed_tools: 'The finalizer emits already completed results, takes any remaining finished results, and records ordinary user/tool-result rows.',
    in_flight_tools: 'It discards the executor, aborts executing tools, leaves queued tasks unstarted, rebuilds the executor, and emits removals for in-progress IDs.',
    unmatched_uses: 'It emits a synthetic cancelled tool_result for each remaining unpaired tool_use; the cancellation text warns that already-started effects may have happened.',
    retry: 'This finalizer does not keep tool executions live for a model retry. A separate fallback-chain attempt uses abandonAttempt and its own tombstone/reset/abort path.',
  },
  server_fallback: {
    scope: 'The allowlist decline is inside the server_fallback event branch and runs only when the projection is userVisible; the target check is uA(toModel) ?? (Ur(toModel) || ay(toModel)).',
    prior_results: 'je aliases ue.toolResults. Rows already handed off by earlier per-event Tn polls live in je; when a normal poll yields a result, xr appends its projected user rows after the yielded event is resumed. xr also preserves attachment rows and user rows returned by the normal tool-result projection. A tool that completed after the previous poll is not proven to be in je yet.',
    decline: 'The branch snapshots We = je.slice(), then ue.reset({clearAssistantMessages:false}) clears the original je array. After pruning K only for discarded assistant UUIDs and rows whose message.model equals toModel, it tombstones those K rows and every row in We, removes in-progress tool IDs, appends the synthetic decline assistant row, pushes that same row into K, yields it, and breaks out of the event loop. Decline tombstones do not carry displayOnly.',
    accepted: 'The accepted branch always prunes K by M.discardedMessages UUID. It clears no je entries unless tombstonedToolUse is true; in that case to=[...je] snapshots every current result/attachment entry, reset empties je, the executor is discarded/rebuilt, and every row in to is tombstoned with displayOnly:true. If tombstonedToolUse is false, je remains intact. This is whole-attempt je cleanup, not a filter by discarded tool IDs.',
    ordinary_abandon: 'abandonAttempt is a separate cleanup: it tombstones all current assistantMessages and toolResults, then reset(clearAssistantMessages:true); do not infer this broader cleanup for server_fallback.',
    synthetic_decline_row: {
      refusal_call: "$dt('refusal', {type:'refusal', category:M.apiRefusalCategory??null, explanation:null, fallback_credit_token:null, fallback_has_prefill_claim:null, recommended_model:null}, M.requestId, ge)",
      refusal_result: 'Native refusal helper returns an assistant API-error row with error=invalid_request, requestId from the event, stop_reason=refusal, and stop_details equal to the refusal metadata; it is not an SDK LlmError. The helper generates its own default timestamp/UUIDs and refusal-facing text.',
      other_reason_call: 'cs({content:"The server routed this response to a model that is not in your organization’s availableModels allowlist; the response was discarded.", error:"invalid_request", now:B.now, uuid:B.uuid})',
      other_reason_result: 'The generic Native cs/Dyn constructor returns an assistant row with isApiErrorMessage=true, error=invalid_request, one text block, native model xa, stop_reason=stop_sequence, empty stop_sequence, default usage, outer timestamp from B.now(), and separate generated outer uuid/message.id.',
      append_and_yield: 'If the helper returns a row, r.appended(Ro) is awaited first; then the exact Ro row is pushed into K and yielded.',
    },
    tombstone_consumers: 'Accepted fallback sets displayOnly:true on both discarded assistant rows and every je entry copied into to. The generic event dispatcher passes only e.message to the callback, dropping displayOnly. The headless consumer calls xr.evict(message.uuid) without a displayOnly check; the interactive callback removes by UUID and calls xNt, which calls removeMessageByUuid. MSe/ASe are snapshot recorders, not the tombstone deletion gate.',
    flow_boundary: 'The decline break and accepted continue bypass the common per-event yield*Tn() at the bottom of the stream loop. The sa wrapper finally calls the underlying iterator.return() but ignores its return value and emits no finally event; any underlying cleanup side effects remain outside this source claim.',
  },
  concurrency_cap: {
    source_expression: 'a.CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY ?? 10',
    typed_source: 'M.int({min:1})',
    parser: 'finite integer environment parser; invalid or <1 values are undefined and therefore use 10; no upper clamp.',
    parser_results: parserResults,
  },
  clock_sleep: {
    method: '$.clock.sleep(ms, options?)',
    validation: 'finite non-negative milliseconds; cancellation signal is forwarded to clock.sleep',
    fixture_wait_ms: 3500,
    local_stub_wait: 'Node setTimeout only delays the loopback child response; it does not emulate Native clock.sleep.',
  },
};
console.log(JSON.stringify(output, null, 2));
