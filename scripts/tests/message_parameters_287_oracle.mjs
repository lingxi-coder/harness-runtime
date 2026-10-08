#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(startMarker,endMarker,label){const start=binary.indexOf(startMarker,184171613),end=binary.indexOf(endMarker,start+startMarker.length);assert(start>=184171613&&end>start,label);const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');}
const arrays=take('Yn=(e)=>','function Pl(e)','Qg array predicate');
const headers=take('var kR=Symbol.for("brand.privateNullableHeaders")','function RR(e)','ve native headers');
const create=take('create(n,r){let s=px(n)','parse(n,r){','beta Messages.create');
const cases=[];
for(const key of ['betas','user_profile_id','workspace_id'])for(const value of [null,false,true,0,7,'',' x ','\r\n x \t','x\ny','x\u0000y','💥',['one','two'],['one',null],[null,'two'],['one',''],[],{}, {toString:null}]){
 const input={body:{model:'claude-sonnet-4-6',max_tokens:16,messages:[],[key]:value},default_headers:{'anthropic-beta':'default','anthropic-workspace-id':'old'}};
 const expected=vm.runInNewContext(`(()=>{
  let ${arrays}
  ${headers}
  const fx={},YB=[],ip={},Pf=()=>({}),px=x=>x;
  const client={_options:{timeout:600000},post:(path,options)=>({body:options.body,headers:options.headers})};
  const resource={_client:client,${create}};
  try{const output=resource.create(INPUT.body);output.headers=Object.fromEntries(ve([INPUT.default_headers,output.headers]).values.entries());return output}catch(error){return {error:error.message}}
 })()`,{INPUT:input,Headers});
 cases.push({input,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual beta Messages.create and ve/MH headers with px identity, no attachments, model warnings/timeouts inactive. Default header precedence supplied as ve input; Node Headers builtin, not embedded Bun fetch; no countTokens, output_format conversion or signed/cloud transport acceptance.',cases};
const json=JSON.stringify(output,null,2)+'\n';
const destination=new URL('../../deps/llm-client/tests/fixtures/message_parameters_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native SDK parameter cases`);
