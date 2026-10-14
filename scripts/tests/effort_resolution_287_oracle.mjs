#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=bytes=>createHash('sha256').update(bytes).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(base,startMarker,endMarker,label){const start=binary.indexOf(startMarker,base),end=binary.indexOf(endMarker,start+startMarker.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const levels=take(178423956,'nu=[',',dL=','nu level order');
const aliases=take(181694118,'var z={med:', ';function cBe','iF alias table');
const parser=take(181694118,'function iF(e){','var X=','iF/$9');
const rank=take(181694118,'function $v(e){','function K(e)','level ranks and disabled-thinking ceiling');
const cap=take(181694118,'function S2t(e){','function Kxe(e)','provider/settings cap combination');
const clamp=take(181694118,'function b3(e,n){','function ce(e,n)','rank clamp');
const resolve=take(181694118,'function jb(e,n,','function a_n(e,n)','jb priority');
const normalize=take(181694118,'function T(e,n){','function B9(e)','T level normalization');
const defaults=take(181694118,'function l_n(e,n){','function VWo(e)','default precedence');
const catalog=take(181694118,'function we(e){','function YYn(e)','catalog default/high');
const numeric=take(181694118,'function aF(e){','function Me(e)','numeric conversion with settings cap');
const cases=[];
function run(input){
 const expected=vm.runInNewContext(`(()=>{
  let ${levels};${aliases};${parser}${rank}${cap}${clamp}${resolve}${normalize}${defaults}${catalog}${numeric}
  const G=Number.isInteger,a={CLAUDE_CODE_EFFORT_LEVEL:INPUT.env===null?undefined:INPUT.env},He=()=> 'firstParty',Be=x=>x,V0=x=>x;
  const K=()=>INPUT.state.settings_cap==='max'?null:INPUT.state.settings_cap??null;
  const Ape=()=>[{apiName:'model',maxEffortLevel:INPUT.state.provider_cap}],jw=()=>INPUT.caps.supported,pQ=()=>INPUT.caps.max,xXe=()=>INPUT.caps.xhigh;
  const F=()=>INPUT.state.carried,Kxe=()=>INPUT.state.managed_default,te=()=>INPUT.state.flag_default,Pe=()=>INPUT.state.model_override_default,xa=()=>({default_effort:INPUT.state.catalog_default}),ny=()=>INPUT.caps.thinking_disabled_cap;
  const options={...INPUT.state.turn!==undefined&&{turnEffort:INPUT.state.turn},...INPUT.state.hook!==undefined&&{hookEffortValue:INPUT.state.hook},carriedEffort:INPUT.state.carried};
  const value=jb('model',INPUT.primary,options),side=INPUT.thinking_disabled&&typeof value==='string'&&XYn(value)&&JYn('model')?PXe:value;
  return {parsed:iF(INPUT.env===null?undefined:INPUT.env)??null,env:$9()??null,env_cleared:$9()===null,has_value:value!==undefined,value:value??null,has_side_value:side!==undefined,side:side??null};
 })()`,{INPUT:input});cases.push({input,expected});
}
const caps={supported:true,max:true,xhigh:true,thinking_disabled_cap:true};
for(const env of [null,'','low','MED','high','MAX','xhigh','auto','UNSET',' auto ',' high ','0','-0','3.5','+17x','0xff','1e999','wat','\uFEFF12x','\u008512','9'.repeat(400)])
 for(const settings_cap of [null,'low','high','max'])for(const provider_cap of [null,'medium','max'])
 for(const primary of [undefined,null,'low','max','xhigh',7,false]) {
  run({env,caps,state:{settings_cap,provider_cap},...primary!==undefined&&{primary},thinking_disabled:true});
 }
for(const supported of [false,true])for(const max of [false,true])for(const xhigh of [false,true])
 for(const hook of [undefined,null,'xhigh','max',7,false,'unknown'])for(const settings_cap of [null,'medium']){
  run({env:'low',primary:'high',caps:{...caps,supported,max,xhigh},state:{settings_cap,...hook!==undefined&&{hook}},thinking_disabled:true});
 }
for(const env of [null,'auto','medium'])for(const turn of [undefined,null,'xhigh',17])for(const primary of [undefined,null,'low'])
 for(const state of [{},{catalog_default:'medium'},{model_override_default:'xhigh',catalog_default:'low'},{carried:'low',model_override_default:'xhigh'},{flag_default:'max',carried:'low'},{managed_default:'high',flag_default:'low',carried:'xhigh'}]){
  run({env,caps,state:{...state,...turn!==undefined&&{turn}},...primary!==undefined&&{primary},thinking_disabled:false});
 }
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual iF/$9/jb/T/l_n/we/S2t/b3/aF and disabled-thinking ranks. Selected-model capabilities and validated managed/flag/carried/override/catalog defaults/settings cap are supplied host state. No full settings folding, catalog acquisition, carry lifetime, CLI command, or live-provider acceptance.',cases};
const json=JSON.stringify(output,null,2)+'\n',destination=new URL('../../crates/core/tests/fixtures/effort_resolution_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native effort resolution cases`);
