#!/usr/bin/env node
// Execute native h7t and real Read/readFor enqueue statements from fixed bytes.
import assert from 'node:assert/strict';
import {createHash} from 'node:crypto';
import {readFileSync,writeFileSync} from 'node:fs';
import {fileURLToPath} from 'node:url';
import vm from 'node:vm';
const binary=readFileSync(process.argv[2]);
const digest=bytes=>createHash('sha256').update(bytes).digest('hex');
const sha=digest(binary);
assert.equal(sha,'6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea');
const sourceByteRanges=[];
function take(startMarker,endMarker,label){
 const start=binary.indexOf(startMarker,185762130),end=binary.indexOf(endMarker,start+startMarker.length);
 assert(start>=185762130&&end>start,label);
 const bytes=binary.subarray(start,end);sourceByteRanges.push({label,start,end,sha256:digest(bytes)});
 return bytes.toString('utf8');
}
const consumer=take('async function h7t(e,n){','function T1e(e,n,r)', 'h7t consumer');
const producers=[
 ['readFor',take('let qe=g.nestedMemoryAttachmentTriggers;if(qe','let Qe=g.remoteCall','readFor enqueue')],
 ['notebook',take('let zn=z.nestedMemoryAttachmentTriggers;if(zn','let xn={type:"notebook"','notebook enqueue')],
 ['image',take('$n=z.nestedMemoryAttachmentTriggers;if($n','CS({operation:"read",tool:"FileReadTool",filePath:r,content:Pn.file.base64})','image enqueue')],
 ['text',take('let jt=z.nestedMemoryAttachmentTriggers;if(jt','let Xt,un,rn','text enqueue')],
];
const cases=[];
for(const agent of [false,true])for(const disabled of [false,true])for(const triggers of [[],['b','a'],['a','a']]){
 const input={agent,disabled,triggers,pending:['a','c','a'],appendDuring:null,throwOn:null};
 cases.push({input});
}
for(const variant of [
 {appendDuring:'late',throwOn:null},
 {appendDuring:null,throwOn:'b'},
 {appendDuring:'late',throwOn:'late'},
])cases.push({input:{agent:false,disabled:false,triggers:['a','b'],pending:['a','c'],...variant}});
for(const row of cases){
 row.expected=JSON.parse(JSON.stringify(await vm.runInNewContext(`(async()=>{
 const cfg=INPUT, e={nestedMemoryAttachmentTriggers:[...cfg.triggers],pendingNestedMemoryTriggers:[...cfg.pending],consumedNestedMemoryTriggers:[],...(cfg.agent?{agentId:'child'}:{})};
 const a={CLAUDE_CODE_DISABLE_CLAUDE_MDS:cfg.disabled},pe=()=>({}),calls=[],O7t=async()=>({});
 let appended=false;
 const D7t=async path=>{calls.push(path);if(cfg.appendDuring&&!appended){appended=true;e.nestedMemoryAttachmentTriggers.push(cfg.appendDuring);}if(path===cfg.throwOn)throw Error('native fixture failure');return[{path}];};
 ${consumer}
 let attachments=null,failed=false;try{attachments=await h7t(e,[]);}catch{failed=true;}
 return{queued:e.nestedMemoryAttachmentTriggers,pending:e.pendingNestedMemoryTriggers,consumed:e.consumedNestedMemoryTriggers,calls,attachments,failed};
 })()`,{INPUT:row.input})));
}
const producerCases=producers.map(([name,source])=>{
 // Image snippet starts with an assignment inside a native let declaration.
 const code=name==='image'?'let '+source:source;
 const expected=vm.runInNewContext(`(()=>{const queue=['existing'],g={nestedMemoryAttachmentTriggers:queue},z=g,Pe='new',r='new';function run(){${code}}run();run();return queue;})()`);
 assert.deepEqual(JSON.parse(JSON.stringify(expected)),['existing','new']);
 return{name,expected:JSON.parse(JSON.stringify(expected))};
});
const output={version:'2.1.287',native_sha256:sha,sourceByteRanges,
 scope:'Exact initialized h7t arrays, root/agent pending transfer, disable, live iteration and failure retention; exact four Read/readFor enqueue snippets. Missing-context arrays, D7t filesystem semantics, worker attachment pipeline and coordinator/rewind producers are separate.',cases,producerCases};
const file=fileURLToPath(new URL('../../crates/tool-api/tests/fixtures/nested_trigger_2_1_287.json',import.meta.url));
const json=JSON.stringify(output,null,2)+'\n';
if(process.argv.includes('--check'))assert.equal(readFileSync(file,'utf8'),json);else writeFileSync(file,json);
process.stdout.write(`${process.argv.includes('--check')?'checked':'wrote'} native trigger oracle: ${cases.length} consumers and ${producerCases.length} producer snippets\n`);
