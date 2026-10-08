#!/usr/bin/env node
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
const binary=fs.readFileSync(process.argv[2]), sha=b=>createHash('sha256').update(b).digest('hex'), ranges=[];
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
function take(base,first,last,label){const start=binary.indexOf(first,base),end=binary.indexOf(last,start+first.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const gate=take(181693060,'function AM(','function KSe(','AM wakeup gate');
const lookup=take(179125203,'function S(','function Cje(','S/ny/bzt/L8r/zQe/N8r/m capability lookup');
const admission=take(179125203,'var N={','function O9o(','served feature admission');
const catalog=take(179125203,'Vtr={',';function O()','baked model catalog').slice(4);
const baked=vm.runInNewContext('('+catalog+')').models;
const models=[...baked.map(x=>x.id),'custom-model','claude-mythos-5[1m]','claude-fable-5[1M]'];
const environments=[undefined,'','fable_5_mitigations','-fable_5_mitigations','fable_5_mitigations,-fable_5_mitigations','-fable_5_mitigations,fable_5_mitigations','claude-fable-5=-fable_5_mitigations','claude-fable-*= -fable_5_mitigations','claude-mythos-5=-fable_5_mitigations','claude-*=fable_5_mitigations;claude-fable-5=-fable_5_mitigations','*= -fable_5_mitigations;custom-model=fable_5_mitigations',' =fable_5_mitigations','CLAUDE-FABLE-5=-fable_5_mitigations','\ufefffable_5_mitigations\ufeff','\u0085fable_5_mitigations\u0085','claude-fable-5[1m]=-fable_5_mitigations','claude-fable-5 = fable_5_mitigations =ignored','xhigh_effort,','fable_5_mitigations;claude-mythos-*=-fable_5_mitigations'];
const cases=[];
for(const model of models)for(const environment of environments)for(const served of [false,true]){
 const context=vm.createContext({a:{CLAUDE_CODE_MODEL_CAPABILITIES:environment},cL:()=>({servedCapabilityLookup:()=>served}),xa:id=>baked.find(x=>x.id===id)});
 vm.runInContext(admission+lookup+gate,context);
 const expected=vm.runInContext(`AM(${JSON.stringify(model)},${JSON.stringify(model)})`,context);
 cases.push({model,environment:environment??null,served,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:ranges,scope:'Actual native AM and capability lookup including override grammar, served true admission, baked mitigation and Mythos fallback. Canonical first model input and served observations supplied; served/account/catalog acquisition and provider identity projection remain separate.',cases};
const json=JSON.stringify(output,null,2)+'\n',path=new URL('../../crates/core/tests/fixtures/loop_model_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(fs.readFileSync(path,'utf8'),json);else fs.writeFileSync(path,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native loop model cases`);
