#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=bytes=>createHash('sha256').update(bytes).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(base,startMarker,endMarker,label){const start=binary.indexOf(startMarker,base),end=binary.indexOf(endMarker,start+startMarker.length);assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const levels=take(178423956,'nu=[',',dL=','native effort ranks');
const ranks=take(181694118,'function $v(e){','function K(e)','native b ranks');
const fold=take(181694118,'function K(e){','function S2t(e)','K original-source minimum');
const identity=take(181694118,'function qSe(e){','function be(e,n)','qSe canonical equality');
const strip=take(178300800,'function Ct(e){','function cI(e,n)','context identity strip');
const cases=[];
const ids={'sonnet':'claude-sonnet-4-6','claude-sonnet-4-6-20260928':'claude-sonnet-4-6','anthropic.claude-sonnet-4-6-v1:0':'claude-sonnet-4-6','opus':'claude-opus-5-5','claude-opus-5-5[1m]':'claude-opus-5-5[1m]'};
for(const model of ['claude-sonnet-4-6','sonnet','claude-sonnet-4','claude-opus-5-5','claude-opus-5-5[1m]','unknown-model'])
 for(const first of [undefined,'low','medium','high','xhigh','max'])for(const second of [undefined,'low','medium','high','xhigh','max'])
 for(const per of [{},{sonnet:{maxEffortLevel:'max'}},{'claude-sonnet-4':{maxEffortLevel:'low'}},{sonnet:{maxEffortLevel:'xhigh'},'claude-sonnet-4-6':{maxEffortLevel:'medium'}},{'anthropic.claude-sonnet-4-6-v1:0':{maxEffortLevel:'high'}},{opus:{maxEffortLevel:'max'}},{sonnet:{effortLevel:'low'}}]) {
  const layers=[{...first!==undefined&&{maxEffortLevel:first},modelSettings:per},{...second!==undefined&&{maxEffortLevel:second}},{modelSettings:{unrelated:{maxEffortLevel:'low'}}}];
  const input={model,layers,identities:ids};
  const expected=vm.runInNewContext(`(()=>{let ${levels};${ranks}${fold}${identity}${strip}
    const bu=()=>INPUT.layers.slice(0,1),Xo=()=>[1,2],ge=i=>INPUT.layers[i],Rt=e=>INPUT.identities[e]??e,Be=e=>e;
    return K(INPUT.model);
  })()`,{INPUT:input});
  cases.push({input,expected});
 }
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual K/b/qSe/Ct, original layers and canonical identity results supplied. Alias/catalog/override acquisition and settings schema parsing are not native acceptance here.',cases};
const json=JSON.stringify(output,null,2)+'\n',destination=new URL('../../crates/core/tests/fixtures/effort_settings_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native settings-cap cases`);
