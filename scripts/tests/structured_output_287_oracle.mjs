#!/usr/bin/env node
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const binary=readFileSync(process.argv[2]);
const digest=bytes=>createHash('sha256').update(bytes).digest('hex');
const nativeSha256=digest(binary);
assert.equal(nativeSha256,'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(base,startMarker,endMarker,label){
  const start=binary.indexOf(startMarker,base),end=binary.indexOf(endMarker,start+startMarker.length);
  assert(start>=base&&end>start,label);
  const bytes=binary.subarray(start,end);
  sourceByteRanges.push({label,start,end,sha256:digest(bytes)});
  return bytes.toString('utf8');
}
const disable=take(179726616,'function rK(){','function Y_t(e)','rK environment string and HIPAA gate');
const truth=take(177878934,'function Le(e){','function IT(e)','Le/gs boolean parsing');
const parser=take(178161833,'function i(n){','export{B7o,D}','D typed environment parsers');
const boolSlot=take(178163242,'jE=D.bool()',',JE=D.bool()','experimental-beta bool schema');
const getter=take(178163242,'function ZZe(t,o){','var a=ZZe','ZZe cached environment getter');
const registration=take(179726616,'Koe=R("structured_outputs",',',ner=R(','structured-output beta registration');
const structuredBeta=JSON.parse(registration.slice('Koe=R("structured_outputs",'.length,-1));
const eligibility=take(180615120,'function own(e){','function rN()','own structured-output eligibility');
const main=take(185762130,'function ZLo(e,n,r,s){','async function nPo(e','ZLo automatic main format');
const sideGate=take(185762130,'let $t=Boolean(b)&&','let It=dLr(','side-query automatic format and beta');
const sideConfig=take(185762130,'let rn={...$t&&','Qt=QD(e.querySource)','side-query format and effort fields');
const modelOrder=take(180615120,'var av=[','function nwn()','actual av/lr model-order comparison');
const cases=[];
for(const env of [null,'','1','0','false','true',' ','FALSE'])for(const hipaa of [false,true]){
  for(const eligible of [false,true])for(const opus41 of [false,true]){
    for(const capability of [false,true])for(const explicit of [false,true]){
      const input={env,hipaa,eligible,opus41,capability,explicit};
      const expected=vm.runInNewContext(`(()=>{
        const process={env:INPUT.env===null?{}:{CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:INPUT.env}},Oc=Number;
        ${truth}${parser}${getter}let ${boolSlot};const a=ZZe({CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS:jE},null);
        const ol=()=>INPUT.hipaa,Be=()=>INPUT.opus41?'claude-opus-4-1':'claude-sonnet-4-6';
        const cc=()=>INPUT.eligible,zF=value=>value;${modelOrder}
        const a_e=()=>INPUT.capability,Koe=STRUCTURED_BETA;
        ${disable}${eligibility}${main}
        const schema={type:'json_schema',schema:{type:'object',properties:{ok:{type:'boolean'}},required:['ok'],additionalProperties:false}};
        const config={effort:'low',...INPUT.explicit&&{format:{type:'explicit_fixture'}}},betas=[];
        ZLo(schema,config,betas,'fixture');
        const b=schema,ht=fn=>fn(),We='fixture',kt=[],un={effort:'low'};
        ${sideGate}${sideConfig}dummy=undefined;
        return {disabled:Boolean(rK()),main:{config,betas},side:{config:rn,betas:kt,hasConfig:Yt}};
      })()`,{INPUT:input,STRUCTURED_BETA:structuredBeta});
      cases.push({input,expected:JSON.parse(JSON.stringify(expected))});
    }
  }
}
assert.equal(cases.length,256);
const output={version:'2.1.287',native_sha256:nativeSha256,sourceByteRanges,
  scope:'Exact Le/D.bool/ZZe typed environment getter, rK, own, ZLo and side-query config. Model eligibility, capability and HIPAA taint are supplied inputs. This supersedes the previous raw-string environment stub.',cases};
const file=fileURLToPath(new URL('../../crates/llm-runtime/tests/fixtures/structured_output_2_1_287.json',import.meta.url));
const json=JSON.stringify(output,null,2)+'\n';
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),json);
else writeFileSync(file,json);
process.stdout.write(`${process.argv.includes('--check')?'checked':'wrote'} native structured-output policy: ${cases.length} cases\n`);
