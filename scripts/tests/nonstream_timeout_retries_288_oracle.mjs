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
const integer = take('var N=/^[+-]?', 'function JL(', 179084600, 'actual integer environment coercer');
const boolean = take('function Le(e)', 'function zT(', 178927900, 'actual boolean environment coercers');
const schemas = take('function i(n)', 'export{Its,M};', 179192570, 'actual typed environment schemas');

const constants = take('Ljo=0.9,Njo=2', ',Fjo=',188933200,'actual duration threshold and default retries');
const guard = take('let an,fn=a.CLAUDE_CODE_NONSTREAMING_TIMEOUT_RETRIES', 'let In=await r.onError',188938700,'actual nonstream timeout retry guard');
pin('ii=M.int({min:0,digitsOnly:!0})',179255200,'actual explicit retry schema');
pin('CLAUDE_CODE_NONSTREAMING_TIMEOUT_RETRIES:()=>ii',179254200,'actual explicit retry registry');
const ctx=vm.createContext({jT:class extends Error {},rc:class extends Error {},m:()=>{}});
vm.runInContext('var $c=(()=>{'+integer+';return $c})(),M=(()=>{'+boolean+schemas+';return M})();var '+constants+';var a={get CLAUDE_CODE_NONSTREAMING_TIMEOUT_RETRIES(){return M.int({min:0,digitsOnly:true}).parse(explicit)}};function _9(){return persistent};function observe(){try{'+guard+';return {exhausted:false,count:V}}catch(e){if(!(e instanceof rc))throw e;return {exhausted:true,count:V}}}',ctx);
const prefixLength=guard.indexOf(';if(')+1;assert(prefixLength>0);
vm.runInContext('function limit(){'+guard.slice(0,prefixLength)+'return fn}',ctx);
const settings=[null,'','0','1','2','+2','-0','-1','3','002','2x','1e3','1_000','Infinity','NaN','1.5','0x2',' 2 ','\ufeff2\ufeff','\u00852','+','-','2147483648','9007199254740993','1'+'0'.repeat(400)];
const cases=[];
for(const explicit of settings)for(const persistent of [false,true])for(const long_stream of [false,true])for(const transport_timeout of [false,true])for(const count of [0,1,2,5])for(const timeout_ms of [0,1,1000,300000])for(const ratio of [0,0.89,0.9,1,1.2]){
 const elapsed_ms=Math.floor(timeout_ms*ratio);ctx.explicit=explicit===null?undefined:explicit;ctx.persistent=persistent;ctx.r={failedStreamOutlastedTimeout:long_stream,nonStreamingTimeoutMs:timeout_ms};ctx.rn=transport_timeout?vm.runInContext('new jT()',ctx):new Error('non-timeout');ctx.g={now:()=>elapsed_ms};ctx.Gt=0;ctx.V=count;ctx.h={};
 const expected=JSON.parse(JSON.stringify(vm.runInContext('observe()',ctx)));const limit=vm.runInContext('limit()',ctx);
 cases.push({explicit,persistent,long_stream,transport_timeout,count,timeout_ms,elapsed_ms,limit:limit??null,expected});
}
const output={version:'2.1.288',native_sha256:sha(binary),sourceByteRanges,scope:'Actual typed explicit-retry schema and current duration/count guard. CLI, physical source acquisition, retry scheduling and stream classification excluded.',cases};
const text=JSON.stringify(output,null,2)+'\n';const path=new URL('../../crates/llm-runtime/tests/fixtures/nonstream_timeout_retries_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),text);else writeFileSync(path,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} native nonstream timeout retry cases`);
