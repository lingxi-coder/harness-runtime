#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex'),sourceByteRanges=[];
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
function take(first,last,start,label){const a=binary.indexOf(first,start),b=binary.indexOf(last,a+first.length);assert(a>=start&&b>a,label);const bytes=binary.subarray(a,b);sourceByteRanges.push({label,start:a,end:b,sha256:sha(bytes)});return bytes.toString('utf8');}
const truthy=take('function Le(e){','function gs(',177870000,'actual parsed bool');
const trim=take('function t(n){let e=n?.trim();','function u(',178161833,'actual parsed string');
const token=take('function lc(){','function aL()',180500000,'actual lc/Zvn');
const decode=take('function wU(r){','function cEn(',180530000,'actual JWT decoding');
const identity=take('function Io(){','function oVo()',180863000,'actual Io/Fyt/VJn');
const remote=take('function ma(){','function AKo()',179890000,'actual ma/lz');
const entrypoint=take('var s=new Set(["remote_cowork"','function _Oe()',178231000,'actual Pz');
const fast=take('function e_(){','function tT(',180872000,'actual e_/Ln');
const cases=[];
function add(input){
 const context=vm.createContext({INPUT:input,Buffer,Y:JSON.parse,x:()=>input.cached_access_token,To:()=>true,fn:()=>null,g8:()=>false,le:()=>({}),Xl:{orgStatus:{status:'pending'}}});
 vm.runInContext(truthy+trim+'var a={CLAUDE_CODE_REMOTE:Le(INPUT.remote),CLAUDE_CODE_ENVIRONMENT_KIND:t(INPUT.environment_kind),CLAUDE_CODE_ENTRYPOINT:t(INPUT.entrypoint)};var process={env:{CLAUDE_CODE_SESSION_ACCESS_TOKEN:INPUT.access_token}};'+token+decode+remote+entrypoint+identity+fast,context);
 const expected=vm.runInContext('({has_session_token:Zvn(),no_user_account:e_(),agent_owned_remote:Ln()})',context);
 cases.push({input,expected});
}
const service={org_service_name:'service',code_agent_id:'agent'},worker={'ccr:role':'session_worker',code_agent_id:'agent'};
const payloads=[null,[],true,3,'text',{},service,{...service,account_uuid:null},{...service,sub:null},{...service,org_service_name:''},{...service,org_service_name:3},{...service,code_agent_id:''},{...service,code_agent_id:true},{...service,org_service_name:' ',code_agent_id:' '},worker,{...worker,account_uuid:null},{...worker,'ccr:account_id':null},{...worker,sub:null},{...worker,code_agent_id:''},{...worker,code_agent_id:{}},{...worker,'ccr:role':'SESSION_WORKER'},{...worker,'ccr:role':null},{...worker,org_service_name:'service'}];
const jwt=payload=>'header.'+Buffer.from(JSON.stringify(payload)).toString('base64url')+'.signature';
for(const payload of payloads)for(const prefix of ['', 'sk-ant-si-'])for(const remote of ['false','true'])for(const environment_kind of [null,'byoc','bridge'])for(const entrypoint of [null,'remote_cowork','remote_cowork_trigger','cli'])add({access_token:prefix+jwt(payload),cached_access_token:null,remote,environment_kind,entrypoint});
for(const remote of [null,'','false','true','1','yes','on','TRUE',' yes ','no','off','0','falsehood','\uFEFFtrue\uFEFF','\u0085true\u0085'])for(const environment_kind of [null,'',' byoc ','\uFEFFbyoc\uFEFF','\u0085byoc\u0085'])for(const entrypoint of [null,' remote_cowork ','remote_cowork_trigger','remote_cowork_other'])add({access_token:jwt(worker),cached_access_token:null,remote,environment_kind,entrypoint});
for(const access_token of [null,'', ' ',jwt(service),jwt({sub:'user'})])for(const cached_access_token of [null,'',jwt(service),jwt({sub:'user'})])add({access_token,cached_access_token,remote:'true',environment_kind:'byoc',entrypoint:'remote_cowork'});
const valid=Buffer.from(JSON.stringify(service)).toString('base64url');
const encoded=[valid,valid+'=',valid+'===garbage',valid+'A',valid.replaceAll('-', '+').replaceAll('_','/'),valid.slice(0,4)+'!? \n'+valid.slice(4),valid.slice(0,4)+'Ł'+valid.slice(4),valid.slice(0,4)+'😀'+valid.slice(4),'','A','AA','AAA','====','bm9uLWpzb24',Buffer.from('{"org_service_name":"\u00FF","code_agent_id":"agent"}').toString('base64url')];
for(const mid of encoded)for(const prefix of ['', 'sk-ant-si-'])add({access_token:prefix+'header.'+mid+'.signature',cached_access_token:null,remote:'true',environment_kind:'byoc',entrypoint:'remote_cowork'});
for(const access_token of ['not-a-token','header.payload','header.payload.signature.extra','sk-ant-sid_cookie','header..signature'])add({access_token,cached_access_token:null,remote:'true',environment_kind:null,entrypoint:'remote_cowork'});
const rawJson=[
 '{"org_service_name":"service","code_agent_id":"agent","extra":1e400}',
 '{"org_service_name":"\\ud800","code_agent_id":"agent"}',
 '{"org_service_name":"service","code_agent_id":"\\udfff"}',
 '{"org_service_name":"service","code_agent_id":"agent","\\ud800":1}',
 '{"org_service_name":"service","code_agent_id":"agent","extra":"\\ud800"}',
 '{"org_service_name":"service","code_agent_id":"agent","account_uuid":1e400}',
 '{"org_service_name":"service","code_agent_id":"agent","extra":'+ '['.repeat(2000)+'0'+']'.repeat(2000)+'}',
 ...['01','-01','1e-','1.5.6','--1','NaN','Infinity','1e+400','-0.0','0e-1000'].map(value=>'{"org_service_name":"service","code_agent_id":"agent","extra":'+value+'}'),
 '{"org_service_name":"service","code_agent_id":"agent","code_agent_id":null}',
 '{"org_service_name":"service","code_agent_id":null,"code_agent_id":"agent"}',
];
for(const raw of rawJson)add({access_token:'header.'+Buffer.from(raw).toString('base64url')+'.signature',cached_access_token:null,remote:'true',environment_kind:null,entrypoint:'remote_cowork'});
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual parsed env bool/string, lc/Zvn, wU/Io/Fyt/VJn/e_ and ma/Pz/Ln helpers. Supplied cached session-token slot, intercepted JSON parse and ordinary Node Buffer. FD/socket/background-token acquisition, signature verification and complete CLI excluded.',cases};
const text=JSON.stringify(output,null,2)+'\n',file=new URL('../../deps/llm-client/tests/fixtures/session_identity_2_1_287.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),text);else writeFileSync(file,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native session identity cases`);
