#!/usr/bin/env node
import fs from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
const binary=fs.readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex'),ranges=[];
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
function take(base,first,last,label){const start=binary.indexOf(first,base),end=binary.indexOf(last,start+first.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const truth=take(177878934,'function Le(e){','function IT(e)','native typed environment bool');
const table=take(179125203,'Vtr={',';function O()','current baked catalog').slice(4);
const models=vm.runInNewContext('('+table+')').models;
const admission=take(179125203,'var N={','function O9o(','served feature admission');
const lookup=take(179125203,'function S(','function Cje(','native ny and ordered N8r overrides');
const fast=take(180615120,'function Ry(','function Lk(','native Ry');
const global=take(180615120,'function To(){','function e_()','native To');
const cases=[];
const identities=[['claude-opus-4-7','claude-opus-4-7'],['claude-opus-5-5','claude-opus-5-5'],['claude-sonnet-5-5','claude-sonnet-5-5'],['anthropic/claude-opus-5-5','claude-opus-5-5'],['CLAUDE-OPUS-5-5','claude-opus-5-5'],['claude-opus-5-5[1m]','claude-opus-5-5'],['claude-opus-4-7-20260828','claude-opus-4-7'],['unknown-model','unknown-model']];
const overrides=[null,'fast_mode','-fast_mode','claude-opus-4-7=fast_mode;claude-opus-5-5=-fast_mode','claude-opus-*=fast_mode;-fast_mode;claude-opus-5*=fast_mode','CLAUDE-OPUS-5-5=-fast_mode','\uFEFFclaude-opus-5-5\uFEFF=\uFEFF-fast_mode\uFEFF'];
for(const [model,canonical] of identities)for(const provider of ['firstParty','gateway','bedrock'])for(const disabled of [null,'true','false','wat','1','\uFEFFon\uFEFF','\u0085on\u0085'])for(const environment of overrides){
 const context=vm.createContext({INPUT:{disabled},N:{},a:{CLAUDE_CODE_MODEL_CAPABILITIES:environment??undefined},cL:()=>({}),xa:id=>models.find(x=>x.id===id),He:()=>provider,Rt:x=>x,Be:()=>canonical,Py:()=>model});
 vm.runInContext(truth+'a.CLAUDE_CODE_DISABLE_FAST_MODE=Le(INPUT.disabled??undefined);'+admission+lookup+global+fast,context);
 cases.push({model,canonical,provider,disabled,environment,expected:vm.runInContext(`Ry(${JSON.stringify(model)})`,context)});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:ranges,scope:'Actual To/Ry/ny/N8r and Le with supplied resolved identities. Covers current environment acquisition; remote served observations, org/account availability, canonical identity discovery and full CLI lifecycle are separate.',cases};
const json=JSON.stringify(output,null,2)+'\n',file=new URL('../../crates/llm-runtime/tests/fixtures/fast_environment_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(fs.readFileSync(file,'utf8'),json);else fs.writeFileSync(file,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native Fast environment cases`);
