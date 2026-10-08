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
const count=take('countTokens(n,r){let s=px(n)','toolRunner(n,r){','beta Messages.countTokens');
const normalize=take('function px(e){','class Mc extends','px current parameter conversion');
const values=[null,false,true,0,7,'',' x ','ab','💥','x\ny','x\u0000y',['one','two'],['one',null],[null,'two'],['one',''],[],{}, {toString:null},[['one','two'],false,7,{}]];
const cases=[];
for(const key of ['betas','user_profile_id','workspace_id'])for(const value of values){
 const input={body:{model:'claude-sonnet-4-6',messages:[],[key]:value},default_headers:{'anthropic-beta':'default','anthropic-workspace-id':'old'}};
 const expected=vm.runInNewContext(`(()=>{
  let ${arrays}
  ${headers}${normalize}
  const $n=Error,client={post:(url,options)=>({url,body:options.body,headers:options.headers})};
  const resource={_client:client,${count}};
  try{const output=resource.countTokens(INPUT.body);output.headers=Object.fromEntries(ve([INPUT.default_headers,output.headers]).values.entries());output.body_json=JSON.stringify(output.body);return output}catch(error){return {error:error.message}}
 })()`,{INPUT:input,Headers});
 cases.push({input,expected});
}
for(const body of [{model:'fixture',messages:[]},{model:'fixture',messages:[],output_format:{type:'json_schema'},output_config:{effort:'high'}},{model:'fixture',messages:[],output_format:{type:'json_schema'},output_config:{format:{type:'explicit'}}}]){
 const input={body,default_headers:{}};
 const expected=vm.runInNewContext(`(()=>{let ${arrays} ${headers}${normalize}const $n=Error,resource={_client:{post:(url,options)=>({url,body:options.body,headers:options.headers})},${count}};try{const o=resource.countTokens(INPUT.body);o.headers=Object.fromEntries(o.headers.values.entries());o.body_json=JSON.stringify(o.body);return o}catch(e){return {error:e.message}}})()`,{INPUT:input,Headers});cases.push({input,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual beta Messages.countTokens, px and ve/MH; supplied default client headers, Node Headers rather than embedded Bun. No request-options overrides, actual CLI count call, cloud endpoints or live-provider acceptance.',cases};
const json=JSON.stringify(output,null,2)+'\n',destination=new URL('../../deps/llm-client/tests/fixtures/count_tokens_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native countTokens cases`);
