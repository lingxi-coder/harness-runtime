#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex'),sourceByteRanges=[];
assert.equal(sha(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
function take(first,last,offset,label){const start=binary.indexOf(first,offset),end=binary.indexOf(last,start+first.length);assert(start>=offset&&end>start&&end-start<2500,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString();}
const modes=take('function Cjt(e,n)', 'function BVn(',188838300,'actual connector display selection');
const budget=take('function Ajt(){', 'var RY=',188838800,'actual failed probe budget');
const errors=take('P4e=(qn)=>{', ',I4e=',189000650,'actual unclaimed display probe gate');
const success=take('I4e=()=>{', ',t6e=',189001080,'actual healed probe sticky commit');
const arm=take('if((bu?.type==="adaptive"||bu?.type==="enabled")&&gh', 'break}}if(ANe(',189010390,'actual updates request arm');
const redactIdentity=take('Czt=w("redact_thinking"', ',G2e=',180411300,'actual Czt beta identity');
const capability=take('var EF="capability_rejected: ";', 'function ONo(',186225290,'actual capability token boundary');
const overflow=take('function b4n(e)', 'var fie=',186225540,'actual context copy recognizers');
const signatures=take('function KFr(e)', 'function YFr(',186226000,'actual signature copy recognizer');
const contextErrors=take('function vA(e)', 'var UZn=',187958790,'actual context error gate');
const signatureErrors=take('function XT(e)', 'function Pne(',187976120,'actual signature error gate');
const fastErrors=take('function Yle(e)', 'function oFe(',188948210,'actual fast rejection gate');
const context=vm.createContext({});
vm.runInContext(`class xt extends Error {constructor(status){super();this.status=status;}}
var settings={};var host={displayUpdatesProbeFailures:0};var KM='idle',kw=[],Qe=[],qe={disabled:false};
function w(name,header){return {name,header};}
var cEe={header:'thinking-display-updates-2026-08-18'};var ${redactIdentity};
var a={},flags={summaries:false,updates:true,recognized:false,denied:false,trusted:false};
function Cvn(){return flags.summaries;} function $Fo(){return flags.updates;} function Oo(){return host;}
function IR(){return flags.recognized;} function pF(){return flags.denied;}
function He(){return flags.trusted?'firstParty':'gateway';} function ig(){return true;} function iM(){return false;}
function c(e){throw e;} function sm(state){state.disabled=true;} function t(){} function i(){} function $fe(x){return x;} function Et(x){return x;} function _A(){return 'fixture';}
var V={model:'fixture'};
${modes}${budget}var ${errors};var ${success};
${capability}${overflow}${signatures}${contextErrors}${signatureErrors}${fastErrors}
function mutate(body,betas,enabled,extra,simulated,disabled,state){
 let bu=JSON.parse(JSON.stringify(body.thinking)),ir=betas.map(header=>header==='thinking-display-updates-2026-08-18'?cEe:header===Czt.header?Czt:{header}),gh=enabled,Ji=extra?{thinking:null}:{},qe={disabled},KM=state;
 function jp(s){return s.disabled;} function CE(){const at=ir.indexOf(Czt);if(at!==-1)ir.splice(at,1);}
 a.CLAUDE_CODE_SIMULATE_PROXY_USAGE=simulated;
 ${arm}
 return {thinking:bu,betas:ir.map(x=>x.header)};
}
`,context);
const modeCases=[];
for(const display of [null,'','summarized','highlights','omitted','updates','unknown'])for(const explicit of [false,true])for(const summaries of [false,true])for(const updates of [false,true]){
 context.flags={summaries,updates};const result=context.Cjt(display===null?undefined:display,explicit);modeCases.push({display,explicit,summaries,updates,expected:result});
}
const probeCases=[];
for(const state of ['idle','retrying','spent'])for(const status of [200,400,422,429,500])for(const header_sent of [false,true])for(const billing_error of [false,true])for(const recognized_error of [false,true])for(const policy_denied of [false,true])for(const trusted_first_party_endpoint of [false,true])for(const failures of [0,1,2,3]){
 context.KM=state;context.kw=header_sent?[context.cEe.header]:[];context.Qe=[context.cEe];context.host={displayUpdatesProbeFailures:failures};context.flags={recognized:recognized_error,denied:policy_denied,trusted:trusted_first_party_endpoint};
 const error=vm.runInContext(`new xt(${status})`,context);error.type=billing_error?'billing_error':'invalid_request_error';const retry=context.P4e(error)??null;
 probeCases.push({state,status,header_sent,billing_error,recognized_error,policy_denied,trusted_first_party_endpoint,failures,expected:{retry,state:context.KM,failures:context.host.displayUpdatesProbeFailures}});
}
const successCases=[];
for(const state of ['idle','retrying','spent'])for(const disabled of [false,true]){context.KM=state;context.qe={disabled};context.I4e();successCases.push({state,disabled,expected:{state:context.KM,disabled:context.qe.disabled}});}
const wireCases=[];
for(const type of ['adaptive','enabled','disabled','unknown'])for(const enabled of [false,true])for(const extra of [false,true])for(const simulated of [false,true])for(const disabled of [false,true])for(const state of ['idle','retrying','spent']){
 const body={thinking:{type,...type==='enabled'&&{budget_tokens:1024}}},betas=['base-beta','redact-thinking-2026-02-12','summarize-connector-text-2026-03-13'];const expected=context.mutate(body,betas,enabled,extra,simulated,disabled,state);wireCases.push({body,betas,enabled,extra,simulated,disabled,state,expected:JSON.parse(JSON.stringify(expected)),thinkingBytes:JSON.stringify(expected.thinking),betaBytes:expected.betas.join(',')});
}
const knownErrorCases=[];
for(const status of [400,422])for(const message of [
 'fixture unknown request rejection','signature in `thinking` block','invalid data in `redacted_thinking` block',
 'thinking.signature: Field required','thinking block cannot be modified','redacted_thinking: invalid signature',
 'capability_rejected: thinking_signature','capability_rejected: thinking_signature_extra',
 'capability_rejected: thinking_signature.extra','capability_rejected: thinking_signature! rejected',
 'Fast mode is not enabled','fast mode is not enabled','input is too long for requested model',
 'prompt is too long: 100 tokens > 50','input length and `max_tokens` exceed context limit',
 'input length and max_tokens exceed context limit',
]){
 const error=vm.runInContext(`new xt(${status})`,context);error.message=message;
 const recognized=context.vA(error)||context.VO(error)||context.Yle(error),denied=context.XT(error);
 knownErrorCases.push({status,message,expected:recognized||denied,recognized,denied});
}
const output={version:'2.1.288',native_sha256:sha(binary),sourceByteRanges,scope:'Actual Cjt, P4e/I4e, Ajt/Pjt and automatic updates request arm; selected known context/signature/fast gates. Full IR/pF recognition, policy denial, route trust and interleaved capability supplied explicitly. Full flag/model acquisition, server classifier and SDK/CLI host lifecycle are independent requirements.',modeCases,probeCases,successCases,wireCases,knownErrorCases};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../deps/llm-client/tests/fixtures/thinking_display_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(JSON.stringify({modeCases:modeCases.length,probeCases:probeCases.length,successCases:successCases.length,wireCases:wireCases.length,knownErrorCases:knownErrorCases.length}));
