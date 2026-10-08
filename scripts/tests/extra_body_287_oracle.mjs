#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]), sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(base,startMarker,endMarker,label){
 const start=binary.indexOf(startMarker,base),end=binary.indexOf(endMarker,start+startMarker.length);
 assert(start>=base&&end>start,label);const bytes=binary.subarray(start,end);
 sourceByteRanges.push({label,start,end,sha256:sha(bytes)});return bytes.toString('utf8');
}
const bom=take(178269091,'function Do(e){','function j(e,','BOM removal');
const parser=take(178269091,'var be=8192;','function Wwe(e)','gt strict JSON parser/cache');
const object=take(178423956,'function N(e){','import{isAbsolute','N object predicate');
const stringify=take(178066645,'function _(e,n,r){','function fD(e)','instrumented JSON.stringify');
const afk=take(179726616,'function K_t(e){','var o4r=','AFK predicate');
const cleanup=take(185762130,'function hV(e){','function ', 'hV extra body sanitation');
const values=[null,false,true,0,7,[],['keep','other'], 'keep, other', 'afk-mode', 'AFK-MODE-2026', 'x-afk-mode-y', 'afK-mode,keep',
 'keep, afk-mode , keep', ['keep','AFK-MODE',null], [['keep',null],['afk-mode','other']],
 '\uFEFFafk-mode\uFEFF,\uFEFFkeep\uFEFF', '\u0085afk-mode\u0085,\u0085keep\u0085', {}, {toString:'afk-mode'}, [{toString:null}],
 1e21,1e-7,1e-6,9007199254740993];
const inputs=[];
for(const key of ['betas','anthropic_beta'])for(const value of values)for(const betas of [[],['computed','keep']])inputs.push({extra:{before:true,[key]:value,after:1},betas});
for(const user_id of ['user','{"tk":"secret","account":"abc"}', '{"before":1,"tk":false,"after":2}',
 '{"tk":null,"2":2,"0":0,"01":1,"nested":{"tk":"kept"}}', '\uFEFF{"tk":"secret","account":"abc"}',
 '{"tk":true,"number":1e21,"small":1e-7,"zero":-0}', '{"tk":true,"text":"💥"}',
 'null','[]','{"account":"abc"}','broken'])inputs.push({extra:{metadata:{user_id,other:'kept'}},betas:[]});
for(const metadata of [null,[],{}, {user_id:null},'not-an-object'])inputs.push({extra:{metadata},betas:[]});
for(const user_id of [
 '{"tk":"secret","id":"\\ud800","\\udc00":"value"}',
 '{"t\\u006b":1,"id":"\\ud800A\\udc00💥"}',
 '{"tk":false,"\\ud800":1,"\\udc00":2,"\\ud800":3,"0":0,"4294967294":4,"4294967295":5}',
 '{"__proto__":{"tk":"nested"},"tk":null,"value":{"tk":"kept"}}',
 '{"tk":0,"large":1e999,"small":-1e-999,"integer":18446744073709551615}',
 '{"tk":0,"value":01}', '{"tk":0,"value":1.}', '{"tk":0,"value":1e}',
 '{"tk":0,}', '{"tk":0,"value":[1,]}', '{"tk":0,"value":[1 2]}',
 '{"tk":0,"value":"\\uQQQQ"}', '{"tk":0}false',
 '  {"id":"\\ud800"}  ',
 ...[512,2048].map(depth=>'{"tk":"secret","deep":'+ '['.repeat(depth)+'"\\ud800"'+']'.repeat(depth)+'}')
]) inputs.push({extra:{metadata:{user_id,other:'kept'}},betas:[]});
const cases=inputs.map(input=>{
 const expected=vm.runInNewContext(`(()=>{
  const process={env:{CLAUDE_CODE_EXTRA_BODY:JSON.stringify(INPUT.extra)}};
  const qM=()=>false,t=()=>{},l=String,st=e=>e,d=()=>{},sT=x=>x;
  const Qx=fn=>Object.assign(fn,{cache:new Map()}),ng=()=>({[Symbol.dispose]() {}}),Se=x=>x;
  ${bom}${parser}${object}${stringify}${afk}${cleanup}
  try {const value=hV(INPUT.betas);return {value,json:JSON.stringify(value)}}catch(error){return {error:error.message}}
 })()`,{INPUT:input});
 return {input,expected};
});
const rawCases=['', 'null','[]','7','broken','{"x":1}', '\uFEFF{"x":1}', '\uFEFF\uFEFF{"x":1}', ' {"x":1}', '\uFEFF{"metadata":{"user_id":"{\\"tk\\":true,\\"id\\":\\"x\\"}"}}'].map(raw=>{
 const expected=vm.runInNewContext(`(()=>{const process={env:{CLAUDE_CODE_EXTRA_BODY:RAW}},qM=()=>false,t=()=>{},l=String,st=e=>e,d=()=>{},sT=x=>x,Qx=fn=>Object.assign(fn,{cache:new Map()}),ng=()=>({[Symbol.dispose]() {}}),Se=x=>x;${bom}${parser}${object}${stringify}${afk}${cleanup}return hV([])})()`,{RAW:raw});return {raw,expected};
});
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges,scope:'Actual hV, gt/se/Do, N, K_t and instrumented JSON.stringify. HIPAA false, logging/cache/measurement supplied; map sanitation and metadata string bytes, not complete HTTP serialization or arbitrary outer extra-body UTF16 parsing. Metadata includes escaped unpaired keys/leaves, duplicates, overflow and 2048-deep arrays.',cases,rawCases};
const json=JSON.stringify(output,null,2)+'\n';
for(const path of ['../../deps/llm-client/tests/fixtures/extra_body_2_1_287.json','../../crates/llm-runtime/tests/fixtures/extra_body_2_1_287.json']){
 const destination=new URL(path,import.meta.url);
 if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
}
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} sanitation cases and ${rawCases.length} raw-parser cases`);
