#!/usr/bin/env node
// Execute SDK codec, schemas, handlers and multi-round driver extracted from
// the supplied SHA-pinned native artifact. No native source is vendored.
// Usage: node scripts/tests/mcp_input_required_286_oracle.mjs /path/to/2.1.286 [--check]
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const hash = value => createHash('sha256').update(value).digest('hex');
const binarySha = hash(binary);
assert.equal(binarySha, '75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433');
const sourceRanges = [];
const sdkOffset = 209701831;
function chunk(offset) {
  const end = binary.indexOf('// @bun @bytecode\n', offset + 1);
  const bytes = binary.subarray(offset, end < 0 ? binary.length : end);
  const nul = bytes.indexOf(0, Math.max(bytes.lastIndexOf('export{'), 0));
  return bytes.subarray(0, nul < 0 ? bytes.length : nul);
}
function range(startMarker, endMarker, searchOffset = 0) {
  const bytes = chunk(sdkOffset);
  const start = bytes.indexOf(startMarker, searchOffset);
  const end = bytes.indexOf(endMarker, start + startMarker.length);
  assert(start >= 0 && end > start, `Native source range missing: ${startMarker}`);
  const source = bytes.subarray(start, end);
  sourceRanges.push({ signature: startMarker, start: sdkOffset + start, end: sdkOffset + end, sha256: hash(source) });
  return source.toString('utf8');
}
function pureModule(offset, imports = {}) {
  const bytes = chunk(offset);
  sourceRanges.push({ signature: 'complete bundled pure schema module', start: offset, end: offset + bytes.length, sha256: hash(bytes) });
  const source = bytes.toString('utf8').replace(/import\{[^}]+\}from"[^"]+";/g, '')
    .replace(/export\{([^}]+)\};/, 'return {$1};');
  return vm.runInNewContext(`(()=>{${source}})()`, { URL, URLSearchParams, ...imports }, { timeout: 2_000 });
}
const schemaCore = pureModule(176381936);
const schema = pureModule(176393579, schemaCore);
const schemaDates = pureModule(177781187, schema);
const source = [
  range('var Hr=Symbol.for(', 'function xs('),
  range('var Js="2026-07-28"', 'function Gs('),
  range('var Xs={', 'function Ao(){'),
  range('function Ao(){', 'var Lo;'),
  range('var Lo;', 'var Uo='),
  range('var Hs=function(', ',Fo=0') + ';',
  range('var Qo=[', 'var $r='),
  range('var $r=', 'var da=') + 'var da=[Xr,es];',
  range('var ha=(e)=>', 'var ts='),
  range('function Kr(', 'var Ls='),
  range('function $n(', 'function En('),
  range('var Un=!0', 'var Gn='),
  range('var Bt=60000', 'var Qn='),
  range('function Rr(', '/*!'),
  range('function ul(', 'function dl('),
].join('\n');
const methods = [
  range('_envelopeOutbound(e){', '_resolveNonCompleteResult('),
  range('_getRequestHandler(e){', 'async _oncancel('),
  range('_setupTimeout(e,t,r,s,a=!1){', 'async connect('),
  range('_onprogress(e){', '_onresponse('),
  range('_onresponse(e){', 'get transport(){'),
  range('request(e,t,r){', 'async notification('),
  range('setRequestHandler(e,t,r){', '_outboundServerInfo('),
  range('_resolveNonCompleteResult(e,t){', '_setupListChangedHandlers(', 270_000),
  range('_wrapHandler(e,t){', 'assertCapability(e,t){', 270_000),
  range('assertRequestHandlerCapability(e){', 'async ping(', 270_000),
].join('\n');

class ControlledClock {
  constructor() { this.now = 0; this.nextId = 1; this.timers = new Map(); this.delays = []; }
  schedule(fn, delay, kind = 'native') {
    const id = this.nextId++;
    this.timers.set(id, { fn, time: this.now + delay, kind });
    this.delays.push({ kind, delay });
    return id;
  }
  clear(id) { this.timers.delete(id); }
  advance() {
    const next = [...this.timers].sort((a, b) => a[1].time - b[1].time || a[0] - b[0])[0];
    if (!next) return false;
    this.now = next[1].time; this.timers.delete(next[0]); next[1].fn(); return true;
  }
  async settle(promise) {
    let settled = false, value, error;
    promise.then(result => { settled = true; value = result; }, failure => { settled = true; error = failure; });
    for (let iteration = 0; iteration < 300; iteration++) {
      // Drain native async/schema/promise continuations before firing a timer.
      for (let tick = 0; tick < 80; tick++) await Promise.resolve();
      if (settled) { if (error) throw error; return value; }
      if (!this.advance()) throw Error('Native scenario stalled without a registered timer');
    }
    throw Error('Native scenario exceeded the controlled scheduler bound');
  }
}
const clock = new ControlledClock();
const harness = `
const definitions=Qe();
// Controlled transport emits only result frames. Use the native modern frame
// schema for classification; protocol-error frame plumbing is outside scope.
const $ve=e=>definitions.JSONRPCResultResponseSchema.safeParse(e).success;
class Client {
 constructor(test){this.test=test;this._negotiatedProtocolVersion=$r;this._options={};
  this._requestMessageId=0;this._requestHandlers=new Map();this._responseHandlers=new Map();
  this._progressHandlers=new Map();this._timeoutInfo=new Map();this._capabilities=test.capabilities??{};
  this._inputRequiredDriverConfig=_a(test.input_required_config);this.requests=[];this.handlerEvents=[];
  this.progress=[];this.errors=[];this.controller=new AbortController();this.results=[...test.server_results];
  this._transport={sessionId:'native-fixture-session',hasPerRequestStream:test.per_request_stream??false,
   send:async(request,options)=>{this.requests.push({request:copy(request),options:{
    ...options.headers!==undefined&&{headers:copy(options.headers)},
    ...options.requestSignal&&{request_signal:true}}});
    if(request.method==='notifications/cancelled')return;
    options.requestSignal?.addEventListener('abort',()=>this.handlerEvents.push({event:'transport-abort',id:request.id}),{once:true});
    const result=this.results.shift();if(result===undefined)throw Error('Missing controlled server result');
    for(const notice of result.progress??[])clock.schedule(()=>this._onprogress({params:{progressToken:request.id,...notice.value}}),notice.after,'server-progress');
    if(!result.never)clock.schedule(()=>this._onresponse({jsonrpc:'2.0',id:request.id,result:result.payload??result}),result.after??0,'server-response');
   }};
  for(const [method,handler]of Object.entries(test.handlers??{}))this.setRequestHandler(method,async(request,ctx)=>{
   this.handlerEvents.push({event:'start',method,id:ctx.mcpReq.id,validated_request:copy(request),params:copy(request.params),session_id:ctx.sessionId,
    signal_aborted:ctx.mcpReq.signal.aborted,request_state:ctx.mcpReq.requestState()??null});
   if(handler.call_unavailable)ctx.mcpReq[handler.call_unavailable]();
   if(handler.delay!==undefined)await new Promise((resolve,reject)=>{
    const timer=clock.schedule(resolve,handler.delay,'handler');
    const abort=()=>{clock.clear(timer);this.handlerEvents.push({event:'abort',method,id:ctx.mcpReq.id});reject(ctx.mcpReq.signal.reason)};
    if(ctx.mcpReq.signal.aborted)abort();else ctx.mcpReq.signal.addEventListener('abort',abort,{once:true});
   });
   if(handler.fail){this.handlerEvents.push({event:'fail',method,id:ctx.mcpReq.id});throw Error(handler.fail)}
   this.handlerEvents.push({event:'finish',method,id:ctx.mcpReq.id});return handler.result;
  });
 }
 get transport(){return this._transport}
 buildContext(ctx){return ctx}
 _outboundMetaEnvelope(){return es.outboundEnvelope({protocolVersion:$r,clientInfo:{name:'oracle',version:'1'},clientCapabilities:this._capabilities})}
 _onerror(error){this.errors.push(error.message)}
 ${methods}
}
function errorShape(error){return{name:error.name??'Error',...error.code!==undefined&&{code:error.code},message:error.message??String(error),...error.data!==undefined&&{data:copy(error.data)}}}
function copy(value){return value===undefined?undefined:JSON.parse(JSON.stringify(value))}
function decode(test){let value=es.decodeResult(test.method,test.payload);return value.kind==='invalid'?{kind:value.kind,error:errorShape(value.error)}:copy(value)}
function schemaCheck(test){const value=definitions[test.schema].safeParse(test.payload);return{valid:value.success,...value.success&&{value:copy(value.data)}}}
async function flow(test){let client;try{client=new Client(test)}catch(error){return{error:errorShape(error),requests:[],handler_events:[],progress:[],timer_delays:copy(clock.delays),elapsed_ms:clock.now,errors:[]}}
 const options={...test.options,signal:client.controller.signal,
  ...test.progress_enabled&&{onprogress:value=>client.progress.push(copy(value))}};
 if(test.cancel_after!==undefined)clock.schedule(()=>client.controller.abort(Error('cancelled by owner')),test.cancel_after,'owner-cancel');
 let result,error;try{result=await client.request(test.original_request,options)}catch(failure){error=errorShape(failure)}
 return{...result!==undefined&&{result:copy(result)},...error&&{error},requests:client.requests,handler_events:client.handlerEvents,
  progress:client.progress,timer_delays:copy(clock.delays),elapsed_ms:clock.now,errors:client.errors};
}
({decode,schemaCheck,flow,defaults:()=>_a(),rebuild:(test)=>copy(Fn(test.original,test.responses,test.state))});`;
let oracle;
try {
  oracle = vm.runInNewContext(`${source}\n${harness}`, { ...schema, ...schemaDates,
    VDe:'io.modelcontextprotocol/protocolVersion',QRt:'io.modelcontextprotocol/clientInfo',
    qDe:'io.modelcontextprotocol/serverInfo',Ume:'io.modelcontextprotocol/clientCapabilities',
    ZRt:'io.modelcontextprotocol/logLevel',sZt:'io.modelcontextprotocol/subscriptionId',
    AbortController, URL, atob, btoa, clock, Date: { now: () => clock.now },
    setTimeout: (fn, delay) => clock.schedule(fn, delay), clearTimeout: id => clock.clear(id),
  }, { timeout: 3_000 });
} catch (error) {
  throw new Error(`Native extraction evaluation failed: ${error.message}`);
}
const plain = value => JSON.parse(JSON.stringify(value));
const complete = { resultType: 'complete', content: [{ type: 'text', text: 'done' }] };
const roots = { method: 'roots/list' };
const elicitation = { method: 'elicitation/create', params: { message: 'Choose', requestedSchema: { type: 'object', properties: {} } } };
const sampling = { method: 'sampling/createMessage', params: { messages: [{ role: 'user', content: { type: 'text', text: 'Question' } }], maxTokens: 10 } };
const input = (inputRequests, requestState) => ({ resultType: 'input_required', ...(inputRequests!==undefined&&{inputRequests}), ...(requestState!==undefined&&{requestState}) });
const decodeCases = [
  ['complete', complete], ['missing-result-type', { content: [] }], ['null-result', null], ['array-result', []],
  ['non-string-result-type', { resultType: 7 }], ['unsupported-result-type', { resultType: 'waiting' }],
  ['empty-input', input({})], ['missing-input', input()], ['array-input', input([])],
  ['non-object-input', input('bad')], ['state-only', input(undefined, 'state-1')], ['empty-state', input(undefined, '')],
  ['array-input-with-state', input([], 'state-1')], ['wrong-state-with-input', input({ a: roots }, 1)],
  ['nonempty-input', input({ a: roots })], ['deep-validation-deferred', input({ a: null })],
  ['complete-invalid-content', { resultType: 'complete', content: 'bad' }],
  ['task-codec-only', { resultType: 'task', taskId: 't' }],
].map(([name,payload])=>({name,method:'tools/call',payload}));
decodeCases.push(
  ...['2024-02-29T12:30Z','2024-02-29T12:30:01.12+08:00','2023-02-29T12:30Z','2024-02-29T12:30:01+99:99','not-a-date'].map(lastModified=>({name:`annotation-datetime-${lastModified}`,method:'tools/call',payload:{resultType:'complete',content:[{type:'text',text:'done',annotations:{lastModified}}]}})),
  ...['2.0','9007199254740992'].map(ttl=>{const raw_json=`{"resultType":"complete","ttlMs":${ttl},"cacheScope":"private","tools":[]}`;return{name:`list-ttl-lexical-${ttl}`,method:'tools/list',raw_json,payload:JSON.parse(raw_json)}}),
);
const numericOrderRaw='{"resultType":"complete","content":[],"structuredContent":{"number":2.0},"_meta":{"number":10.0}}';
decodeCases.push({name:'complete-numeric-decode-field-order',method:'tools/call',raw_json:numericOrderRaw,payload:JSON.parse(numericOrderRaw)});
for (const test of decodeCases) {test.expected = plain(oracle.decode(test));if(test.expected.kind==='complete')test.expected_result_json=JSON.stringify(test.expected.result);}
const schemaCases = [
  ['input-request-roots','InputRequestSchema',roots], ['input-request-elicitation','InputRequestSchema',elicitation],
  ['input-request-sampling','InputRequestSchema',sampling], ['input-request-unknown','InputRequestSchema',{method:'tools/call',params:{name:'x'}}],
  ['input-request-null','InputRequestSchema',null], ['sampling-missing-tokens','InputRequestSchema',{method:'sampling/createMessage',params:{messages:[]}}],
  ['elicitation-wrong-schema','InputRequestSchema',{method:'elicitation/create',params:{message:'q',requestedSchema:'bad'}}],
  ['input-result-state-only','InputRequiredResultSchema',input(undefined,'s')],
  ['input-result-invalid-embedded','InputRequiredResultSchema',input({bad:{method:'tools/call'}})],
  ['roots-response','InputResponseSchema',{roots:[]}], ['invalid-response','InputResponseSchema',{answer:1}],
].map(([name,schema,payload])=>({name,schema,payload}));
schemaCases.push(
  ...[null,'bad',[]].map(params=>({name:`roots-invalid-params-${JSON.stringify(params)}`,schema:'InputRequestSchema',payload:{method:'roots/list',params}})),
  {name:'elicitation-unicode17-url-constructor-boundary',schema:'InputRequestSchema',payload:{method:'elicitation/create',params:{mode:'url',message:'Open',url:`https://${String.fromCodePoint(0x323b0)}.example`}}},
);
schemaCases.push(...[{type:'string',enum:1},{type:'string',enum:['a'],format:'bogus'},{type:'string',minLength:'bad',enum:['a']}].map((property,index)=>({name:`elicitation-string-union-fallback-${index}`,schema:'InputRequestSchema',payload:{method:'elicitation/create',params:{message:'q',requestedSchema:{type:'object',properties:{a:property}}}}})));
for (const test of schemaCases) test.expected=plain(oracle.schemaCheck(test));
const handler = result => ({ result });
const base = { original_request:{method:'tools/call',params:{name:'worker',arguments:{q:'original'},_meta:{keep:'metadata'}}},
  capabilities:{roots:{},elicitation:{form:{}},sampling:{}}, handlers:{'roots/list':handler({roots:[]})} };
const flowInputs = [
  {name:'complete-without-fulfilment',server_results:[complete]},
  {name:'successful-request-stream-is-not-aborted',per_request_stream:true,server_results:[complete]},
  {name:'successful-input-required-stream-is-not-aborted',per_request_stream:true,server_results:[input({r:roots}),complete]},
  {name:'integer-index-input-key-order',server_results:[input(Object.fromEntries(['2','1','x','0','4294967294','4294967295'].map(key=>[key,roots]))),complete]},
  {name:'non-index-input-key-order',server_results:[input(Object.fromEntries(['01','-1','1.0','4294967295','a'].map(key=>[key,roots]))),complete]},
  {name:'complete-invalid-schema',server_results:[{resultType:'complete',content:'bad'}]},
  {name:'state-only-250ms',server_results:[input(undefined,'opaque-1'),complete],progress_enabled:true,options:{headers:{'Mcp-Param-Custom':'retained'},timeout:5000,maxTotalTimeout:1000}},
  {name:'resources-read-method-preserved',original_request:{method:'resources/read',params:{uri:'file:///original'}},server_results:[input({r:roots},'read-state'),{resultType:'complete',ttlMs:0,cacheScope:'private',contents:[{uri:'file:///original',text:'done'}]}]},
  {name:'two-round-answer-replacement',server_results:[input({first:roots},'round-1'),input({second:roots},'round-2'),complete],progress_enabled:true},
  {name:'round-state-is-not-accumulated',original_request:{method:'tools/call',params:{name:'worker',requestState:'original-state',inputResponses:{original:{roots:[]}}}},server_results:[input({first:roots},'round-1'),input({second:roots}),complete]},
  {name:'parallel-three-local-handlers',server_results:[input({r:roots,e:elicitation,s:sampling}),complete],handlers:{
    'roots/list':{result:{roots:[]},delay:5},'elicitation/create':{result:{action:'accept',content:{}},delay:20},
    'sampling/createMessage':{result:{role:'assistant',model:'sample',content:{type:'text',text:'reply'}},delay:10}}},
  {name:'one-failure-cancels-sibling',server_results:[input({fail:roots,wait:sampling})],handlers:{'roots/list':{fail:'handler failed',delay:5},'sampling/createMessage':{result:{role:'assistant',model:'sample',content:{type:'text',text:'reply'}},delay:100}}},
  {name:'missing-handler',server_results:[input({e:elicitation})]},
  {name:'registration-requires-capability',capabilities:{},server_results:[input({r:roots})]},
  {name:'elicitation-url-requires-mode-capability',server_results:[input({e:{method:'elicitation/create',params:{mode:'url',message:'Open',url:'https://example.test'}}})],handlers:{'elicitation/create':handler({action:'cancel'})}},
  {name:'sampling-result-schema-is-native',server_results:[input({s:sampling})],handlers:{'sampling/createMessage':handler({role:'assistant',model:1,content:{type:'text',text:'bad'}})}},
  {name:'unsupported-embedded-method',server_results:[input({bad:{method:'tools/call',params:{name:'x'}}})]},
  {name:'invalid-embedded-object',server_results:[input({bad:null})]},
  {name:'invalid-embedded-params-schema',server_results:[input({bad:{method:'sampling/createMessage',params:{messages:[]}}})],handlers:{'sampling/createMessage':handler({role:'assistant',model:'s',content:{type:'text',text:'x'}})}},
  {name:'handler-context-send-unavailable',server_results:[input({r:roots})],handlers:{'roots/list':{call_unavailable:'send',result:{roots:[]}}}},
  {name:'default-ten-round-limit',server_results:Array.from({length:11},()=>input({r:roots},'state')),progress_enabled:true},
  {name:'configured-two-round-limit',input_required_config:{maxRounds:2},server_results:Array.from({length:3},()=>input({r:roots}))},
  {name:'auto-fulfilment-disabled',input_required_config:{autoFulfill:false},server_results:[input({r:roots})]},
  {name:'explicit-manual-input-required',options:{allowInputRequired:true},server_results:[input({r:roots},'manual')]},
  {name:'state-only-total-timeout',server_results:[input(undefined,'1'),input(undefined,'2')],options:{maxTotalTimeout:400}},
  {name:'handler-time-consumes-total-budget',server_results:[input({r:roots})],handlers:{'roots/list':{result:{roots:[]},delay:600}},options:{maxTotalTimeout:500}},
  {name:'owner-cancel-between-rounds',server_results:[input(undefined,'1')],cancel_after:30},
  {name:'owner-cancel-local-handler',server_results:[input({r:roots})],handlers:{'roots/list':{result:{roots:[]},delay:100}},cancel_after:30},
  {name:'retry-request-timeout-preserves-cancellation',server_results:[input({r:roots}),{never:true}],options:{timeout:40},progress_enabled:true},
  {name:'retry-stream-timeout-aborts-stream',server_results:[input({r:roots}),{never:true}],options:{timeout:40},per_request_stream:true},
  {name:'native-progress-reset-timeout',server_results:[input({r:roots}),{payload:complete,after:70,progress:[{after:30,value:{progress:1,message:'working'}}]}],options:{timeout:50,resetTimeoutOnProgress:true},progress_enabled:true},
  {name:'native-progress-total-budget-refusal',server_results:[input({r:roots}),{payload:complete,after:80,progress:[{after:70,value:{progress:1,message:'working'}}]}],options:{timeout:100,maxTotalTimeout:60,resetTimeoutOnProgress:true},progress_enabled:true},
  {name:'missing-handler-before-params-schema',server_results:[input({s:{method:'sampling/createMessage',params:{messages:[]}}})]},
  {name:'unicode17-url-controlled-constructor-boundary',capabilities:{elicitation:{url:{}}},server_results:[input({e:{method:'elicitation/create',params:{mode:'url',message:'Open',url:`https://${String.fromCodePoint(0x323b0)}.example`}}})],handlers:{'elicitation/create':handler({action:'cancel'})}},
  {name:'task-result-remains-disabled',server_results:[{resultType:'task',taskId:'task-1'}]},
];
flowInputs.push(
 {name:'roots-handler-retains-common-wire-params',server_results:[input({r:{method:'roots/list',params:{extra:'keep',_meta:'not-object'}}}),complete]},
 {name:'roots-handler-schema-projection',server_results:[input({r:{method:'roots/list',params:{extra:'drop',_meta:{kept:true}}}}),complete]},
 {name:'elicitation-handler-schema-projection',server_results:[input({e:{method:'elicitation/create',params:{extra:'drop',message:'q',mode:'form',task:{ttl:100,extra:'drop'},_meta:{kept:true},requestedSchema:{type:'object',properties:{a:{type:'string',enum:1,extra:'drop'}},required:['a'],'x-retained':'yes'}}}}),complete],handlers:{'elicitation/create':handler({content:{},action:'cancel',extra:'drop'})}},
 {name:'sampling-handler-and-result-schema-projection',server_results:[input({s:{method:'sampling/createMessage',params:{extra:'drop',maxTokens:2,messages:[{role:'user',extra:'drop',content:{type:'text',text:'Q',extra:'drop',annotations:{priority:0.5,extra:'drop'}}}],modelPreferences:{hints:[{name:'preferred',extra:'drop'}],costPriority:0.5,extra:'drop'}}}}),complete],handlers:{'sampling/createMessage':handler({extra:'drop',role:'assistant',model:'sample',content:{type:'text',text:'reply',extra:'drop',annotations:{priority:0.1,extra:'drop'}},stopReason:'end'})}},
);
flowInputs.push({name:'complete-numeric-public-field-order',raw_json:numericOrderRaw,server_results:[JSON.parse(numericOrderRaw)]});
const flowCases=[];
for(const test of flowInputs){
  clock.now=0;clock.nextId=1;clock.timers.clear();clock.delays=[];
  const full={...base,...test,handlers:test.handlers??base.handlers};
  full.expected=plain(await clock.settle(oracle.flow(full)));
  if(full.expected.result!==undefined)full.expected_result_json=JSON.stringify(full.expected.result);
  flowCases.push(full);
}
assert.deepEqual(plain(oracle.defaults()),{autoFulfill:true,maxRounds:10});
const byName=name=>flowCases.find(test=>test.name===name).expected;
assert.equal(byName('two-round-answer-replacement').requests[2].request.params.inputResponses.first,undefined);
assert.equal(byName('two-round-answer-replacement').requests[2].request.params.inputResponses.second.roots.length,0);
assert.equal(byName('round-state-is-not-accumulated').requests[2].request.params.requestState,'original-state');
assert.equal(byName('state-only-250ms').elapsed_ms,250);
assert(byName('resources-read-method-preserved').requests.every(call=>call.request.method==='resources/read'));
assert.equal(byName('resources-read-method-preserved').requests[1].request.params.uri,'file:///original');
assert.deepEqual(byName('state-only-250ms').requests[0].options.headers,byName('state-only-250ms').requests[1].options.headers);
assert.equal(byName('state-only-250ms').requests[1].request.params._meta.keep,'metadata');
assert.equal(byName('state-only-250ms').requests[1].request.params._meta.progressToken,1);
assert(byName('state-only-250ms').timer_delays.some(timer=>timer.kind==='native'&&timer.delay===250));
assert.equal(byName('parallel-three-local-handlers').elapsed_ms,20);
assert.equal(byName('parallel-three-local-handlers').handler_events.slice(0,3).filter(event=>event.event==='start').length,3);
assert.equal(byName('default-ten-round-limit').requests.length,11);
assert.equal(byName('default-ten-round-limit').error.code,'INPUT_REQUIRED_ROUNDS_EXCEEDED');
assert(byName('one-failure-cancels-sibling').handler_events.some(event=>event.event==='abort'&&event.id==='wait'),JSON.stringify(byName('one-failure-cancels-sibling')));
assert.equal(byName('missing-handler').error.code,'CAPABILITY_NOT_SUPPORTED');
assert.equal(byName('unsupported-embedded-method').error.code,'INVALID_RESULT');
assert.equal(byName('registration-requires-capability').error.code,'CAPABILITY_NOT_SUPPORTED');
assert.equal(byName('state-only-total-timeout').error.code,'REQUEST_TIMEOUT');
assert.equal(byName('state-only-total-timeout').elapsed_ms,500);
assert.equal(byName('handler-time-consumes-total-budget').elapsed_ms,600);
assert.equal(byName('native-progress-reset-timeout').elapsed_ms,70);
assert.equal(byName('native-progress-total-budget-refusal').error.code,'REQUEST_TIMEOUT');
assert.equal(byName('retry-request-timeout-preserves-cancellation').requests.at(-1).request.method,'notifications/cancelled');
assert.equal(byName('task-result-remains-disabled').error.code,'UNSUPPORTED_RESULT_TYPE');
const fixture={version:'2.1.286',binary_sha256:binarySha,source_byte_ranges:sourceRanges,
 coverage:{native_binary_cli_executed:false,native_tasks_feature_enabled:false,production_rust_executed:false,
  actual_native:['bundled pure schema modules','Ao schema factory','es modern codec','_a/Fn/xn/Kn/Jn driver','si/ai/Ea embedded dispatch','Client _resolveNonCompleteResult','Client _wrapHandler','request/response/progress/timeout/handler-registration methods'],
  controlled:['in-memory transport result frames and progress notifications','handler callbacks and external aborts','virtual Date.now/setTimeout/clearTimeout clock','modern result-frame classification via native JSONRPCResultResponseSchema','plain identity buildContext and modern envelope configuration','Node WHATWG URL/URLSearchParams constructor; IDNA Unicode tables are outside native Bun runtime proof'],
  not_executed:['CLI binary process','network/stdio transports','legacy schema validation beyond method membership','elicitation applyDefaults=true branch','JSON-RPC peer error frames','Tasks extension gated by tengu_mcp_tasks']},
 safe_integral_number_wire_cases:[
   '{"resultType":"complete","content":[],"structuredContent":{"number":2.0,"negativeZero":-0.0},"_meta":{"number":10.0}}',
   '{"method":"roots/list","params":{"number":2.0,"_meta":{"number":10.0}}}',
   '{"name":"worker","arguments":{"number":2.0},"inputResponses":{"r":{"roots":[],"number":3.0}}}',
 ].map(raw_json=>({raw_json,canonical_json:JSON.stringify(JSON.parse(raw_json))})),
 defaults:plain(oracle.defaults()),decode_cases:decodeCases,schema_cases:schemaCases,flow_cases:flowCases};
const output=JSON.stringify(fixture,null,2)+'\n';
const fixturePath=fileURLToPath(new URL('../../crates/mcp/tests/fixtures/input_required_2_1_286.json',import.meta.url));
if(process.argv.includes('--check'))assert.equal(readFileSync(fixturePath,'utf8'),output,'MCP input_required native fixture drift');
else{mkdirSync(dirname(fixturePath),{recursive:true});writeFileSync(fixturePath,output)}
console.log(`Verified native input_required: ${decodeCases.length} codec, ${schemaCases.length} schema, ${flowCases.length} flow cases (${binarySha})`);
