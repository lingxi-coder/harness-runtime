#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]);
const hash=bytes=>createHash('sha256').update(bytes).digest('hex');
assert.equal(hash(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const ranges=[];
function take(first,last,offset,label){
 const start=binary.indexOf(first,offset),end=binary.indexOf(last,start+first.length);
 assert(start>=offset&&end>start&&end-start<4000,label);
 const bytes=binary.subarray(start,end);ranges.push({label,start,end,sha256:hash(bytes)});return bytes.toString();
}
const controls=take('function lNe(e)','var wle=',188772000,'native fallback materialization, category, start and malformed predicates');
const counters=take('var wle=','function cNe(',188773300,'native finite nonnegative iteration counters');
const iterations=take('function $cn(e)','function Ucn(',188773900,'native fallback usage iteration projection');
const context=vm.createContext({});vm.runInContext(controls+counters+iterations,context);
const copy=value=>value===undefined?null:JSON.parse(JSON.stringify(value));
const models=[null,[],{}, {model:null},{model:0},{model:''},{model:' '},{model:'primary'}, {model:'target',extra:true}];
const triggers=[null,{}, {type:'other',category:'cyber'}, {type:'refusal',category:''}, {type:'refusal',category:'cyber'}, {type:'refusal',category:'x'.repeat(64)}, {type:'refusal',category:'x'.repeat(65)}, {type:'refusal',category:'😀'.repeat(32)}, {type:'refusal',category:'😀'.repeat(33)}];
const blockCases=[];
for(const from of models)for(const to of models)for(const trigger of triggers){
 const input={type:'fallback',from,to,trigger,extra:'omitted'};
 const expected=copy(context.UHt(input));blockCases.push({input,expected,materialized:expected?copy(context.lNe(expected)):null});
}
for(const input of [null,[],{}, {type:'text',text:'hello'}, {type:'FALLBACK'}])blockCases.push({input,expected:copy(context.UHt(input)),materialized:null});
const streamCases=[];
for(const index of [undefined,null,0,-1,0.5,1e20,'0',true])for(const content_block of [null,{}, {type:'text'}, ...blockCases.filter((_,i)=>i%13===0).map(c=>c.input)])for(const type of ['content_block_start','content_block_stop']){
 const input={type,index,content_block};streamCases.push({input,expected:copy(context.dNe(input)),malformed:context.HHt(input)});
}
const counts=[null,false,'2',-1,0,0.5,1,1e20,{},[]];
const usageCases=[null,[],{}, {iterations:null}, {iterations:[]}].map(input=>({input,expected:copy(context.$cn(input))}));
for(const count of counts)for(const model of [undefined,null,0,'','primary','target'])for(const type of [undefined,0,'','message','fallback_message']){
 const entry={type,model,input_tokens:count,output_tokens:count,cache_read_input_tokens:count,cache_creation_input_tokens:count};
 const input={iterations:[null,false,{}, {type:'fallback_message',model:'first',input_tokens:2}, entry, {type:'fallback_message',model:0}, {type:'message',model:'ignored'}]};
 usageCases.push({input,expected:copy(context.$cn(input))});
}
const data={version:'2.1.288',native_sha256:hash(binary),sourceByteRanges:ranges,scope:'Actual lNe/vle/BHt/dNe/UHt/Cle/HHt/wle/$cn execute without substituted dependencies. JSON-wire observations only; lane admission, salvage, selected model and fallback credit/cost ownership remain separate.',blockCases,streamCases,usageCases};
const path=new URL('../../deps/llm-client/tests/fixtures/fallback_response_2_1_288.json',import.meta.url),text=JSON.stringify(data,null,2)+'\n';
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),text);else writeFileSync(path,text);
console.log(JSON.stringify({blockCases:blockCases.length,streamCases:streamCases.length,usageCases:usageCases.length,ranges:ranges.length}));
