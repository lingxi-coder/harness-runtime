#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),hash=b=>createHash('sha256').update(b).digest('hex'),ranges=[];
assert.equal(hash(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
function take(first,last,offset,label){const start=binary.indexOf(first,offset),end=binary.indexOf(last,start+first.length);assert(start>=offset&&end>start&&end-start<4000,label);const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:hash(bytes)});return bytes.toString();}
const state=take('function AO(e,t)','function Ats(',178976490,'native beta sent/rejected transitions');
const betas=take('uL=w("server_side_fallback"',',XU=',180412200,'actual June and July server beta identities');
const wire=take('function Mn(e){return e.replace','function Gy(',179326700,'native fallback target context-tag removal');
const request=take('function NHt(e','function FHt(',188771300,'native server fallback request admission and beta order');
const final=take('let pm=Tu&&','if(a1=pm.fallbacks',189012900,'native post-beta-transport parameter gate');
const truth=take('function Le(e){','function zT(e)',178927000,'native typed boolean parsing');
const admission=take('function HVn(e)','function oHo(',188775250,'native visible and server query admission');
const dialog=take('function zFo(e)','function G4n(',185912800,'native dialog suppression predicate');
const context=vm.createContext({});vm.runInContext(`function w(name,header){return{name,header};}var ${betas};${state}${wire}${request}${dialog}${admission}${truth}
function Ka(){return facts.first_party;}function IFo(){return facts.eligible;}
function JN(){return policy.enabled;}function Yo(){return{value:policy.switch_models_on_flag};}function RFo(){return policy.enabled&&policy.switch_models_on_flag&&policy.server_allowed;}function _$r(){return policy.candidate_model??undefined;}function N8e(){return policy.candidate_same_model;}function jHt(){return policy.default_allowed&&!policy.default_beta_rejected;}
function finish(Tu,Ip,oc){${final}return pm;}`,context);
const variants=[{eligible:true,first_party:true,transport:true},{eligible:false,first_party:true,transport:true},{eligible:true,first_party:false,transport:true},{eligible:true,first_party:true,transport:true,threaded:true},{eligible:true,first_party:true,transport:true,silent:true},{eligible:true,first_party:true,transport:false},{eligible:true,first_party:true,transport:true,simulated:true},{eligible:true,first_party:true,transport:true,mismatch:true},{eligible:true,first_party:true,transport:true,silent:true,initial:[0]},{eligible:true,first_party:true,transport:true,silent:true,initial:[1]},{eligible:true,first_party:true,transport:true,initial:[1,0]},{eligible:true,first_party:true,transport:false,initial:[0,1]},{eligible:true,first_party:true,transport:true,simulated:true,initial:[0,1]}];
const requests=[];
for(let bits=0;bits<16;bits++)for(const mode of [null,'explicit','default'])for(const facts of variants){
 context.facts=facts;const descriptor=[context.uL,context.I_];
 const initial={explicit_active:!!(bits&1),default_active:!!(bits&2),explicit_rejected:!!(bits&4),default_rejected:!!(bits&8)};
 const sticky={sent:new Set(descriptor.filter((_,i)=>bits&(1<<i))),rejected:new Set(descriptor.filter((_,i)=>bits&(1<<(i+2))))};
 const betas=(facts.initial??[]).map(i=>descriptor[i]);const lane=mode?{forModel:facts.mismatch?'other':'primary',model:'Tar[1M]get[2m]',mode}:undefined;
 const parameters=context.NHt(lane,'primary',betas,sticky,!!facts.silent,!!facts.threaded);
 const actualBetas=facts.transport&&!facts.simulated?betas:[];
 const final=context.finish(facts.transport,actualBetas,parameters);
 requests.push({policy:{lane,explicit_target_eligible:facts.eligible,silent_arm:!!facts.silent,threaded_request:!!facts.threaded,beta_transport_enabled:facts.transport,simulated_proxy_usage:!!facts.simulated},firstParty:facts.first_party,model:'primary',initial,initialBetas:(facts.initial??[]).map(i=>descriptor[i].header),expected:{parameters:final,betas:actualBetas.map(b=>b.header),state:{explicit_active:sticky.sent.has(context.uL),default_active:sticky.sent.has(context.I_),explicit_rejected:sticky.rejected.has(context.uL),default_rejected:sticky.rejected.has(context.I_)}}});
}
let seed=812381;function bit(){seed=(Math.imul(seed,1664525)+1013904223)>>>0;return (seed>>>28)&1;}
const cases=[];
for(let index=0;index<1024;index++){
 const policy={candidate_model:bit()?'target':null,enabled:!!bit(),switch_models_on_flag:!!bit(),server_allowed:!!bit(),candidate_same_model:!!bit(),default_allowed:!!bit(),explicit_beta_rejected:!!bit(),default_beta_rejected:!!bit()};context.policy=policy;
 const inputs={currentModel:'primary',alreadyUsed:!!bit(),declined:!!bit(),inCascadeEpisode:!!bit(),cascadeNext:bit()?'next':undefined,suppressionAlreadyLogged:!!bit(),requestDialog:bit()?{}:undefined,isMainThread:!!bit(),consumerLacksDialogCapability:!!bit(),preferenceQuestionPending:!!bit(),serverDefaultFallbacksEnabled:()=>policy.default_allowed,selfTargetDefaultFallbacksEnabled:()=>false,sticky:{rejected:new Set(policy.explicit_beta_rejected?[context.uL]:[])},threadedRequest:!!bit()};
 policy.threaded_request=inputs.threadedRequest;policy.request_dialog=inputs.requestDialog!==undefined;policy.consumer_lacks_dialog_capability=inputs.consumerLacksDialogCapability;policy.preference_question_pending=inputs.preferenceQuestionPending;
 cases.push({policy,query:{model:inputs.currentModel,alreadyUsed:inputs.alreadyUsed,declined:inputs.declined,inCascade:inputs.inCascadeEpisode,next:inputs.cascadeNext,main:inputs.isMainThread,logged:inputs.suppressionAlreadyLogged},expected:JSON.parse(JSON.stringify(context.HVn(inputs)))});
}
const envCases=[null,'','0','false','1','true','on','off','yes','TRUE',' true ','\uFEFF1\uFEFF','\u00851','tRuE','\u30001','random'].map(input=>({input,expected:context.Le(input??undefined)}));
const shared={version:'2.1.288',native_sha256:hash(binary),sourceByteRanges:ranges,scope:'Actual HVn/zFo, AO/qy/sm/jp, NHt/Mn and final transport-beta gate. Model/category/feature/preference acquisition is supplied as host facts; full resolver, query counters, wrappers and swap acceptance remain separate.'};
for(const [path,data]of [[new URL('../../deps/llm-client/tests/fixtures/fallback_request_2_1_288.json',import.meta.url),{...shared,requests}],[new URL('../../crates/core/tests/fixtures/fallback_admission_2_1_288.json',import.meta.url),{...shared,cases,envCases}]]){const text=JSON.stringify(data,null,2)+'\n';if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),text);else writeFileSync(path,text);}
console.log(JSON.stringify({requests:requests.length,admission:cases.length,ranges:ranges.length}));
