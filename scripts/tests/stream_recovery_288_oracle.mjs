#!/usr/bin/env node
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]),sha=b=>createHash('sha256').update(b).digest('hex');
assert.equal(sha(binary),'bbe93063f7a0879a1021b2891e5c9354e5b3b98433e32efe6750f7710afed750');
const sourceByteRanges=[];
function take(first,last,start,label){const a=binary.indexOf(first,start),z=binary.indexOf(last,a+first.length);assert(a>=start&&z>a,label);const b=binary.subarray(a,z);sourceByteRanges.push({label,start:a,end:z,sha256:sha(b)});return b.toString('utf8');}
const decision=take('function uWo(','var fWo=2;',188952000,'actual stream recovery decisions and counters');
const context=vm.createContext({});vm.runInContext(decision,context);
const causes=['denied','overloaded','serverError','timedOut','stalled','connectionLost','truncated','malformed','badRequest','unknown'],progresses=['output','thinkingOnly','partialOutput','started','nothing'],cases=[];
for(const cause of causes)for(const progress of progresses)for(let bits=0;bits<64;bits++)for(const maxRetries of [0,3])for(const counter of [0,2]){
 const failure={cause,progress,stopReasonReceived:!!(bits&1),outlastedNonStreamingTimeout:!!(bits&2)},policy={maxRetries,maxOverloaded:3,hasFallbackModel:!!(bits&4),persistent:!!(bits&8),background:!!(bits&16)},counts={retries:counter,overloaded:counter,stalls:counter,truncations:counter,afterThinkingOnly:counter,triedWithoutStreaming:counter!==0},allowNonStreaming=!!(bits&32);
 context.F=failure;context.P=policy;context.C=counts;context.ALLOW=allowNonStreaming;
 const expected=JSON.parse(JSON.stringify(vm.runInContext('uWo(F,C,P,ALLOW)',context)));
 cases.push({failure,policy,counts,allowNonStreaming,expected});
}
const notice=take('let G3e=cs({','yield G3e;break e}',189079000,'actual partial notice and truncation producer');
const notices=[];
for(let bits=0;bits<128;bits++){const input={Ji:!!(bits&1),UR:!!(bits&2),Wg:!!(bits&4),Tc:!!(bits&8),Bu:!!(bits&16),mf:!!(bits&32),xSe:!!(bits&64)};const ctx=vm.createContext({...input,Ua:'API Error',cs:x=>x});const expected=JSON.parse(JSON.stringify(vm.runInContext(notice+';G3e',ctx)));notices.push({input,expected});}
const output={version:'2.1.288',native_sha256:sha(binary),sourceByteRanges,notices,scope:'Actual partial notice/truncation producer and uWo/dFe/oH/nH/Xle stream decision functions. Error/progress classification, source admission, max API attempt guard, backoff, bookkeeping and physical execution excluded.',cases};
const text=JSON.stringify(output,null,2)+'\n',path=new URL('../../crates/llm-runtime/tests/fixtures/stream_recovery_2_1_288.json',import.meta.url);
if(process.argv.includes('--check'))assert.equal(readFileSync(path,'utf8'),text);else writeFileSync(path,text);
console.log(`${process.argv.includes('--check')?'checked':'wrote'} ${cases.length} actual stream recovery cases`);
