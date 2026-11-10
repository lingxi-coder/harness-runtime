#!/usr/bin/env node
// Exact native SDK transport/manifest helpers; execution has no network or credentials.
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import vm from 'node:vm';
const binary = readFileSync(process.argv[2]);
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const nativeSha256 = 'a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450';
assert.equal(sha(binary), nativeSha256);
const ranges = [
  {
    "name": "sdk_transport",
    "start": 290049561,
    "end": 290052709,
    "sha256": "42a3317abbc27229b3bc3ded844adb6f6f2c3e7d6649c89ab262d82c730e00c7"
  },
  {
    "name": "manifest_parser",
    "start": 290052976,
    "end": 290053502,
    "sha256": "c12bb81a5a4f6c1dc91401b18058423e1d3d162d1a40a748e1083fa209fe21b7"
  },
  {
    "name": "native_send",
    "start": 301173575,
    "end": 301173851,
    "sha256": "421c74c013480240e9ff37d2909838e9965ad84955013ee0515ebdd23a83972a"
  }
];
const sources = Object.fromEntries(ranges.map(range => {
 const bytes = binary.subarray(range.start, range.end);
 assert.equal(sha(bytes), range.sha256, range.name);
 return [range.name, bytes.toString('utf8')];
}));
let timers = [];
const native = vm.runInNewContext(
 sources.sdk_transport + '\n' + sources.manifest_parser + '\nclass IO {' + sources.native_send + '}\n({Transport:PTn,parse:Oni,isRequest:Kdr,emptyCapabilities:pPt,IO,swallow:n});',
 {D: value => value !== null && typeof value === 'object' && !Array.isArray(value), queueMicrotask,
  ye:70000,Re:10000,u:value=>value,Cle:()=>({optional:()=>({})}),
  rs:(_signal,options)=>{timers.push(options);return {signal:'bounded',cleanup(){}};}},
 {timeout:1000},
);
const clone = value => JSON.parse(JSON.stringify(value));
const initializeResult = {protocolVersion:'2025-11-25',capabilities:{tools:{}},serverInfo:{name:'fixture',version:'1'}};
const initialize = {jsonrpc:'2.0',id:0,method:'initialize',params:{protocolVersion:'2025-11-25',capabilities:{}}};
const initialized = {jsonrpc:'2.0',method:'notifications/initialized'};
const list = {jsonrpc:'2.0',id:1,method:'tools/list'};
const baseManifest = {initializeResult,toolsListResult:{tools:[]}};
const replayInputs = [
 {name:'ordinary',manifest:baseManifest,messages:[initialize,initialized,list,list]},
 ...[{}, {roots:{listChanged:true},elicitation:{}},{elicitation:{}},null,[],{extra:true}].map(capabilities=>({
  name:'caps-'+JSON.stringify(capabilities),manifest:baseManifest,
  messages:[{...initialize,params:{...initialize.params,capabilities}},initialized,list]})),
 ...[null,false,{}, {progressToken:null},{progressToken:4},{progressToken:'x'},
  {'io.modelcontextprotocol/related-task':{taskId:'x'}},{'io.modelcontextprotocol/related-task':{taskId:4}}].map(meta=>({
   name:'meta-'+JSON.stringify(meta),manifest:{...baseManifest,initializeResult:{...initializeResult,_meta:meta}},messages:[initialize,initialized,list]})),
 {name:'protocol-mismatch',manifest:baseManifest,messages:[{...initialize,params:{...initialize.params,protocolVersion:'other'}},initialized,list]},
 {name:'list-before-initialized',manifest:baseManifest,messages:[initialize,list,initialized,list]},
 {name:'cursor-declared-null',manifest:baseManifest,messages:[initialize,initialized,{...list,params:{cursor:null}},list]},
 {name:'notification-before-initialize',manifest:baseManifest,messages:[initialized,initialize,list]},
 {name:'no-manifest',messages:[initialize,initialized,list]},
];
const replayCases = replayInputs.map(input=>{
 const transport = new native.Transport('fixture',async()=>undefined,input.manifest);
 const outputs = input.messages.map(message=>{
  const value = transport.answerFromManifest(message);
  return value===native.swallow?{swallow:true}:value===undefined?{forward:true}:{reply:clone(value)};
 });
 return {input,outputs,outcome:transport.manifestOutcome};
});
const parserCases = [undefined,null,false,[],{}, {fixture:null}, {fixture:{initializeResult}},
 {fixture:{initializeResult,toolsListResult:null}}, {fixture:{initializeResult,toolsListResult:[]}},
 {fixture:baseManifest,ignored:{initializeResult:false}}, {fixture:{initializeResult,toolsListResult:{tools:[]}},'__proto__':null}
].map(input=>{
 const parsed=native.parse(input,['fixture']);
 return {input,manifests:Array.from(parsed.manifests.entries()).map(([key,value])=>[key,clone(value)]),
  malformedField:parsed.malformedField,malformedServerNames:clone(parsed.malformedServerNames)};
});
const deadlineCases=[];
const utf16MetaCases = ['{"progressToken":"\\ud800"}', '{"io.modelcontextprotocol/related-task":{"taskId":"\\udfff"}}'].map(metaJson => {
 const transport=new native.Transport('fixture',async()=>undefined,{initializeResult:{...initializeResult,_meta:JSON.parse(metaJson)}});
 const result=transport.answerFromManifest(initialize);
 return {metaJson,accepted:result!==undefined && result!==native.swallow};
});
for (const message of [initialize,initialized,list,{jsonrpc:'2.0',id:4,result:{}},{jsonrpc:'2.0',id:4,error:{code:-32601,message:'unknown'}}]) {
 timers=[];let request,signal,options;
 const io=new native.IO();io.sendRequest=async(r,_schema,s,o)=>{request=r;signal=s;options=o;return {mcp_response:{jsonrpc:'2.0',id:0,result:{}}};};
 await io.sendMcpMessage('fixture',message);
 deadlineCases.push({message,request:clone(request),bounded:signal!==undefined,timers:clone(timers),options:clone(options)});
}
// A pending request does not prevent a response from being sent in the other direction.
const entered=[];const pending=[];
const transport=new native.Transport('fixture',(_server,message)=>{entered.push(clone(message));return new Promise(resolve=>pending.push(resolve));});
const sends=[transport.send(list),transport.send({jsonrpc:'2.0',id:'peer',result:{}})];
assert.equal(entered.length,2);
for (const resolve of pending)resolve(undefined);
await Promise.all(sends);
let closeCount=0;transport.onclose=()=>closeCount++;
await transport.close();await transport.close();
let closedMessage;try{await transport.send(list);}catch(error){closedMessage=error.message;}
const fixture={version:'2.1.322',nativeSha256,ranges,scope:'Pure SDK transport manifest replay/parser, control-request deadline classification and concurrency/close. Host object classification and timer facilities are explicit seams; real control loopback receipts are separate.',
 initializeCapabilities:clone(native.emptyCapabilities),replayCases,parserCases,deadlineCases,utf16MetaCases,
 concurrency:{entered,closeCount,closedMessage}};
const output=new URL('../../crates/runtime/tests/fixtures/mcp_sdk_2_1_322.json',import.meta.url);
const rendered=JSON.stringify(fixture,null,2)+'\n';
if(process.argv.includes('--write'))writeFileSync(output,rendered);else assert.equal(readFileSync(output,'utf8'),rendered);
console.log(`SDK MCP2.1.322: ${replayCases.length} manifest, ${parserCases.length} parser, ${deadlineCases.length} deadlines, ${utf16MetaCases.length} UTF-16 metadata; concurrent send and close match`);
