#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=bytes=>createHash('sha256').update(bytes).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const start=binary.indexOf('function jw(e){',181694118),end=binary.indexOf('function Sx(',start);
assert(start>=181694118&&end>start);
const bytes=binary.subarray(start,end),source=bytes.toString('utf8'),cases=[];
const support=[undefined,false,true],lists=[undefined,[],['low'],['max'],['xhigh'],['low','medium','high','xhigh','max']];
for(const supported of [false,true])for(const max of [false,true])for(const xhigh of [false,true])
 for(const effort of support)for(const max_effort of support)for(const xhigh_effort of support)for(const levels of lists){
  const input={native:{supported,max,xhigh},facts:{effort,max_effort,xhigh_effort,levels}};
  const expected=vm.runInNewContext(`(()=>{
    const bkn=()=>false,Dbe=()=>undefined,Be=x=>x,cQn=()=>INPUT.facts.levels;
    const zQe=key=>INPUT.facts[key],a={},ny=(_,key)=>INPUT.native[{effort:'supported',max_effort:'max',xhigh_effort:'xhigh'}[key]],cc=()=>undefined,zF=()=>false;
    ${source}
    return {supported:jw('model'),max:pQ('model'),xhigh:xXe('model')};
  })()`,{INPUT:input});
  cases.push({input,expected});
 }
const catalogStart=binary.indexOf('Vtr={',179125203),catalogEnd=binary.indexOf(';function O()',catalogStart);
assert(catalogStart>=179125203&&catalogEnd>catalogStart);
const catalogBytes=binary.subarray(catalogStart+4,catalogEnd);
const baked=vm.runInNewContext('('+catalogBytes.toString('utf8')+')');
const bakedDefaults=baked.models.map(model=>({model:model.id,default:model.default_effort??'high',capabilities:model.capabilities}));
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:[{label:'jw/J/pQ/xXe',start,end,sha256:sha(bytes)},{label:'Vtr baked catalog',start:catalogStart+4,end:catalogEnd,sha256:sha(catalogBytes)}],bakedDefaults,scope:'Actual native explicit effort/per-level facts and runtime list precedence. Builtin fallback supplied; excludes bkn/Dbe, served acquisition, account caps and full CLI. Actual baked defaults included; served overrides excluded.',cases};
const json=JSON.stringify(output,null,2)+'\n',destination=new URL('../../crates/llm-runtime/tests/fixtures/effort_catalog_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native effort catalog cases`);
