#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]);
const sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges=[];
function take(first,last,offset,label){const start=binary.indexOf(first,offset),end=binary.indexOf(last,start+first.length);assert(start>=offset&&end>start&&end-start<2500,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString();}
const predicate=take('function rFe(e)', 'async function Xjo(',188948400,'actual typed 5xx predicate excluding 529');
const immediate=take('let On=ove(rn)||dtt(rn)', 'let fo=qle(rn)',188940400,'actual pre-retry immediate fallback gate');
const overload=take('if(uB(rn)&&(a.FALLBACK_FOR_ALL_PRIMARY_MODELS', 'let oo=_9()&&HGt(rn)',188942100,'actual overload increment and threshold fallback gate');
const retryClassifier=take('function eWo(e)', 'function $Gt(e)',188950250,'actual overload and persistent capacity acceptance before retry decline');
const overloadPredicate=take('function uB(e){if(!(e instanceof xt))', 'function X(e)',181196900,'actual raw-status or overload-payload classifier');
const ctx=vm.createContext({Headers});
vm.runInContext(`class xt extends Error {constructor(status){super();this.status=status;}}
class UP extends Error {constructor(from,to,reason){super();this.reason=reason;}}
let watchdog=false;
function _9(){return watchdog;}
function ove(){return false;} function dtt(){return false;}
function pc(){return 'firstParty';} function tFe(){return undefined;}
function i(){} function p(){} function Et(m){return m;} function _A(){return 'firstParty';} function d(v){return v;}
${overloadPredicate} function ft(){return false;}
function LZ(){return false;} function f0e(){return false;} function kSt(){return false;}
class tu extends Error {}
function b8e(){return false;} function rWo(){return false;} function HGt(e){return uB(e)||e.status===429;}
function XD(){return false;} function qle(){return false;} function Vle(){return false;} function Al(){return false;}
function BZ(){return true;} function jke(){return false;} function nFe(){return undefined;} function See(){return false;}
function _m(){return undefined;} function PQe(){} function Lfe(){return false;} function $Gt(){return false;}
let a={FALLBACK_FOR_ALL_PRIMARY_MODELS:false,IS_SANDBOX:true},WY=3,nn='primary';
${predicate}${retryClassifier}
function declinedIsTerminal(status,declined,overloadPayload,persistent){
 watchdog=persistent;const error=new xt(status);error.headers=new Headers({'x-should-retry':declined?'false':'true'});
 error.message=overloadPayload?'{"type":"overloaded_error"}':'{"type":"api_error"}';
 return declined&&!eWo(error);
}
function evaluate(status,available,persistent,overloaded,overloadPayload){
 watchdog=persistent;let r={model:'primary',fallbackModel:available?'fallback':undefined},rn=new xt(status),w=overloaded;rn.message=overloadPayload?'{"type":"overloaded_error"}':'{"type":"api_error"}';
 try {${immediate}${overload} return null;} catch(e){if(e instanceof UP)return e.reason;throw e;}
}
`,ctx);
const cases=[];
for(let status=0;status<650;status++)for(const available of [false,true])for(const persistent of [false,true])for(const overload_payload of [false,true])cases.push({status,available,persistent,overload_payload,overloaded:0,expected:ctx.evaluate(status,available,persistent,0,overload_payload)});
for(const status of [400,429,500,529,599])for(let overloaded=0;overloaded<10;overloaded++)for(const available of [false,true])for(const persistent of [false,true])for(const overload_payload of [false,true])cases.push({status,available,persistent,overload_payload,overloaded,expected:ctx.evaluate(status,available,persistent,overloaded,overload_payload)});
assert.equal(cases.length,5600);
const declineCases=[];
for(const status of [429,529,503,500])for(const declined of [false,true])for(const overload_payload of [false,true])for(const persistent of [false,true])declineCases.push({status,declined,overload_payload,persistent,expected:ctx.declinedIsTerminal(status,declined,overload_payload,persistent)});
const data={version:'2.1.288',native_sha256:sha(binary),sourceByteRanges,scope:'Actual native immediate HTTP server and cumulative overload fallback gates with distinct configured models. Model-not-found/permission classifiers stubbed false; no implicit first-party fallback, background admission, Fast repair or generic retry policy asserted.',cases,declineCases};
const output=JSON.stringify(data,null,2)+'\n';
const path=new URL('../../crates/llm-runtime/tests/fixtures/http_fallback_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),output);else writeFileSync(path,output);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native HTTP fallback decisions`);
