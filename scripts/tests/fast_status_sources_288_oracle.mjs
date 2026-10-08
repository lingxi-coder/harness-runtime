#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex'),sourceByteRanges=[];
assert.equal(sha(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
function take(first,last,start,label){const a=binary.indexOf(first,start),z=binary.indexOf(last,a+first.length);assert(a>=start&&z>a,label);const bytes=binary.subarray(a,z);sourceByteRanges.push({label,start:a,end:z,sha256:sha(bytes)});return bytes.toString('utf8');}
const profile=take('function Qp(){','function n9r()',182094900,'actual profile-scope predicate');
const eligible=take('function s2e(){','function xn(){',181680100,'actual td/s2e status OAuth eligibility');
const get=take('let g=async()=>{',';async function h(){',181690600,'actual independent global OAuth status auth');
const cases=[];
for(let bits=0;bits<16;bits++)for(const token of [null,'','token'])for(const key of [null,'','key'])for(const session_token of [false,true])for(const no_user_account of [false,true]){
 const scopes=['user:profile','user:ccr_inference','user:inference','unexpected'].filter((_,i)=>bits&(1<<i)),input={token,scopes,key,session_token,no_user_account};
 const context=vm.createContext({C4:'user:profile',mn:()=>token===null?null:{accessToken:token,scopes},Io:()=>true,STn:()=>session_token,Q8:s=>scopes.includes(s),s_:()=>no_user_account,KEY:key,Ex:auth=>auth});
 let expected=null;try{expected=await vm.runInContext(profile+eligible+'let r=KEY;'+get+';g()',context);}catch(error){assert.equal(error.message,'No auth available');}
 cases.push({input,expected});
}
const output={version:'2.1.288',native_sha256:sha(binary),sourceByteRanges,scope:'Actual Qp/td/s2e and status g over the independent global mn OAuth snapshot and previously captured alternate API key. First-party/global admission, session identity and scope sources supplied. Model credentials do not occur in these helpers. Source acquisition, cache lifecycle and full native dispatch excluded.',cases};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../crates/llm-runtime/tests/fixtures/fast_status_sources_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} independent status-source cases`);
