#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=bytes=>createHash('sha256').update(bytes).digest('hex'),ranges=[];
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
function take(base,startMarker,endMarker,label){let start=binary.indexOf(startMarker,base),end=binary.indexOf(endMarker,start+startMarker.length);assert(start>=base&&end>start,label);let bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const command=take(198608235,'var F={','\nexport{','current effort local command helpers');
const ultraGate=take(181694118,'function Sx(e){','function o_n(e,n,r){','Sx/jC independent workflow and model gates');
const wrapper=take(198614853,'async function u(','export{','noninteractive command wrapper');
const warningStart=binary.indexOf('dL=',178423956);
const warningLiteral=binary.subarray(warningStart+3,warningStart+1500).toString('utf8').match(/^("(?:\\.|[^"\\])*")/)[1];
ranges.push({label:'maximum effort caution',start:warningStart+3,end:warningStart+3+Buffer.byteLength(warningLiteral),sha256:sha(Buffer.from(warningLiteral))});
const warning=vm.runInNewContext(warningLiteral);
const coreParts=[
 take(178423956,'nu=[',',dL=','level order'),
 take(181694118,'var z={med:',';function cBe','parser aliases'),
 take(181694118,'function iF(e){','var X=','iF/$9'),
 take(181694118,'function $v(e){','function K(e)','ranks'),
 take(181694118,'function S2t(e){','function Kxe(e)','settings/provider cap'),
 take(181694118,'function ZYn(e,n){','function $ue(e){','cap option admission'),
 take(181694118,'function $ue(e){','function ce(e,n)','options and clamp'),
 take(181694118,'function F9(e){','function Uue(e){','scalar display'),
 take(181694118,'function Uue(e){','function IXe(e)','command level parser'),
 take(181694118,'function dBe(e){','function eXn(e)','persistable'),
 take(181694118,'function be(e,n){','function kd(e,n)','write patch'),
 take(181694118,'function jb(e,n,','function a_n(e,n)','resolution'),
 take(181694118,'function T(e,n){','function B9(e)','normalization'),
 take(181694118,'async function _e(e,n,r){','function aht(e)','write admission'),
 take(181694118,'function KA(e,n,r){','function WC(e,n,r)','displayed level'),
 take(181694118,'function aF(e){','function l_n(e,n)','display scalar and descriptions'),
 take(181694118,'function l_n(e,n){','function VWo(e)','default priority'),
 take(181694118,'function we(e){','function YYn(e)','baked/default slot'),
];
const encoded=value=>value===undefined?{$undefined:true}:Array.isArray(value)?value.map(encoded):value&&typeof value==='object'?Object.fromEntries(Object.entries(value).map(([k,v])=>[k,encoded(v)])):value;
const cases=[];
for(const args of ['','current','status','CURRENT','help','-h','--help','med','LOW','xhigh','max','auto','unset','ultracode','ultracode on','ultracode off','ultracode no','ultracode on extra','wat','\uFEFFmedium\uFEFF','\u0085low\u0085'])
 for(const env of [null,'low','auto','0'])for(const cap of [null,'medium','max'])for(const save of [false,true]) {
  const input={args,env,save,model:'claude-opus-4-7',settings_key:'claude-opus-4-7',primary:'low',workflow:true,ultracode:false,caps:{supported:true,max:true,xhigh:true},state:{settings_cap:cap,catalog_default:'xhigh'}};
  await run(input);
 }
for(const key of ['claude-opus-4-7','constructor','__proto__','toString'])for(const args of ['low','max','auto'])
 for(const failure of [false,true])await run({args,env:null,save:true,model:key,settings_key:key,workflow:false,ultracode:false,caps:{supported:true,max:false,xhigh:false},state:{catalog_default:'medium'},failure});
for(const primary of [undefined,null,0,7,'medium','max'])for(const env of [null,'low','auto'])for(const workflow of [false,true])for(const xhigh of [false,true])
 for(const args of ['status','ultracode','ultracode off'])await run({args,env,save:true,model:'model',settings_key:'model',workflow,ultracode:true,caps:{supported:true,max:false,xhigh},state:{catalog_default:'medium',managed_default:'high'},...primary!==undefined&&{primary}});
async function run(input){
 const writes=[];
 const nativeCore=vm.runInNewContext(`(()=>{
  let ${coreParts[0]};${coreParts.slice(1).join(';')}
  const a={CLAUDE_CODE_EFFORT_LEVEL:INPUT.env??undefined},G=Number.isInteger,K=()=>INPUT.state.settings_cap==='max'?null:INPUT.state.settings_cap??null;
  const He=()=> 'firstParty',Be=x=>x,V0=x=>x,Ape=()=>[],jw=()=>INPUT.caps.supported,pQ=()=>INPUT.caps.max,xXe=()=>INPUT.caps.xhigh;
  const Kxe=()=>INPUT.state.managed_default??null,te=()=>INPUT.state.flag_default,F=()=>INPUT.state.carried,Pe=()=>INPUT.state.model_override_default,xa=()=>({default_effort:INPUT.state.catalog_default}),ny=()=>false;
  const qSe=()=>INPUT.settings_key,pr=()=>null,hn=async(source,patch)=>{WRITES.push(ENCODE(patch));return INPUT.failure?{error:{message:'fixture failure'}}:{}};
  const $be=()=>false,k=()=>false,dL=WARNING;
  return {jb,KA,Uue,dBe,b3,$ue,F9,B6r,w3,$9,Kxe};
 })()`,{INPUT:input,WRITES:writes,ENCODE:encoded,WARNING:warning});
 const expected=await vm.runInNewContext(`(async()=>{
  const {$ue,b3,Uue,dBe,F9,B6r,w3,$9,KA,Kxe,jb}=CORE;
  const a={CLAUDE_CODE_EFFORT_LEVEL:INPUT.env??undefined},rt=()=>INPUT.model,nm=()=>INPUT.workflow,ae=()=>INPUT.caps.xhigh,pr=()=>null,Ia=()=>false,i=()=>{},S=x=>x,c=x=>x,d=()=>{},i_n=()=>null;
  const OXe=value=>value===undefined?{kind:'default'}:{kind:'level',value},fQ=(a,b)=>JSON.stringify(a)===JSON.stringify(b),r_n=()=>{},Rt=x=>x,Py=()=>INPUT.model,dR=['help','-h','--help'];
  ${ultraGate}${command}${wrapper}
  let state={sessionEffort:INPUT.primary===undefined?{kind:'inherit'}:{kind:'level',value:INPUT.primary},ultracode:INPUT.ultracode};
  const context={getAppState:()=>state,setAppState:fn=>state=fn(state),options:{isNonInteractiveSession:!INPUT.save},storageV5:undefined};
  const bh=()=>state.sessionEffort.kind==='level'?state.sessionEffort.value:undefined;
  const response=await u(INPUT.args,context);
  return {message:response.value,state,writes:WRITES};
 })()`,{INPUT:input,CORE:nativeCore,WRITES:writes});
 cases.push({input,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:ranges,scope:'Actual local command helpers, local/noninteractive wrapper, core effort resolution and be/_e/w3 patch admission. Identity, capabilities, admitted defaults/caps and session primary are supplied. Remote transport, native settings IO merge/atomicity, telemetry, account acquisition, picker UI, warning experiments and complete Ultracode lifecycle excluded.',cases};
const json=JSON.stringify(output,null,2)+'\n',destination=new URL('../../crates/command-api/tests/fixtures/effort_command_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native local effort command cases`);
