#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex'),sourceByteRanges=[];
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
function take(first,last,start,label){const a=binary.indexOf(first,start),b=binary.indexOf(last,a+first.length);assert(a>=start&&b>a,label);const bytes=binary.subarray(a,b);sourceByteRanges.push({label,start:a,end:b,sha256:sha(bytes)});return bytes.toString('utf8');}
const source=take('function rk(', 'class Pk',181262000,'actual rk/xk/bK/vK');
const parsedString=take('function t(n){let e=n?.trim();','function u(',178161833,'actual env string parser');
const cases=[];
function add(input){
 const stored=input.stored_token===null?null:{accessToken:input.stored_token,scopes:['stored:scope']};
 const context=vm.createContext({INPUT:input,sO:()=>input.descriptor_token,Zeo:()=>input.descriptor_scopes,Uz:()=>input.from_background_snapshot,Jl:()=>input.host_managed,zn:()=>({read:()=>({claudeAiOauth:stored})}),d:()=>{}});
 vm.runInContext(parsedString+'var a={CLAUDE_CODE_OAUTH_TOKEN:t(INPUT.environment_token),CLAUDE_CODE_OAUTH_SCOPES:t(INPUT.environment_scopes)};'+source,context);
 const result=vm.runInContext('vK()',context);
 const expected=result===null?null:{access_token:result.accessToken,scopes:result.scopes??[],source:vm.runInContext('!!a.CLAUDE_CODE_OAUTH_TOKEN',context)?'environment':result===stored?'store':'descriptor'};
 cases.push({input,expected});
}
for(const environment_token of [null,'','token','\uFEFF token \uFEFF'])for(const environment_scopes of [null,'',' \t\n','user:profile user:inference','user:inference\uFEFFuser:profile','user:inference\u0085user:profile'])for(const descriptor_token of [null,'','fd'])for(const descriptor_scopes of [null,[],['user:profile']])for(const stored_token of [null,'stored'])for(const from_background_snapshot of [false,true])for(const host_managed of [false,true])add({environment_token,environment_scopes,descriptor_token,descriptor_scopes,stored_token,from_background_snapshot,host_managed});
for(const environment_token of [' ', '\uFEFF','\u0085','\u0085 token \u0085'])for(const environment_scopes of [null,'\uFEFF','\u0085','user:profile\r\nuser:inference'])add({environment_token,environment_scopes,descriptor_token:' ',descriptor_scopes:null,stored_token:'stored',from_background_snapshot:false,host_managed:false});
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual rk/xk/vK token and scope selection, projected onto consumed accessToken/scopes/source. Parsed environmental token, supplied FD scopes/background/host flags and saved snapshot. Descriptor/socket acquisition, full store payload, expiry/metadata and model source policy excluded.',cases};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../deps/llm-client/tests/fixtures/oauth_source_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native OAuth source cases`);
