#!/usr/bin/env node
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
const binary=fs.readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex'),ranges=[];
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
function take(base,first,last,label){const start=binary.indexOf(first,base),end=binary.indexOf(last,start+first.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const source=take(179125203,'Vtr={',';function O()','Vtr current baked catalog').slice(4);
const models=vm.runInNewContext('('+source+')').models.map(({id,capabilities})=>({id,capabilities}));
const lookup=take(179125203,'function S(','function Cje(','S/ny/bzt/L8r/zQe/N8r/m current lookup');
const gates=take(179125203,'var N={','function O9o(','capability feature admission');
const fast=take(180615120,'function Ry(','function Lk(','Ry current fast model gate');
const lean=take(181761562,'function Yr(','function AG(','Yr current standard prompt gate');
const cases=[];
for(const model of models.map(x=>x.id))for(const support of [null,false,true])for(const global of [false,true]){
 const context=vm.createContext({N:{},a:{CLAUDE_CODE_MODEL_CAPABILITIES:support===null?undefined:(support?'fast_mode':'-fast_mode')},cL:()=>({}),xa:id=>models.find(x=>x.id===id),To:()=>global,Rt:x=>x,Be:x=>x,Py:()=>model});
 vm.runInContext(gates+lookup+fast,context);
 cases.push({model,support,global,expected:vm.runInContext(`Ry(${JSON.stringify(model)})`,context)});
}
const leanCases=[];
for(const model of models.map(x=>x.id))for(const support of [null,false,true])for(const firstParty of [false,true]){
 const context=vm.createContext({a:{CLAUDE_CODE_MODEL_CAPABILITIES:support===null?undefined:(support?'lean_prompt':'-lean_prompt')},cL:()=>({}),xa:id=>models.find(x=>x.id===id),Be:x=>x,jSe:()=>false,od:()=>firstParty});
 vm.runInContext(gates+lookup+lean,context);
 leanCases.push({model,support,firstParty,standard:vm.runInContext(`Yr(${JSON.stringify(model)})`,context)});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:ranges,models,fastCases:cases,leanCases,scope:'Exact current baked capability arrays and actual Ry/ny fast and Yr standard-prompt gates. Identity, global To/od and absent early-access gate supplied; served/account acquisition, full model initialization, prompt section contents and dynamic overrides remain separate.'};
const json=JSON.stringify(output,null,2)+'\n',file=new URL('../../crates/core/tests/fixtures/model_capabilities_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(fs.readFileSync(file,'utf8'),json);else fs.writeFileSync(file,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${models.length} current capability arrays and ${cases.length} fast and ${leanCases.length} lean cases`);
