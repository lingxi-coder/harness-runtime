#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(first,last,start,label){const a=binary.indexOf(first,start),b=binary.indexOf(last,a+first.length);assert(a>=start&&b>a,label);const bytes=binary.subarray(a,b);sourceByteRanges.push({label,start:a,end:b,sha256:sha(bytes)});return bytes.toString('utf8');}
const constants=take('var VJr=Math.round(fY/86400)',';',178157022,'actual scope constants');
const profile=take('function zp(){','function E3r()',180615120,'actual profile predicate');
const eligible=take('function YBe(){','function Ln()',180615120,'actual qu/YBe eligibility');
const get=take('let g=async()=>{',';async function h(){',180882335,'actual Rbe auth selection');
const snapshot=take('function NQn(){','function eJe()',181242000,'actual helper cache snapshot');
const keys=take('function mb(e={}){','function OWt()',181237000,'actual API key source selection');
const safe=take('function Xb(){','function dwn()',181235000,'actual safe API key lookup');
const cases=[];
for(let bits=0;bits<16;bits++)for(const token of [null,'','token'])for(const key of [null,'','key'])for(const session_token of [false,true])for(const no_user_account of [false,true]){
 const scopes=['user:profile','user:ccr_inference','user:inference','unexpected'].filter((_,i)=>bits&(1<<i));
 const input={token,scopes,key,session_token,no_user_account};
 const context=vm.createContext({fY:31536000,fn:()=>token===null?null:{accessToken:token,scopes},To:()=>true,Zvn:()=>session_token,g8:s=>scopes.includes(s),e_:()=>no_user_account,KEY:key,ox:auth=>auth});
 let expected=null;try{expected=await vm.runInContext(constants+';'+profile+eligible+'let r=KEY;'+get+';g()',context);}catch(error){assert.equal(error.message,'No auth available');}
 cases.push({input,expected});
}
const key_sources=[];
for(const static_key of [null,'','static'])for(const helper of [false,true])for(const cached of [null,'','cached'])for(const stored of [null,'','stored']){
 const input={static_key,helper,cached,stored};
 const context=vm.createContext({a:{ANTHROPIC_API_KEY:static_key},f8:()=>false,Cb:()=>false,$3t:()=>false,Le:()=>false,ZZn:()=>'approved',koe:k=>k,AZ:()=>null,vh:()=>helper,NQn:()=>cached,jQn:()=>stored===null?null:{key:stored,source:'keychain'}});
 key_sources.push({input,expected:vm.runInContext(keys+'mb().key',context)});
}
const cache=[];
for(const value of [null,'','cached'])for(const expired of [false,true]){
 const input={value,expired};
 const expected=vm.runInNewContext(snapshot+'NQn()',{HWt:{of:()=>({cache:value===null?null:{value,timestamp:expired?0:Date.now()}})},U:()=>({host:{}})});
 cache.push({input,expected});
}
const safe_lookup=[];
for(const value of [null,'','key'])for(const throws of [false,true]){
 const input={value,throws};const expected=vm.runInNewContext(safe+'Xb()',{nZ:()=>{if(throws)throw Error('fixture');return value;}});safe_lookup.push({input,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual Rbe g auth selection, mb key precedence, NQn cached helper read and Xb failure suppression. Supplied approved/static/store sources, scope/session/no-user predicates, and intercepted ox. Trust/environment/managed/keychain producers, complete Rbe and live CLI excluded.',cases,key_sources,cache,safe_lookup};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../crates/llm-runtime/tests/fixtures/fast_auth_sources_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} auth selections, ${key_sources.length} key sources, ${cache.length} cached values and ${safe_lookup.length} safe lookups`);
