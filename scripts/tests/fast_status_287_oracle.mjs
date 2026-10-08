#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(first,last,label){const start=binary.indexOf(first,180615120),end=binary.indexOf(last,start+first.length);assert(start>=180615120&&end>start,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const ox=take('var rx=1e4;async function ox(','var ix=30000','actual status GET');
const nx=take('var tx=["free","preference"','function U7e(','actual disabled-reason normalization');
const cases=[];
for(const oauth of [false,true])for(const enabled of [true,false,'false',0,[]]){
 const input={oauth,enabled},captured=[],body={enabled,disabled_reason:'unrecognized'};
 const context=vm.createContext({dn:()=>({BASE_API_URL:'https://api.anthropic.com'}),Mo:()=>'test-agent',cp:'oauth-2025-04-20',St:{get:async(url,options)=>{captured.push({method:'GET',url,...options});return {data:body};}}});
 vm.runInContext(ox+nx,context);context.AUTH=oauth?{accessToken:'test-oauth'}:{apiKey:'test-key'};
 const response=await vm.runInContext('ox(AUTH)',context);context.REASON=response.disabled_reason;
 const expected={...captured[0],enabled:!!response.enabled,disabled_reason:response.enabled?null:vm.runInContext('nx(REASON)',context)};
 cases.push({input,expected});
}
const reasons=[null,'free','preference','extra_usage_disabled','network_error','unknown','',false,7,{},[]].map(input=>({input,expected:vm.runInNewContext(nx+'nx(INPUT)',{INPUT:input})}));
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual ox GET options and nx reason normalization. Supplied OAuth beta constant and UA, intercepted Axios get; Axios implicit headers, auth eligibility, refresh, startup and live transport excluded.',cases,reasons};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../deps/llm-client/tests/fixtures/fast_status_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native GET and ${reasons.length} reason cases`);
