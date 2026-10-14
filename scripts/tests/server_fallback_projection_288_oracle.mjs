#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]);
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
assert.equal(hash(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const ranges=[];
function take(first,last,offset,label,max=5000){
 const start=binary.indexOf(first,offset),end=binary.indexOf(last,start+first.length);
 assert(start>=offset&&end>start&&end-start<max,label);
 const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:hash(bytes)});return bytes.toString();
}
const visible=take('function Tle(e)','function _No(',188771000,'native visible-reason predicate');
const discard=take('function FHt(e){','function $Ht(',188771000,'native completed-message discard predicate');
const controls=take('function lNe(e)','var wle=',188772000,'native valid and malformed fallback blocks');
const counter=take('var wle=','function cNe(',188773000,'native usage counter');
const iterations=take('function $cn(e)','function Ucn(',188773000,'native usage iterations');
const mid=take('if(V.serverRefusalFallback===void 0)continue;','continue}if(Bu)',189057000,'native admitted stream hop and sorted text retention');
const boundary=take('if(V.serverRefusalFallback!==void 0){let Nl=oI;','let Jd=V.refusalFallbackModel',189066000,'native terminal pending-hop and usage-only event');
const interrupted=take('jg=()=>{',';function*AE()',189055000,'native interrupted pending-hop event');
const sync=take('Sbn=(qn)=>{let Go=',';function*bbn(',189036000,'native nonstream content projection');
const placeholder=take('var iu="(no content)"',',sL=',181543000,'native empty-content placeholder',100);
const context=vm.createContext({});
vm.runInContext(visible+discard+controls+counter+iterations+placeholder+';'+`
function produce(input){
 let V={model:'primary',serverRefusalFallback:input.armed?{model:'declared'}:undefined},
 Tw=new Map(input.completed.map(([index,block])=>[index,{apiBlockIndex:index,message:{model:'primary',content:[block]}}])),
 Pp=[...Tw.values()],tg={},pf={model:'primary'},oI,ek=false,vS=false,m1=false,ra=input.requestId,Fu=input.stopReason,ha=$cn(input.usage),MR=()=>null;
 const events=[];
 for(const nd of input.hops??(input.hop?[input.hop]:[]))events.push(...(function*(){for(let once of [0]){${mid}continue}})());
 if(input.interrupted){const ${interrupted};const event=jg();if(event)events.push(event);}
 else events.push(...(function*(){${boundary}})());
 return {events,remaining:[...Tw.keys()],model:pf.model};
}
function nonstream(input){
 const V={model:'primary',serverRefusalFallback:input.armed?{}:undefined},O3=undefined,Xe='primary',ra=null,Is=null,NR=false,Ug=0,n=()=>{},te=n,i=n,p=n,d=n,AY=n,lQ=n,O3e=n;
 let vS,ek,iF,T9;const ${sync};return Sbn(input.body);
}
`,context);
const normalize=event=>({fromModel:event.fromModel,toModel:event.toModel,reason:event.reason,apiRefusalCategory:event.apiRefusalCategory??null,midStream:event.midStream,requestId:event.requestId??null,discardedBlocks:event.discardedMessages.map(m=>m.apiBlockIndex),retainedBlocks:event.retainedMessages.map(m=>m.apiBlockIndex),retainedText:event.retainedText,finalStopReason:event.finalStopReason??null});
const text=s=>({type:'text',text:s}),think={type:'thinking',thinking:'thought',signature:'sig'},tool={type:'tool_use',id:'tool',name:'Read',input:{}},native={type:'server_tool_use',id:'server',name:'web_search',input:{}};
const completedSets=[[],[[0,text('a')]],[[2,text('c')],[0,text('a')]],[[2,think],[0,text('a')],[1,tool]],[[3,native],[1,think],[2,text('b')],[0,text('a')]],[[0,tool]],[[0,{type:'redacted_thinking',data:'opaque'}],[1,text('')]]];
const streamCases=[];
for(const armed of [false,true])for(const reason of ['refusal','sticky','other'])for(const completed of completedSets)for(const interrupted of [false,true])for(const stopReason of [null,'end_turn','refusal']){
 const input={armed,completed,interrupted,stopReason,requestId:'req_server',hop:{fromModel:'primary',model:'served',reason,category:'cyber'},usage:{iterations:[{type:'fallback_message',model:'usage-target'}]}};
 const result=context.produce(input);streamCases.push({input,events:result.events.map(normalize),remaining:result.remaining,model:result.model});
}
const sequenceCases=[];
for(const armed of [false,true])for(const completed of completedSets)for(const interrupted of [false,true])for(const reason of ['refusal','other']){
 const input={armed,completed,interrupted,stopReason:'end_turn',requestId:'req_sequence',hops:[{fromModel:'primary',model:'served',reason:'refusal',category:'cyber'},{fromModel:'served',model:'second',reason,category:null}],usage:{}};
 const result=context.produce(input);sequenceCases.push({input,events:result.events.map(normalize),remaining:result.remaining,model:result.model});
}
for(const served of [undefined,'','sticky-target'])for(const armed of [false,true])for(const stopReason of [null,'end_turn','refusal']){
 const input={armed,completed:[],interrupted:false,stopReason,requestId:null,hop:null,usage:served===undefined?{}:{iterations:[{type:'fallback_message',model:served}]}};
 const result=context.produce(input);streamCases.push({input,events:result.events.map(normalize),remaining:result.remaining,model:result.model});
}
const valid={type:'fallback',from:{model:'primary'},to:{model:'served'},trigger:{type:'refusal',category:'bio'}},malformed={type:'fallback',from:{model:'primary'},to:{model:''}};
const contents=[[],[valid],[malformed],[think,valid,text('answer')],[text('a'),tool,valid,text('b')],[text('a'),valid,think,malformed,tool,text('b')],[valid,malformed],[tool,malformed],[think,valid,tool,valid,text('last')]];
const nonstreamCases=[];
for(const armed of [false,true])for(const content of contents)for(const stop_reason of [undefined,null,'end_turn','refusal'])for(const usage of [{},{iterations:[{type:'fallback_message',model:''}]},{iterations:[{type:'fallback_message',model:'usage-target'}]}]){
 const input={armed,body:{model:'primary',content,stop_reason,usage}};
 const result=context.nonstream(input),hop=result.lastHop,normalized=$=>JSON.parse(JSON.stringify($));
 const event=armed&&(hop||result.iterations?.servedFallbackModel!==undefined)?normalize({fromModel:hop?.fromModel??'primary',toModel:hop?.model??result.iterations.servedFallbackModel,reason:hop?'refusal':'sticky',apiRefusalCategory:hop?.category??null,midStream:false,requestId:null,discardedMessages:[],retainedMessages:[],retainedText:'',finalStopReason:stop_reason}):null;
 nonstreamCases.push({input:normalized(input),content:normalized(result.content.filter(block=>block.type!=='fallback')),event});
}
const data={version:'2.1.288',native_sha256:hash(binary),sourceByteRanges:ranges,scope:'Actual native stream hop, terminal/interruption boundary and Sbn body execute. Request lane, completed single-block messages, effort (null) and usage facts are supplied. Host allowlist, query/session latch, UUID envelopes, display and credit/cost ownership remain outside this fixture.',streamCases,sequenceCases,nonstreamCases};
const path=new URL('../../deps/llm-client/tests/fixtures/server_fallback_projection_2_1_288.json',import.meta.url),output=JSON.stringify(data,null,2)+'\n';
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),output);else writeFileSync(path,output);
console.log(JSON.stringify({streamCases:streamCases.length,sequenceCases:sequenceCases.length,nonstreamCases:nonstreamCases.length,ranges:ranges.length}));
