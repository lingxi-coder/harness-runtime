#!/usr/bin/env node
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
const binary=fs.readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const start=binary.indexOf('if(wn(C.fallbackModel)',193794113),end=binary.indexOf('if(K)i("tengu_convolute',start);
assert(start>=193794113&&end>start);
const bytes=binary.subarray(start,end),source=bytes.toString('utf8'),cases=[];
for(const main of [false,true])for(const silent of [false,true]){
 const trace=[];
 const result=vm.runInNewContext(`(()=>{
  let serving='initial',state={mainLoopModel:'initial',mainLoopModelForSession:null,fastMode:false};
  let f={options:{mainLoopModel:'initial',availabilityFallback:{primaryModel:'initial'}},getAppState:()=>state,setAppState:fn=>{state=fn(state)}};
  const C={fallbackModel:'fallback'},Xe=MAIN,K=SILENT,nt=['prior-message'],wn=x=>{serving=x},Tn=undefined;
  let pn=false,ee,se=true,yo;
  const Hwt=x=>TRACE.push(x),Kc=()=>undefined,To=()=>false,Lk=()=>false,Jp=()=>{},Jo=()=>undefined,Nk=()=>{};
  ${source}
  return {requestModel:serving,toolModel:f.options.mainLoopModel,sessionModel:state.mainLoopModel,availabilityFallback:f.options.availabilityFallback??null};
 })()`,{MAIN:main,SILENT:silent,TRACE:trace});
 cases.push({main,silent,expected:result});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:[{label:'native refusal serving/options/session swap',start,end,sha256:sha(bytes)}],scope:'Actual refusal model swap branch. Main-thread and silent-attempt inputs supplied. Child local refusal changes serving model without changing tool options; availability/consent/live model switches and full query loop excluded.',cases};
const json=JSON.stringify(output,null,2)+'\n',file=new URL('../../crates/agent/tests/fixtures/child_refusal_scope_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(fs.readFileSync(file,'utf8'),json);else fs.writeFileSync(file,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native refusal scope cases`);
