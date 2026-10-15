#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const start=binary.indexOf('function px(e){',184171613),end=binary.indexOf('class Mc',start);
assert(start>=184171613&&end>start);const bytes=binary.subarray(start,end);
const cases=[];
for(const format of [null,false,true,0,7,'','json','💥',[],{}, {type:'json_schema',schema:{type:'object'}}]){
 for(const config of [null,false,7,'ab','💥',[],['x',null],{}, {effort:'high',format:null}, {format:false,effort:'low'}, {format:0}, {format:''}, {format:[]}, {format:{}}, {format:{type:'existing'}}, {'2':'two','0':'zero',extension:true}]){
  const body={model:'claude-sonnet-4-6',output_config:config,metadata:{user_id:'fixture'},output_format:format,max_tokens:16};
  const expected=vm.runInNewContext(`(()=>{const $n=Error;${bytes.toString('utf8')}try{return {body_json:JSON.stringify(px(BODY))}}catch(error){return {error:error.message}}})()`,{BODY:body});cases.push({body,expected});
 }
}
for(const body of [{model:'fixture'}, {model:'fixture',output_format:{type:'plain'},metadata:{user_id:'fixture'}}, {model:'fixture',output_format:{type:'plain'},output_config:{format:null},tail:true}]){
 const expected=vm.runInNewContext(`(()=>{const $n=Error;${bytes.toString('utf8')}return {body_json:JSON.stringify(px(BODY))}})()`,{BODY:body});cases.push({body,expected});
}
const output={version:'2.1.287',native_sha256:sha(binary),sourceByteRanges:[{label:'px SDK output-format normalization',start,end,sha256:sha(bytes)}],scope:'Unmodified current native px with only its error class supplied; exact resulting JSON bytes and conflict messages. Not whole SDK/CLI acceptance.',cases};
const json=JSON.stringify(output,null,2)+'\n';
for(const path of ['../../deps/llm-client/tests/fixtures/output_format_2_1_287.json','../../crates/llm-runtime/tests/fixtures/output_format_2_1_287.json']){
 const destination=new URL(path,import.meta.url);if(process.argv.includes('--check'))assert.equal(readFileSync(destination,'utf8'),json);else writeFileSync(destination,json);
}
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native output-format normalization cases`);
