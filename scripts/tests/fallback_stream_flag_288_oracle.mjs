#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync, writeFileSync} from 'node:fs';
import vm from 'node:vm';

const binary = readFileSync(process.argv[2]);
const sha = value => createHash('sha256').update(value).digest('hex');
assert.equal(sha(binary), 'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges = [];
function take(first, last, offset, label) {
  const start = binary.indexOf(first, offset);
  const end = binary.indexOf(last, start + first.length);
  assert(start >= offset && end > start && end - start < 3000, label);
  const bytes = binary.subarray(start, end);
  sourceByteRanges.push({label, start, end, sha256: sha(bytes)});
  return bytes.toString();
}
function pin(source, offset, label) {
  const start = binary.indexOf(source, offset);
  assert(start >= offset && start - offset < 3000, label);
  const end = start + Buffer.byteLength(source);
  sourceByteRanges.push({label, start, end, sha256: sha(binary.subarray(start, end))});
  return source;
}
const spread=pin('{...Te,stream:!1}',188968000,'actual nonstream fallback parameter spread');
const ctx=vm.createContext({});
const cases=[];
for(const stream of [undefined,false,true,null,'extra',{nested:true},[1,2]])for(const reverse of [false,true])for(const numeric of [false,true]){
 let body={model:'claude-sonnet-4-6',messages:[{role:'user',content:'fixture'}],max_tokens:1000};
 if(stream!==undefined)body={stream,...body};
 body.extension='retained';
 if(numeric)body={'2':2,...body,'0':0};
 if(reverse)body=Object.fromEntries(Object.entries(body).reverse());
 ctx.Te=body;
 const expected_json=vm.runInContext('JSON.stringify(('+spread+'))',ctx);
 cases.push({body,expected_json});
}
const output={version:'2.1.288',native_sha256:sha(binary),sourceByteRanges,scope:'Actual AWo final parameter spread and JavaScript serialization; valid object inputs. Full envelopes, cloud transforms, signing and physical dispatch excluded.',cases};
const text=JSON.stringify(output,null,2)+'\n';const path=new URL('../../deps/llm-client/tests/fixtures/fallback_stream_flag_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),text);else writeFileSync(path,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} actual fallback stream-flag byte cases`);
