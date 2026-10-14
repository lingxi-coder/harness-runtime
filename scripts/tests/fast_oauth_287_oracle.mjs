#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(first,last,start,label){const a=binary.indexOf(first,start),b=binary.indexOf(last,a+first.length);assert(a>=start&&b>a,label);const bytes=binary.subarray(a,b);sourceByteRanges.push({label,start:a,end:b,sha256:sha(bytes)});return bytes.toString('utf8');}
const constants=take('var VJr=Math.round(fY/86400)', ';',178157022,'actual OAuth scope constants');
const profile=take('function zp(){','function E3r()',180615120,'actual profile predicate');
const eligible=take('function YBe(){','function Ln()',180615120,'actual qu/YBe eligibility');
const refresh=take('let M=St.isAxiosError(A)&&','?fn()?.accessToken:void 0;',180882335,'actual refresh HTTP trigger');
const cases=[];
for(let bits=0;bits<16;bits++)for(const session_token of [false,true])for(const no_user_account of [false,true]){
 const scopes=['user:profile','user:ccr_inference','user:inference','unexpected'].filter((_,i)=>bits&(1<<i));
 const input={scopes,session_token,no_user_account};const context=vm.createContext({fY:31536000,fn:()=>({scopes}),To:()=>true,Zvn:()=>session_token,g8:s=>scopes.includes(s),e_:()=>no_user_account});
 vm.runInContext(constants+';'+profile+eligible,context);
 cases.push({input,expected:{profile:vm.runInContext('zp()',context),eligible:vm.runInContext('qu()',context)}});
}
const errors=[];
for(const status of [400,401,403,404,500])for(const body of [null,{},'OAuth token has been revoked','prefix OAuth token has been revoked suffix','oauth token has been revoked',{error:{message:'OAuth token has been revoked'}}]){
 const input={status,body};const expected=vm.runInNewContext(`${refresh}?fn()?.accessToken:void 0;!!M`,{A:{response:{status,data:body}},St:{isAxiosError:()=>true},fn:()=>({accessToken:'fixture'})});
 errors.push({input,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual OAuth scope constants, zp/qu/YBe and Rbe HTTP refresh trigger. Supplied scope array/session-token/no-user facts; To admitted. Session-token/JWT producers, account-source resolution and complete Rbe persistence excluded.',cases,errors};
const text=JSON.stringify(output,null,2)+'\n';
for(const relative of ['../../crates/core/tests/fixtures/fast_oauth_2_1_287.json','../../deps/llm-client/tests/fixtures/fast_oauth_2_1_287.json']){
 const file=new URL(relative,import.meta.url);
 if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
}
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} OAuth scope and ${errors.length} refresh trigger cases`);
