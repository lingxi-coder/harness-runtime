// Execute Native 2.1.296 query extraction and default fork-cache marker functions.
// Provider/network, transcript storage, and tool execution are replaced by inert seams.
import fs from 'node:fs';
import vm from 'node:vm';
import crypto from 'node:crypto';
import assert from 'node:assert/strict';
const bytes = fs.readFileSync(`${process.env.HOME}/.local/share/claude/versions/2.1.296`);
const sha = crypto.createHash('sha256').update(bytes).digest('hex');
assert.equal(sha, 'c9b5341637becbd423ddffc5b254afb645682a3868cb708bbc6cc0e7bb419937');
const suggestionAt = bytes.indexOf('[SUGGESTION MODE:');
const suggestion = bytes.subarray(bytes.lastIndexOf('// @bun @bytecode\n', suggestionAt), bytes.indexOf(0, suggestionAt)).toString();
const forkAt = bytes.indexOf('async function MC(');
const fork = bytes.subarray(forkAt, bytes.indexOf('function fur(', forkAt)).toString();
const cacheAt = bytes.indexOf('function c4e(e,n){if(e.type==="text")');
const cache = bytes.subarray(cacheAt, bytes.indexOf('function $fr(', cacheAt)).toString();
const messages = [
  { type: 'assistant', requestId: 'req_http_first', message: { id:'msg_api_first', content:[{type:'thinking',thinking:'private'},{type:'text',text:'first candidate'},{type:'text',text:'not selected within one assistant row'}] } },
  { type: 'user', message: { content:'denied' } },
  { type: 'assistant', isApiErrorMessage:true, requestId:'req_error', message:{content:[{type:'text',text:'API Error'}]} },
  { type: 'assistant', requestId:'req_http_later', message:{content:[{type:'text',text:'second candidate'}]} },
];
let nativeCall;
const context = { xe: x => ({type:'user',message:x}), MC: async x => { nativeCall=x; return {messages}; } };
const begin=suggestion.indexOf('async function I('), end=suggestion.indexOf('function W(',begin);
vm.runInNewContext(suggestion.slice(begin,end)+';globalThis.I=I;',context);
const queryResult = JSON.parse(JSON.stringify(await context.I('native prompt',{signal:{aborted:false}},{})));
const denial = JSON.parse(JSON.stringify(await nativeCall.canUseTool()));
assert.deepEqual(queryResult,{candidates:['first candidate','second candidate'],generationRequestId:'req_http_first'});
assert.deepEqual(denial,{behavior:'deny',message:'No tools needed for suggestion',decisionReason:{type:'other',reason:'suggestion only'}});
assert.equal(nativeCall.skipTranscript,true);assert.equal(nativeCall.skipCacheWrite,true);
assert.equal(nativeCall.querySource,'prompt_suggestion');assert.equal(nativeCall.forkLabel,'prompt_suggestion');
let runOptions;
const toolUseContext={ options:{tools:[],mainLoopModel:'parent',runQuery:async function*(opts){runOptions=opts;yield messages[0];}}, getAppState:()=>({}),getAdvisorSetting:()=>undefined,readFileState:new Map() };
const forkContext={ Date, yf:{}, Rir:(ctx, overrides)=>({...ctx,...overrides,options:overrides.options??ctx.options}), sur:()=>undefined,iur:()=>false,xir:()=>undefined,Cpt:x=>x,li:()=>({}),u6t:x=>x,dC:x=>x,Vu:()=>'',k:()=>false,Ubt:()=>false,sdt:()=>false,t:()=>{},tAe:()=>null,fur:()=>{},Hnn:50 };
vm.runInNewContext(fork+';globalThis.MC=MC;',forkContext);
await forkContext.MC({promptMessages:[],cacheSafeParams:{systemPrompt:'parent',toolUseContext,forkContextMessages:[],stickyBetas:{}},canUseTool:nativeCall.canUseTool,querySource:'prompt_suggestion',forkLabel:'prompt_suggestion',skipTranscript:true,skipCacheWrite:true});
assert.equal(runOptions.maxTurns,50);assert.equal(runOptions.skipCacheWrite,true);
const cases=[
 {roles:['user'],blocks:[['text']]},
 {roles:['user','assistant','user'],blocks:[['text'],['text'],['text']]},
 {roles:['user','assistant','assistant','user'],blocks:[['text'],['text'],['thinking'],['text']]},
 {roles:['user','assistant','user'],blocks:[['text'],['tool_use'],['tool_result']]},
 {roles:['user','assistant','user'],blocks:[['text'],['text','thinking'],['text']]},
 {roles:['user','assistant','user'],blocks:[['text'],['empty_text'],['text']]},
 {roles:['user','assistant','user'],blocks:[['text'],['whitespace_text'],['text']]},
];
for(const pin of [false,true])for(const stepBack of [false,true])for(const parent of [0,1,2,3])cases.push({roles:['user','assistant','user','assistant','user'],blocks:[['text'],['text'],['text'],['tool_use'],['tool_result']],pin,stepBack,parent});
for(const stepBack of [false,true])cases.push({roles:['user','assistant','user'],blocks:[['text'],['text'],['text']],pin:true,stepBack,parent:1});
const cacheContext={uz:()=>false};vm.runInNewContext(cache+';globalThis.vsn=vsn;',cacheContext);
for(const item of cases){const wire=item.roles.map((type,index)=>({type,uuid:String(index),message:{content:item.blocks[index].map(block=>block==='empty_text'?{type:'text',text:''}:block==='whitespace_text'?{type:'text',text:' '}:{type:block,text:'x'})}}));
 const markers=cacheContext.vsn({messages:wire,enablePromptCaching:true,skipCacheWrite:true,canMarkApiSystem:true,forkPointUuid:item.parent===undefined?undefined:String(item.parent),forkCachePin:item.pin??false,whitespaceTextUnstampable:false,topLevelCacheControl:false,isForkPinStepBackEnabled:()=>item.stepBack??false});
 item.markerIndices=[...markers.markerIndices];
}
const pinGateCases=[];
const source=bytes.toString('utf8');
const gateAt=source.indexOf('function Z6(){let e=Ie();');
const gateSource=source.slice(gateAt,source.indexOf('function Rct(',gateAt));
const hbAt=source.indexOf('function HB(e=Ie())');
const hb=source.slice(hbAt,source.indexOf('function J2n(',hbAt));
for(const provider of ['firstParty','anthropicAws','anthropicGoogleCloud','foundry','bedrock','vertex','mantle','custom'])for(const disabled of [false,true])for(const hipaa of [false,true]){
 const gate={Ie:()=>provider,a:{CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:disabled},Za:()=>hipaa};
 vm.runInNewContext(hb+gateSource+';globalThis.enabled=Th();',gate);
 pinGateCases.push({provider,disabled,hipaa,expected:gate.enabled});
}
const fixture={pinGateCases,version:'2.1.296',binary_sha256:sha,queryResult,denial,maxTurns:runOptions.maxTurns,cacheCases:cases};
const json=JSON.stringify(fixture,null,2)+'\n';
for(const path of ['crates/sidequery/tests/fixtures/prompt_suggestion_query_2_1_296.json','../llm-client/tests/fixtures/prompt_suggestion_query_2_1_296.json']){
 if(process.argv.includes('--check'))assert.equal(fs.readFileSync(path,'utf8'),json);else{fs.mkdirSync(path.slice(0,path.lastIndexOf('/')),{recursive:true});fs.writeFileSync(path,json);}
}
console.log(`Native 2.1.296 query: denial, HTTP request id, candidate selection, 50-turn bound, ${cases.length} cache marker cases verified`);
